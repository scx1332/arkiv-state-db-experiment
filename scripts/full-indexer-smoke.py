#!/usr/bin/env python3
"""Unchanged released indexer + SDK against a disposable prefilled native chain.

Requires Docker Compose, Bun, installed sibling SDK, and the indexer's
compose.signed-chain.yml. Uses only disposable fixture keys and volumes.
"""
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
INDEXER = Path(os.environ.get('ARKIV_INDEXER_DIR', ROOT.parent / 'arkiv-chain-indexer-experimental'))
SDK = Path(os.environ.get('ARKIV_SDK_DIR', ROOT.parent / 'arkiv-sdk-js'))
BINARY = Path(os.environ.get('ARKIV_PROPOSER_BIN', ROOT / 'target/release/arkiv-proposer'))


def main():
    spec = importlib.util.spec_from_file_location('fixture', ROOT / 'scripts/genesis-bundle-smoke.py')
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    reserved = set()

    def port():
        while True:
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                value = sock.getsockname()[1]
            if value not in reserved:
                reserved.add(value)
                return value

    def request(url, body=None, token=None):
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        req = urllib.request.Request(url, None if body is None else json.dumps(body).encode(), headers)
        with urllib.request.urlopen(req, timeout=10) as response:
            return json.load(response)

    def wait(check, timeout=90):
        end = time.monotonic() + timeout
        last = None
        while time.monotonic() < end:
            try:
                result = check()
                if result:
                    return result
            except (OSError, ValueError, KeyError, TypeError) as error:
                last = error
            time.sleep(.25)
        raise AssertionError(f'Timed out: {last}')

    with tempfile.TemporaryDirectory(prefix='arkiv-full-indexer-') as directory:
        work = Path(directory)
        producer_port, private_port = port(), port()
        rpc = f'http://127.0.0.1:{producer_port}/'
        token = secrets.token_hex(32)
        env = dict(os.environ, POSTGRES_PASSWORD=secrets.token_hex(24),
                   ARKIV_PROPOSER_TOKEN=token, CHAIN_RPC_URL=rpc, CHAIN_WRITE_RPC_URL=rpc)
        for name in ('PG', 'REDIS', 'DECODER', 'API', 'UI'):
            env[f'INDEXER_{name}_PORT'] = str(port())
        api = 'http://127.0.0.1:' + env['INDEXER_API_PORT']
        compose = ['docker', 'compose', '-p', 'arkiv-indexer-smoke-' + secrets.token_hex(5),
                   '-f', str(INDEXER / 'compose.signed-chain.yml')]

        def dc(*args):
            return subprocess.run(compose + list(args), env=env, check=True, capture_output=True, text=True, timeout=120)

        source = fixture.FIXTURE.replace('block_period_ms = "5000"', 'block_period_ms = "500"')
        for old, new in ((4, 10000), (5, 10001), (6, 10002)):
            source = source.replace(f'expires_at_height = "{old}"', f'expires_at_height = "{new}"')
        (work / 'chain.toml').write_text(source)
        (work / 'proposer.key').write_text('0x' + '01' * 32)
        (work / 'proposer.key').chmod(0o600)
        modules = work / 'node_modules'
        (modules / '@arkiv-network').mkdir(parents=True)
        (modules / '@arkiv-network/sdk').symlink_to(SDK, target_is_directory=True)
        (modules / 'viem').symlink_to(SDK / 'node_modules/viem', target_is_directory=True)
        shutil.copyfile(ROOT / 'scripts/sdk-compat-smoke.ts', work / 'smoke.ts')
        for args in [('genesis', 'lock', work / 'chain.toml'),
                     ('genesis', 'build', work / 'chain.toml', '--out', work / 'bundle', '--locked')]:
            subprocess.run([str(BINARY), *map(str, args)], env=env, check=True, capture_output=True)
        node_log = (work / 'node.log').open('w')
        node = subprocess.Popen([str(BINARY), 'producer', '--run-dir', str(work / 'producer'),
                                 '--key-file', str(work / 'proposer.key'), '--bundle', str(work / 'bundle'),
                                 '--listen', f'127.0.0.1:{producer_port}', '--private-listen',
                                 f'127.0.0.1:{private_port}', '--start', 'running'],
                                env=env, stdout=node_log, stderr=node_log)
        try:
            wait(lambda: request(rpc + 'signed/v1/status'))
            dc('up', '-d')
            wait(lambda: request(api + '/health')['ok'])
            sdk_env = dict(env, SDK_RPC_URL=rpc, SDK_REPORT_FILE=str(work / 'report.json'),
                           SDK_INDEXER_FILE=str(INDEXER / 'src/rpc.ts'))
            subprocess.run(['bun', str(work / 'smoke.ts'), 'exercise'], cwd=work, env=sdk_env, check=True, timeout=240)
            request(f'http://127.0.0.1:{private_port}/signed/v1/control', {'action':'pause'}, token)
            head = int(request(rpc + 'signed/v1/status')['head']['height'])
            wait(lambda: int(request(api + '/health')['features']['entityQueryIndex']['projectedThroughBlock']) == head)
            health = request(api + '/health')
            genesis = health['features']['entityQueryIndex']['genesis']
            assert genesis and genesis['status'] == 'done', genesis
            print('PASS standard backend imported genesis:', json.dumps(genesis), flush=True)
            def rpc_call(url, method, params):
                answer = request(url, {'jsonrpc':'2.0','id':1,'method':method,'params':params})
                assert 'error' not in answer, answer
                return answer['result']
            options = {'atBlock':hex(head),'limit':200,'select':{'key':True,'owner':True,'creator':True,'attributeSchema':True,'attributes':True,'createdAt':True,'updatedAt':True,'expiresAt':True,'creationFlags':True}}
            expected = rpc_call(rpc, 'arkiv_query', ['*', options])['data']
            actual = rpc_call(api + '/shadow-rpc/experimental', 'arkiv_query', ['*', options])['data']
            ordered = lambda rows: sorted(rows, key=lambda row: row['key'])
            assert ordered(actual) == ordered(expected), (actual, expected)
            report = json.loads((work / 'report.json').read_text())
            tx = request(api + '/transaction/' + report['txHash'])
            assert tx, tx
            entity = request(api + '/entity/' + report['survivingKey'])
            assert entity, entity
            sql = "SELECT json_build_object('blocks', (SELECT count(*) FROM blocks), 'transactions', (SELECT count(*) FROM transactions), 'operations', (SELECT count(*) FROM transaction_operations), 'logs', (SELECT count(*) FROM transaction_logs), 'balances', (SELECT count(*) FROM account_balances));"
            counts = json.loads(dc('exec','-T','postgres','psql','-U','indexer','-d','indexer','-Atc',sql).stdout)
            assert counts['blocks'] == head + 1, counts
            assert all(counts[key] > 0 for key in ('transactions','operations','logs','balances')), counts
            print('PASS unchanged scanner/decoder/PostgreSQL/entity projector:', json.dumps(counts), flush=True)
            print('PASS all live entity metadata matches node, including prefills and SDK lifecycle', flush=True)
            aggregate_sql = "SELECT (SELECT count(*) FROM block_ranges) > 0 AND (SELECT count(*) FROM sender_stats) > 0;"
            wait(lambda: dc('exec','-T','postgres','psql','-U','indexer','-d','indexer','-Atc',aggregate_sql).stdout.strip() == 't')
            wait(lambda: request(api + '/statistics')['chain']['id'] == '9405')
            ui = 'http://127.0.0.1:' + env['INDEXER_UI_PORT']
            assert request(ui + '/api/health')['features']['transactionData']
            with urllib.request.urlopen(ui, timeout=10) as response:
                assert b'Arkiv BlockExplorer' in response.read()
            print('PASS range/sender aggregators, statistics and standard frontend API proxy', flush=True)
            dc('restart', 'backend', 'scanner')
            wait(lambda: request(api + '/health')['ok'])
            actual = rpc_call(api + '/shadow-rpc/experimental', 'arkiv_query', ['*', options])['data']
            assert ordered(actual) == ordered(expected)
            print('PASS standard indexer restart retains complete projected state', flush=True)
        except Exception:
            print(dc('logs','--tail','20','scanner','backend','decoder').stdout[-12000:])
            raise
        finally:
            try:
                dc('down', '-v', '--remove-orphans')  # only this randomly named disposable test project
            finally:
                node.terminate()
                node.wait(timeout=15)
                node_log.close()


if __name__ == '__main__':
    main()
