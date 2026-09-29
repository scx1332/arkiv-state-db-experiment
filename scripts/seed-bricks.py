#!/usr/bin/env python3
"""Seed 16 typed, indexed brick entities in an existing named namespace."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import time
import urllib.error

spec = importlib.util.spec_from_file_location('namespace', Path(__file__).with_name('create-sample-namespace.py'))
namespace = importlib.util.module_from_spec(spec)
spec.loader.exec_module(namespace)
raw_request = namespace.request


def request(base, path, body=None):
    # Read-only requests can briefly contend with full-node replay after a
    # restart. Retry them without ever resubmitting the signed transaction.
    for attempt in range(10):
        try:
            return raw_request(base, path, body)
        except urllib.error.HTTPError as error:
            if error.code not in (429, 503) or attempt == 9:
                raise
            time.sleep(2)


namespace.request = request

# Repeated colors and sizes make equality posting sets interesting to explore.
BRICKS = [
    ('ruby-classic', 'red', '2 x 4', 2, 4, 3, 'brick'),
    ('ruby-square', 'red', '2 x 2', 2, 2, 3, 'brick'),
    ('ruby-plate', 'red', '2 x 4', 2, 4, 1, 'plate'),
    ('ocean-classic', 'blue', '2 x 4', 2, 4, 3, 'brick'),
    ('ocean-square', 'blue', '2 x 2', 2, 2, 3, 'brick'),
    ('ocean-beam', 'blue', '1 x 6', 1, 6, 3, 'brick'),
    ('sunshine-classic', 'yellow', '2 x 4', 2, 4, 3, 'brick'),
    ('sunshine-square', 'yellow', '2 x 2', 2, 2, 3, 'brick'),
    ('sunshine-tile', 'yellow', '1 x 2', 1, 2, 1, 'tile'),
    ('forest-classic', 'green', '2 x 4', 2, 4, 3, 'brick'),
    ('forest-plate', 'green', '2 x 2', 2, 2, 1, 'plate'),
    ('snow-classic', 'white', '2 x 4', 2, 4, 3, 'brick'),
    ('snow-tile', 'white', '1 x 2', 1, 2, 1, 'tile'),
    ('midnight-beam', 'black', '1 x 6', 1, 6, 3, 'brick'),
    ('tangerine-square', 'orange', '2 x 2', 2, 2, 3, 'brick'),
    ('violet-plate', 'purple', '2 x 4', 2, 4, 1, 'plate'),
]


def blob(value):
    return struct.pack('>I', len(value)) + value


def attribute(name, value):
    if isinstance(value, int):
        encoded = b'\x03' + struct.pack('>Q', value)
    elif isinstance(value, bytes):
        encoded = b'\x05' + blob(value)
    else:
        encoded = b'\x04' + blob(value.encode())
    return blob(name.encode()) + encoded


def operation(namespace_id, index, brick, lifetime):
    sku, color, size, width, length, height, shape = brick
    payload = json.dumps({'name': sku.replace('-', ' ').title(), 'description': f'{color.title()} {size} toy {shape}', 'units': {'width': 'studs', 'length': 'studs', 'height': 'plates'}}, separators=(',', ':')).encode()
    attrs = {'sample': 'bricks-v1', 'sku': sku, 'color': color, 'size': size,
             'width': width, 'length': length, 'height': height, 'shape': shape,
             'material': 'ABS', '$contentType': 'application/json', '$payload': payload}
    return (b'\x08' + struct.pack('>Q', namespace_id) + b'\x01'
            + (index + 1).to_bytes(16, 'big') + b'\x00'
            + struct.pack('>QQI', 0, lifetime, len(attrs))
            + b''.join(attribute(k, v) for k, v in attrs.items()))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', default='target/release/arkiv-proposer')
    p.add_argument('--peer', required=True)
    p.add_argument('--full', required=True)
    p.add_argument('--key-file', required=True)
    p.add_argument('--genesis-file', required=True)
    p.add_argument('--actor', required=True)
    p.add_argument('--name', default='bricks')
    p.add_argument('--lifetime-blocks', type=int, default=1_296_000, help='30 days at two-second blocks')
    args = p.parse_args()
    catalog = namespace.find_namespace(args.full, args.name)
    if not catalog:
        raise ValueError('Create the named namespace first')
    status = request(args.peer, 'status')
    full_status = request(args.full, 'status')
    if any(status[k] != full_status[k] for k in ('chainId', 'genesisHash', 'runId')):
        raise ValueError('Producer and full node identities differ')
    ns = int(catalog['namespace'])
    actor = bytes.fromhex(args.actor.removeprefix('0x'))
    if len(actor) != 20 or args.lifetime_blocks <= 0:
        raise ValueError('Invalid actor or lifetime')
    # Same chain + namespace + signer is one sample installation. Retrying
    # checks the durable request before submitting, including after timeouts.
    rid = hashlib.sha256(f"bricks-v1:{status['genesisHash']}:{ns}:{args.actor.lower()}".encode()).hexdigest()
    path = 'requests/' + args.actor + '/0x' + rid
    prior = request(args.peer, path)
    if prior['status'] == 'unknown':
        nonce = int(request(args.peer, 'accounts/' + args.actor + '/nonce')['pendingNextNonce'])
        envelope = (struct.pack('>Q', int(status['chainId'])) + actor + bytes.fromhex(rid)
                    + struct.pack('>QI', 90_000_000, len(BRICKS))
                    + b''.join(operation(ns, i, brick, args.lifetime_blocks) for i, brick in enumerate(BRICKS)))
        with tempfile.TemporaryDirectory(prefix='arkiv-bricks-') as temporary:
            native, signed = Path(temporary) / 'native.tx', Path(temporary) / 'signed.tx'
            native.write_bytes(envelope)
            subprocess.run([args.binary, 'sign', '--key-file', args.key_file, '--genesis-file', args.genesis_file, '--tx-file', str(native), '--nonce', str(nonce), '--out', str(signed)], check=True)
            subprocess.run([args.binary, 'submit', '--peer', args.peer, '--transaction-file', str(signed)], check=True)
    for _ in range(120):
        receipt = request(args.full, path)
        if receipt['status'] == 'included':
            if receipt['executionStatus'] != 'committed':
                raise RuntimeError('Sample transaction failed: ' + json.dumps(receipt))
            print(json.dumps({'namespace': args.name, 'namespaceId': ns, 'count': len(BRICKS), 'lifetimeBlocks': args.lifetime_blocks, 'receipt': receipt}))
            return
        time.sleep(1)
    raise RuntimeError('Not observed yet; rerun safely to inspect the same request')


if __name__ == '__main__':
    main()
