#!/usr/bin/env python3
"""Run a fresh complete-source prefill, then keep the verified producer alive.

The genesis state database is built once: stream-build keeps its verified
private state (--state-out) and the producer installs it (--prebuilt-state)
instead of rebuilding it from the stream. --rebuild-genesis-in-producer restores
the older independent producer rebuild.

Intended for a detached container with memory and swap limits. No retries or
resume: each invocation needs a new output directory. Status and logs survive
container exit when the output directory is bind-mounted.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def metrics(pid):
    result = {}
    proc = Path('/proc') / str(pid)
    try:
        fields = dict(line.split(':', 1) for line in (proc / 'status').read_text().splitlines())
        for source, target in [('VmRSS', 'rssKiB'), ('VmHWM', 'peakRssKiB')]:
            result[target] = int(fields.get(source, '0 kB').split()[0])
        result['io'] = {k: int(v) for k, v in
                        (line.split(':', 1) for line in (proc / 'io').read_text().splitlines())}
        # Container cgroup namespaces normally expose the current group at root.
        relative = next(line.split('::', 1)[1] for line in
                        (proc / 'cgroup').read_text().splitlines() if line.startswith('0::'))
        group = Path('/sys/fs/cgroup') / relative.lstrip('/')
        if not (group / 'memory.current').is_file():
            group = Path('/sys/fs/cgroup')
        for field in ('memory.current', 'memory.peak', 'memory.max',
                      'memory.swap.current', 'memory.swap.max'):
            value = (group / field).read_text().strip()
            result[field] = int(value) if value != 'max' else value
        for field in ('memory.events', 'memory.stat'):
            result[field] = {k: int(v) for k, v in
                             (line.split() for line in (group / field).read_text().splitlines())}
    except (OSError, ValueError, StopIteration):
        pass
    return result


REBUILD_STAGES = ('bulk_normalize', 'bulk_indexes')


def verify_ready(status, expected, producer_log=None):
    """Raise unless the producer serves exactly the bundle's height-zero genesis.

    With producer_log (build-once mode), also require that the fresh producer
    installed the prebuilt state exactly once and never rebuilt it.
    """
    head = status['head']
    if (head['height'] != '0' or head['hash'] != expected['genesisHash']
            or head['stateRoot'] != expected['stateRoot']):
        raise RuntimeError('Producer readiness state does not match the genesis bundle')
    if producer_log is not None:
        installs, rebuilt = [], False
        with open(producer_log, errors='replace') as log:
            for line in log:
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(event, dict):
                    continue
                if event.get('event') == 'prefill_prebuilt_install':
                    installs.append(event.get('result'))
                elif (event.get('event') == 'prefill_bulk_phase'
                      or (event.get('event') == 'prefill_progress' and event.get('stage') in REBUILD_STAGES)):
                    rebuilt = True
        if rebuilt or installs not in (['moved'], ['copied']):
            raise RuntimeError('Producer did not start from the prebuilt genesis state')
    return status


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Runner:
    def __init__(self, args):
        self.args = args
        self.child = None
        self.signal = None
        self.started = time.monotonic()
        self.status = {'state': 'starting', 'startedAt': utc_now(), 'phases': []}
        self.args.out.mkdir(parents=True, exist_ok=False)

    def save(self):
        self.status.update(updatedAt=utc_now(), elapsedSeconds=round(time.monotonic() - self.started, 3))
        temporary = self.args.out / 'status.json.tmp'
        temporary.write_text(json.dumps(self.status, indent=2) + '\n')
        temporary.replace(self.args.out / 'status.json')

    def stop(self, signum, _frame):
        self.signal = signum
        if self.child is not None and self.child.poll() is None:
            os.killpg(self.child.pid, signum)

    def check_stop(self):
        if self.signal is not None:
            raise InterruptedError(f'received signal {self.signal}')

    def phase(self, name, command, env=None, ready=None):
        self.check_stop()
        phase = {'name': name, 'startedAt': utc_now(), 'state': 'running'}
        self.status['phases'].append(phase)
        self.status.update(state='running', phase=name)
        self.save()
        started = time.monotonic()
        with (self.args.out / f'{name}.log').open('w') as log, \
                (self.args.out / f'{name}-samples.jsonl').open('w') as samples:
            self.child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                          env=env, start_new_session=True)
            phase['pid'] = self.child.pid
            while True:
                self.check_stop()
                sample = dict(at=utc_now(), elapsedSeconds=round(time.monotonic() - started, 3),
                              **metrics(self.child.pid))
                samples.write(json.dumps(sample) + '\n')
                samples.flush()
                phase['metrics'] = sample
                code = self.child.poll()
                if code is not None:
                    phase.update(exitCode=code, finishedAt=utc_now(),
                                 elapsedSeconds=round(time.monotonic() - started, 3),
                                 state='completed' if code == 0 and ready is None else 'failed')
                    self.save()
                    if code != 0 or ready is not None:
                        raise RuntimeError(f'{name} exited with code {code}; see {name}.log')
                    self.child = None
                    return
                if ready is not None and 'readyAt' not in phase:
                    status = ready()
                    if status is not None:
                        phase.update(readyAt=utc_now(), readySeconds=round(time.monotonic() - started, 3),
                                     verifiedStatus=status)
                        self.status['state'] = 'ready'
                        print('Producer ready; remaining alive without automatic blocks.', flush=True)
                self.save()
                time.sleep(5)

    def run(self):
        a = self.args
        converted, bundle = a.out / 'converted', a.out / 'bundle'
        # Same filesystem as the bundle and node directories: both handoffs are renames.
        prebuilt = None if a.rebuild_genesis_in_producer else a.out / 'genesis-state'
        self.status['genesisBuild'] = 'producer-rebuild' if prebuilt is None else 'once'
        # The default policy keeps the original converter command line.
        expiry = [] if a.expiry_policy == 'dependency-safe' else ['--expiry-policy', a.expiry_policy]
        if a.max_expiry is not None:
            expiry += ['--max-expiry', str(a.max_expiry)]
        self.phase('convert', [sys.executable, str(a.converter), 'convert', '--bundle',
                               str(a.source), '--out', str(converted), '--reserve-gib', str(a.reserve_gib),
                               *expiry])
        handoff = [] if prebuilt is None else ['--state-out', str(prebuilt)]
        self.phase('stream-build', [str(a.bin), 'genesis', 'stream-build', str(a.base),
                                    '--prefill', str(converted / 'prefill.bin'), '--out', str(bundle),
                                    '--sponsor', a.sponsor, *handoff])
        expected = json.loads((bundle / 'report.json').read_text())
        permanent = a.expiry_policy == 'unlimited'
        if permanent:
            # Permanent rows pay no genesis rent; the node must agree with the
            # converter's accounting and have committed the permanent launch.
            conversion = json.loads((converted / 'conversion-report.json').read_text())
            if (expected.get('permanentPrefill') is not True
                    or expected['streamHash'] != conversion['streamHash']
                    or expected['burnedWei'] != conversion['rentWei']
                    or expected['storageUnits'] != conversion['storageUnits']):
                raise RuntimeError('Permanent bundle report does not match the conversion report')
        public, private = free_port(), free_port()
        while private == public:
            private = free_port()
        self.status['producerListen'] = f'127.0.0.1:{public}'
        env = dict(os.environ, ARKIV_PROPOSER_TOKEN=secrets.token_hex(32))

        def ready():
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:{public}/signed/v1/status', timeout=2) as response:
                    status = json.load(response)
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                return None
            status = verify_ready(status, expected, None if prebuilt is None else self.args.out / 'producer.log')
            if permanent and status['capabilities']['gas'].get('permanentEntitiesSupported') is not True:
                raise RuntimeError('Producer does not report permanent genesis entities')
            return status

        handoff = [] if prebuilt is None else ['--prebuilt-state', str(prebuilt)]
        self.phase('producer', [str(a.bin), 'producer', '--run-dir', str(a.out / 'node'),
                                '--bundle', str(bundle), '--key-file', str(a.key),
                                '--listen', f'127.0.0.1:{public}',
                                '--private-listen', f'127.0.0.1:{private}', *handoff],
                   env=env, ready=ready)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('bin', 'converter', 'source', 'base', 'key', 'out'):
        parser.add_argument('--' + name, type=lambda value: Path(value).resolve(), required=True)
    parser.add_argument('--sponsor', required=True)
    parser.add_argument('--reserve-gib', type=int, default=80)
    # far-future needs a base genesis whose max_lifetime is at least --max-expiry;
    # unlimited needs a base genesis built with [prefill] allow_permanent = true.
    parser.add_argument('--expiry-policy', choices=('dependency-safe', 'far-future', 'unlimited'),
                        default='dependency-safe')
    parser.add_argument('--max-expiry', type=int)
    parser.add_argument('--rebuild-genesis-in-producer', action='store_true',
                        help='do not hand the built state to the producer; it rebuilds it from the stream')
    args = parser.parse_args()
    if args.expiry_policy == 'unlimited' and args.max_expiry is not None:
        parser.error('--max-expiry does not apply to --expiry-policy unlimited')
    if args.reserve_gib < 0:
        parser.error('--reserve-gib must be nonnegative')
    os.environ['ARKIV_PREFILL_RESERVE_GIB'] = str(args.reserve_gib)
    runner = Runner(args)
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, runner.stop)
    try:
        runner.run()
    except BaseException as error:
        runner.status.update(state='stopped' if runner.signal else 'failed',
                             error=f'{type(error).__name__}: {error}')
        child = runner.child
        if child is not None:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
            runner.status['phases'][-1].update(exitCode=child.returncode, finishedAt=utc_now(),
                                               state=runner.status['state'])
        runner.save()
        print(runner.status['error'], file=sys.stderr, flush=True)
        return 128 + runner.signal if runner.signal else 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
