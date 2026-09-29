#!/usr/bin/env python3
"""Create an empty named namespace using a funded local signer (no chain reset)."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import time
import urllib.request


def request(base, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base.rstrip('/') + '/signed/v1/' + path,
                                 data=data, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=30) as response:
        return json.load(response)


def find_namespace(full, name):
    height = request(full, 'status')['head']['height']
    pending = [[]]
    while pending:
        result = request(full, 'state/explore', {'height': height, 'map': 'catalog', 'path': pending.pop()})
        node = result['node']
        if node is None:
            continue
        decoded = node.get('decoded') or {}
        if decoded.get('name') == name:
            return {'height': height, **decoded}
        for child in node['children']:
            if child.get('kind') != 'empty':
                pending.append(child['path'])
    return None


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', default='target/debug/arkiv-proposer')
    p.add_argument('--peer', required=True)
    p.add_argument('--full', required=True)
    p.add_argument('--key-file', required=True)
    p.add_argument('--genesis-file', required=True)
    p.add_argument('--actor', required=True, help='public signer address')
    p.add_argument('--name', default='bricks')
    args = p.parse_args()
    existing = find_namespace(args.full, args.name)
    if existing:
        print(json.dumps({'existing': True, **existing}))
        return
    status = request(args.peer, 'status')
    full_status = request(args.full, 'status')
    if any(status[field] != full_status[field] for field in ('chainId', 'genesisHash', 'runId')):
        raise ValueError('Producer and full node identities differ')
    nonce_state = request(args.peer, 'accounts/' + args.actor + '/nonce')
    nonce = int(nonce_state['pendingNextNonce'])
    actor = bytes.fromhex(args.actor.removeprefix('0x'))
    if len(actor) != 20:
        raise ValueError('actor must be a 20-byte public address')
    name = args.name.encode()
    # Stable per-chain/name/actor request ID permits safe retries after timeouts.
    request_id = hashlib.sha256((status['genesisHash'] + args.name + args.actor.lower()).encode()).digest()
    previous = request(args.peer, 'requests/' + args.actor + '/0x' + request_id.hex())
    if previous.get('status') not in (None, 'unknown'):
        raise RuntimeError('Request already submitted; inspect its status before retrying: ' + json.dumps(previous))
    envelope = (struct.pack('>Q', int(status['chainId'])) + actor + request_id
                + struct.pack('>QI', 1_000_000, 1) + b'\x01' + struct.pack('>I', len(name)) + name)
    with tempfile.TemporaryDirectory(prefix='arkiv-namespace-') as temporary:
        native = Path(temporary) / 'native.tx'
        signed = Path(temporary) / 'signed.tx'
        native.write_bytes(envelope)
        subprocess.run([args.binary, 'sign', '--key-file', args.key_file, '--genesis-file', args.genesis_file,
                        '--tx-file', str(native), '--nonce', str(nonce), '--out', str(signed)], check=True)
        subprocess.run([args.binary, 'submit', '--peer', args.peer, '--transaction-file', str(signed)], check=True)
    for _ in range(60):
        found = find_namespace(args.full, args.name)
        if found:
            print(json.dumps({'created': True, 'requestId': '0x' + request_id.hex(), **found}))
            return
        time.sleep(1)
    raise RuntimeError('Namespace not observed yet; inspect request status before retrying')


if __name__ == '__main__':
    main()
