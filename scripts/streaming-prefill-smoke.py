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
profile = { preset = "arkiv-entity-v3/glm-v2@3" }
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
    parser.add_argument('--prefill', type=Path)
    parser.add_argument('--bundle', type=Path, help='reuse an independently built fixture bundle')
    parser.add_argument('--timeout', type=float, default=600, help='per-phase readiness/CLI deadline in seconds')
    parser.add_argument('--expected-entities', type=int, help='required canonical prefill entity count')
    contract_proof = parser.add_mutually_exclusive_group()
    contract_proof.add_argument('--expected-contracts', type=int, help='required authenticated verified_contract posting count')
    contract_proof.add_argument('--expect-contract-proof-limit', action='store_true',
        help='require broad contract equality proof to return ProofLimitExceeded; no verified total claimed')
    parser.add_argument('--resume', action='store_true',
        help='reuse an incomplete height-zero fixture run before any live transaction was prepared; requires --bundle')
    parser.add_argument('--query', default="kind = str('verified_contract')")
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--bin', type=Path, default=ROOT / 'target/debug/arkiv-proposer')
    args = parser.parse_args()
    if args.prefill is None and args.bundle is None:
        parser.error('--prefill or --bundle is required')
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    if any(value is not None and value < 0 for value in (args.expected_entities, args.expected_contracts)):
        parser.error('expected counts must be nonnegative')
    if args.prefill is None:
        args.prefill = args.bundle / 'prefill.bin'
    work = args.work_dir.resolve()
    if args.resume and args.bundle is None:
        parser.error('--resume requires the original --bundle')
    key = work / 'fixture.key'
    if not args.resume:
        work.mkdir(parents=True, exist_ok=False)
        (work / 'chain.toml').write_text(BASE)
        key.write_text('0x' + '01' * 32)
        key.chmod(0o600)
    env = dict(os.environ, ARKIV_PROPOSER_TOKEN=secrets.token_hex(32))
    processes, handles, endpoints, commands = {}, [], {}, {}
    for role in ('producer', 'private', 'full', 'light'):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            endpoints[role] = f'http://127.0.0.1:{sock.getsockname()[1]}'

    def read_bounded(path, cap=1024 * 1024):
        with path.open('rb') as stream:
            data = stream.read(cap + 1)
        if len(data) > cap:
            raise RuntimeError(f'Output exceeds {cap}-byte bound: {path.name}')
        return data

    def tail(path, cap=3000):
        with path.open('rb') as stream:
            stream.seek(0, os.SEEK_END)
            stream.seek(max(0, stream.tell() - cap))
            return stream.read(cap).decode('utf-8', errors='replace')

    if args.resume:
        if not work.is_dir() or (work / 'acceptance.json').exists():
            raise RuntimeError('Resume requires an existing incomplete acceptance directory')
        if any((work / name).exists() for name in ('live.txt', 'live.tx')):
            raise RuntimeError('Resume only supports failure before live transaction preparation')
        if read_bounded(key, 128).decode('utf-8') != '0x' + '01' * 32:
            raise RuntimeError('Resume fixture key mismatch')
        if read_bounded(work / 'chain.toml', 65536).decode('utf-8') != BASE:
            raise RuntimeError('Resume fixture chain description mismatch')
        expected_genesis = read_bounded(args.bundle / 'genesis.bin', 65536)
        for role, database in (('producer', 'node.redb'), ('full', 'node.redb'), ('light', 'headers.redb')):
            if not (work / role / database).is_file():
                raise RuntimeError(f'Resume requires the completed {role} installation')
            if read_bounded(work / role / 'genesis.bin', 65536) != expected_genesis:
                raise RuntimeError(f'Resume {role} belongs to another bundle')
        for role in ('producer', 'full'):
            if not (work / role / 'prefill.bin').is_file():
                raise RuntimeError(f'Resume requires retained {role} prefill')

    cli_count = 0
    def cli(*values):
        nonlocal cli_count
        while True:
            cli_count += 1
            stdout = work / f'cli-{cli_count:02}.stdout'
            stderr = work / f'cli-{cli_count:02}.stderr'
            if not stdout.exists() and not stderr.exists():
                break
        with stdout.open('wb') as out, stderr.open('wb') as error:
            result = subprocess.run([str(args.bin), *map(str, values)], env=env,
                                    stdout=out, stderr=error, timeout=args.timeout)
        if result.returncode:
            raise RuntimeError(f'CLI failed: {tail(stderr)}')
        return read_bounded(stdout).decode('utf-8')

    def request(role, route, body=None):
        headers = {'Content-Type': 'application/json'}
        if role == 'private':
            headers['Authorization'] = 'Bearer ' + env['ARKIV_PROPOSER_TOKEN']
        req = urllib.request.Request(endpoints[role] + '/signed/v1/' + route,
                                     data=None if body is None else json.dumps(body).encode(), headers=headers)
        for attempt in range(40):
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    payload = response.read(8 * 1024 * 1024 + 1)
                    if len(payload) > 8 * 1024 * 1024:
                        raise RuntimeError('RPC response exceeded 8 MiB')
                    result = json.loads(payload)
                if result.get('error') == 'ServerBusy':
                    time.sleep(.1)
                    continue
                return result
            except urllib.error.HTTPError as error:
                if error.code != 429 or attempt == 39:
                    raise RuntimeError(f'{role}/{route}: {error.code} {error.read(1000)!r}') from error
                time.sleep(.1)
        raise RuntimeError('Server remained busy')

    def wait(predicate):
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            for role, process in processes.items():
                if process.poll() is not None:
                    raise RuntimeError(f"{role} exited: {tail(work / (role + '.log'))}")
            try:
                result = predicate()
                if result:
                    return result
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.1)
        raise RuntimeError('Readiness/sync timed out')

    def start(role):
        print(json.dumps({'stage': 'start', 'role': role}), flush=True)
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
        bundle = args.bundle.resolve() if args.bundle else work / 'bundle'
        if args.bundle is None:
            cli('genesis', 'lock', work / 'chain.toml')
            cli('genesis', 'build', work / 'chain.toml', '--out', work / 'base', '--locked')
            cli('genesis', 'stream-build', work / 'base/genesis.bin', '--prefill', args.prefill.resolve(),
                '--out', bundle, '--sponsor', ADDRESS)
            cli('genesis', 'stream-verify', bundle)
        commands['producer'] = ['producer', '--run-dir', work / 'producer', '--key-file', key,
            '--bundle', bundle, '--listen', endpoints['producer'][7:],
            '--private-listen', endpoints['private'][7:]]
        initial = start('producer')
        identity = initial['identity']
        bundle_report = json.loads(read_bounded(bundle / 'report.json'))
        assert bundle_report['genesisHash'] == identity['genesisHash'], 'report/live genesis identity mismatch'
        live_genesis = request('producer', 'genesis')
        assert bytes.fromhex(live_genesis['genesisBytes'].removeprefix('0x')) == read_bounded(bundle / 'genesis.bin', 65536)
        with (bundle / 'prefill.bin').open('rb') as stream:
            framing = stream.read(20)
        assert len(framing) == 20 and framing[:12] == b'ARKPFL02' + (3).to_bytes(4, 'big')
        entity_count = int.from_bytes(framing[12:], 'big')
        assert entity_count == int(bundle_report['rowCount']), 'report/canonical row count mismatch'
        assert (bundle / 'prefill.bin').stat().st_size == int(bundle_report['canonicalBytes'])
        if args.expected_entities is not None:
            assert entity_count == args.expected_entities, (entity_count, args.expected_entities)
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
        assert genesis_proof['nextNonce'] == '0', 'acceptance requires an unused fixture account'
        def entity_query(text, limit=64):
            result = request('light', 'query/text', {'query': text, 'requireProof': True,
                'height': '0', 'namespace': '1', 'limit': limit, 'cursor': None})
            assert result.get('verificationStatus', {}).get('status') == 'verified', result
            return result

        query = entity_query(args.query)
        assert query.get('entities'), query
        # The posting count is authenticated by this one-row equality proof;
        # the remaining contracts are not fetched or retained by the harness.
        def check_contract_proof():
            if args.expect_contract_proof_limit:
                result = request('light', 'query/text', {'query': "kind = str('verified_contract')",
                    'requireProof': True, 'height': '0', 'namespace': '1', 'limit': 1, 'cursor': None})
                assert result.get('error') == 'ProofLimitExceeded', result
                assert result.get('verificationStatus', {}).get('status') == 'resource_limit', result
                return None
            result = entity_query("kind = str('verified_contract')", limit=1)
            count = int(result['postingCount'])
            if args.expected_contracts is not None:
                assert count == args.expected_contracts, (count, args.expected_contracts)
            return count
        total_contracts = check_contract_proof()
        payload = work / 'live.txt'
        payload.write_text('live write after streaming genesis')
        rid = '0x' + secrets.token_hex(32)
        tx = work / 'live.tx'
        cli('create', '--key-file', key, '--genesis-file', bundle / 'genesis.bin',
            '--nonce', '0', '--request-id', rid, '--data-file', payload, '--out', tx,
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
        restarted_query = entity_query(args.query)
        assert restarted_query['entities'] == query['entities'], 'genesis entity results changed after restart'
        assert restarted_query['postingCount'] == query['postingCount']
        assert check_contract_proof() == total_contracts
        report = {'result': 'passed', 'genesis': bundle_report,
            'head': head, 'canonicalEntityCount': entity_count,
            'sampledEntityProofCount': len(query['entities']), 'authenticatedVerifiedContractCount': total_contracts,
            'expectedEntities': args.expected_entities, 'expectedContracts': args.expected_contracts,
            'broadContractProof': 'resource_limit' if args.expect_contract_proof_limit else 'verified',
            'resumed': args.resume,
            'proofQuery': args.query, 'fullDownloadedPrefill': True,
            'lightHasNoPrefill': True, 'verifiedAccountProof': True, 'verifiedEntityProof': True,
            'liveWrite': True, 'restart': True, 'verifiedEntityProofAfterRestart': True}
        (work / 'acceptance.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
    finally:
        stop()
        for handle in handles:
            handle.close()

if __name__ == '__main__':
    main()
