#!/usr/bin/env python3
"""Isolated v2 prefill producer/full/light acceptance. Uses disposable test keys.

The full follower downloads its input; the light follower receives no dataset.
All processes are stopped on exit. --work-dir retains evidence for inspection.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import time
import urllib.request
import urllib.error

ROOT = Path(__file__).resolve().parents[1]
ADDRESS = '0x1a642f0e3c3af545e7acbd38b07251b3990914f1'
BASE = '''schema = "arkiv-genesis-bundle-v1"
protocol = "arkiv-launch-prefill-v1"
name = "Streaming acceptance"
profile = { preset = "arkiv-entity-v2/glm-v2@1" }
[chain]
id = "7706815"
run_id = "0x00000000000000000000000077068150"
start_time = "2026-10-19T00:00:00Z"
block_period_ms = "2000"
proposer = "@proposer"
[allocations]
expected_initial_supply_wei = "1000000000000000000000000000000"
[[allocations.entry]]
label = "proposer"
address = "ADDRESS"
balance_wei = "1000000000000000000000000000000"
[prefill]
builder = "arkiv-prefill-v1"
namespace = "entities"
rent_policy = "sponsored-rent-v1"
sponsor = "@proposer"
[prefill.limits]
max_entities = "256"
max_canonical_bytes = "1048576"
max_storage_units = "8388608"
'''.replace('ADDRESS', ADDRESS)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--prefill', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--bin', type=Path, default=ROOT / 'target/debug/arkiv-proposer')
    args = parser.parse_args()
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    (work / 'chain.toml').write_text(BASE)
    key = work / 'fixture.key'
    key.write_text('0x' + '01' * 32)
    key.chmod(0o600)
    env = dict(os.environ, ARKIV_PROPOSER_TOKEN=secrets.token_hex(32))
    processes, handles, endpoints, commands = {}, [], {}, {}
    for role in ('producer', 'private', 'full', 'light'):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            endpoints[role] = f'http://127.0.0.1:{sock.getsockname()[1]}'

    def cli(*values):
        result = subprocess.run([str(args.bin), *map(str, values)], env=env,
                                capture_output=True, text=True, timeout=600)
        if result.returncode:
            raise RuntimeError(f'CLI failed: {result.stderr}')
        return result.stdout

    def request(role, route, body=None):
        headers = {'Content-Type': 'application/json'}
        if role == 'private':
            headers['Authorization'] = 'Bearer ' + env['ARKIV_PROPOSER_TOKEN']
        req = urllib.request.Request(endpoints[role] + '/signed/v1/' + route,
                                     data=None if body is None else json.dumps(body).encode(), headers=headers)
        for attempt in range(40):
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    result = json.load(response)
                if result.get('error') == 'ServerBusy':
                    time.sleep(.1)
                    continue
                return result
            except urllib.error.HTTPError as error:
                if error.code != 429 or attempt == 39:
                    raise RuntimeError(f'{role}/{route}: {error.code} {error.read()[:1000]!r}') from error
                time.sleep(.1)
        raise RuntimeError('Server remained busy')

    def wait(predicate):
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            for role, process in processes.items():
                if process.poll() is not None:
                    raise RuntimeError(f'{role} exited: {(work / (role + ".log")).read_text()[-3000:]}')
            try:
                result = predicate()
                if result:
                    return result
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.1)
        raise RuntimeError('Readiness/sync timed out')

    def start(role):
        log = (work / (role + '.log')).open('a')
        handles.append(log)
        processes[role] = subprocess.Popen([str(args.bin), *map(str, commands[role])], env=env, stdout=log, stderr=log)
        return wait(lambda: request(role, 'status'))

    def stop():
        for process in processes.values():
            process.terminate()
        for process in processes.values():
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        processes.clear()

    def sync():
        head = request('producer', 'status')['head']
        for role in ('full', 'light'):
            wait(lambda role=role: request(role, 'status').get('head') == head)
        return head

    def proof(height):
        result = request('light', 'query/account', {'actor': ADDRESS, 'height': str(height)})
        assert result.get('verificationStatus', {}).get('status') == 'verified', result
        return result

    try:
        cli('genesis', 'lock', work / 'chain.toml')
        cli('genesis', 'build', work / 'chain.toml', '--out', work / 'base', '--locked')
        cli('genesis', 'stream-build', work / 'base/genesis.bin', '--prefill', args.prefill.resolve(),
            '--out', work / 'bundle', '--sponsor', ADDRESS)
        cli('genesis', 'stream-verify', work / 'bundle')
        commands['producer'] = ['producer', '--run-dir', work / 'producer', '--key-file', key,
            '--bundle', work / 'bundle', '--listen', endpoints['producer'][7:],
            '--private-listen', endpoints['private'][7:]]
        initial = start('producer')
        identity = initial['identity']
        for role in ('full', 'light'):
            commands[role] = [role, '--run-dir', work / role, '--peer', endpoints['producer'],
                '--listen', endpoints[role][7:], '--source-id', identity['sourceId'],
                '--run-id', identity['runId'], '--chain-id', identity['chainId'],
                '--genesis-hash', identity['genesisHash'], '--proposer', ADDRESS]
            if role == 'full':
                commands[role] += ['--replication-peer', endpoints['private']]
            start(role)
        assert sync()['height'] == '0'
        def file_digest(path):
            with path.open('rb') as stream:
                return hashlib.file_digest(stream, 'sha256').digest()
        assert file_digest(work / 'full/prefill.bin') == file_digest(args.prefill)
        assert not (work / 'light/prefill.bin').exists()
        genesis_proof = proof(0)
        query = request('light', 'query/text', {"query": "kind = str('verified_contract')",
            'requireProof': True, 'height': '0', 'namespace': '1', 'limit': 64, 'cursor': None})
        assert query.get('verificationStatus', {}).get('status') == 'verified', query
        assert query.get('entities'), query
        payload = work / 'live.txt'
        payload.write_text('live write after streaming genesis')
        rid = '0x' + secrets.token_hex(32)
        tx = work / 'live.tx'
        cli('create', '--key-file', key, '--genesis-file', work / 'bundle/genesis.bin',
            '--nonce', '0', '--request-id', rid, '--payload-file', payload, '--out', tx,
            '--gas-limit', '1000000', '--lifetime-blocks', '1000')
        admitted = request('producer', 'transactions', {'transactionBytes': '0x' + tx.read_bytes().hex()})
        assert admitted.get('status') == 'pending', admitted
        request('private', 'control', {'action': 'step'})
        head = sync()
        receipt = request('producer', f'requests/{ADDRESS}/{rid}')
        assert receipt.get('executionStatus') == 'committed', receipt
        proof(1)
        stop()
        for role in ('producer', 'full', 'light'):
            start(role)
        assert sync() == head
        assert proof(0) == genesis_proof
        assert request('producer', f'requests/{ADDRESS}/{rid}') == receipt
        report = {'result': 'passed', 'genesis': json.loads((work / 'bundle/report.json').read_text()),
            'head': head, 'queriedContracts': len(query['entities']), 'fullDownloadedPrefill': True,
            'lightHasNoPrefill': True, 'verifiedAccountProof': True, 'verifiedEntityProof': True,
            'liveWrite': True, 'restart': True}
        (work / 'acceptance.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
    finally:
        stop()
        for handle in handles:
            handle.close()

if __name__ == '__main__':
    main()
