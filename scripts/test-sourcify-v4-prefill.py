#!/usr/bin/env python3
"""Tests of scripts/sourcify-v4-prefill.py (v4 Sourcify bundle -> row codec 3).

A synthetic v4 bundle, written here independently of the converter, covers every
kind and reference (chunked and inline values, both code slots, a literal
on-chain code hash, proxy attributes); an independent decoder reads prefill.bin
back. Tampered copies of the bundle, with checksums and report totals rewritten
so that each mutation reaches its check, must be refused.

Optional, with real data:
  SOURCIFY_V4_BUNDLE=DIR     convert a processor bundle (process --format udc) and
                             compare every row with its line
  ARKIV_PROPOSER_BIN=FILE    build a permanent-prefill base genesis and run
                             `genesis stream-build` on the synthetic stream
"""

import base64
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "sourcify-v4-prefill.py"
spec = importlib.util.spec_from_file_location("sourcify_v4_prefill", SCRIPT)
conv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(conv)

CHAIN = 7706815
OWNER = bytes.fromhex("1111111111111111111111111111111111111111")
PREFIX = "sourcify.v4"
MAX = (1 << 64) - 1


def keccak(data):
    return hashlib.new("KECCAK-256", data).digest()


def key_of(chain, owner, nonce):
    return keccak(chain.to_bytes(32, "big") + bytes.fromhex("44" + "00" * 18 + "44") + owner
                  + nonce.to_bytes(8, "big") + bytes(16))


def cjson(value):
    def canonical(v):
        if isinstance(v, dict):
            return {k: canonical(v[k]) for k in sorted(v, key=lambda s: s.encode("utf-16-be"))}
        if isinstance(v, list):
            return [canonical(x) for x in v]
        return v
    return json.dumps(canonical(value), ensure_ascii=False, separators=(",", ":")).encode()


def deflate(raw):
    z = zlib.compressobj(9, zlib.DEFLATED, -15)
    body = z.compress(raw) + z.flush()
    return b"\x01" + body if len(body) < len(raw) else b"\x00" + raw


class SyntheticBundle:
    """A minimal v4 writer: entities in creation order, dependencies first."""

    def __init__(self, chain=CHAIN, owner=OWNER, prefix=PREFIX):
        self.chain, self.owner, self.prefix = chain, owner, prefix
        self.lines = []

    def add(self, kind, attrs):
        nonce = len(self.lines)
        key = key_of(self.chain, self.owner, nonce)
        attrs = [("arkiv.schema", "str", True, f"{self.prefix}.{kind}")] + attrs
        attrs.sort(key=lambda a: a[0].encode())
        self.lines.append({"nonce": nonce, "key": "0x" + key.hex(), "expiresAt": "0xffffffffffffffff",
                           "attributes": [{"name": n, "type": t, "indexed": i, "value": v} for n, t, i, v in attrs]})
        return key

    @staticmethod
    def b(data):
        return base64.b64encode(data).decode()

    def chunked(self, encoded):
        keys = [self.add("chunk", [("data", "bytes", False, self.b(encoded[i:i + 120000]))])
                for i in range(0, len(encoded), 120000)]
        return b"\x02" + hashlib.sha256(encoded).digest() + struct.pack(">I", len(encoded)) + b"".join(keys)

    def source(self, text, chunk=False):
        raw = text.encode()
        value = b"\x00" + raw
        if chunk:
            value = self.chunked(value)
        key = self.add("source", [("content", "bytes", False, self.b(value)),
                                  ("hash", "bytes32", True, "0x" + hashlib.sha256(raw).hexdigest())])
        return key, hashlib.sha256(raw).digest()

    def code(self, bytecode):
        key = self.add("code", [("code", "bytes", False, self.b(deflate(bytecode))),
                                ("hash", "bytes32", True, "0x" + keccak(bytecode).hex())])
        return key, keccak(bytecode)

    def compilation(self, sources, codes, abi):
        groups = {"abi": abi, "compilation": {"name": "C", "language": "Solidity", "compilerVersion": "0.8.0"},
                  "artifacts": {"metadata": "{}", "sources": {}}}
        group_bytes = {g: cjson(v) for g, v in groups.items()}
        refs = [{"kind": "source", "path": f"/sources/s{i}.sol/content", "source": i} for i in range(len(sources))]
        refs += [{"kind": "code", "path": f"/{slot}Bytecode/recompiledBytecode", "code": slot,
                  "encoding": {"prefix": True, "uppercase": []}} for slot in codes]
        manifest = cjson({"codes": {s: "0x" + h.hex() for s, (k, h) in codes.items()},
                          "digests": {g: "0x" + hashlib.sha256(b).hexdigest() for g, b in group_bytes.items()},
                          "refs": refs, "sources": ["0x" + h.hex() for k, h in sources]})
        attrs = [("manifest", "bytes", False, self.b(deflate(manifest))),
                 ("hash", "bytes32", True, "0x" + hashlib.sha256(manifest).hexdigest())]
        if sources:
            attrs.append(("sourcerefs", "bytes", False, self.b(b"\x00" + b"".join(k for k, h in sources))))
        attrs += [(g, "bytes", False, self.b(deflate(b))) for g, b in group_bytes.items()]
        attrs += [(f"{slot}coderef", "key", False, "0x" + k.hex()) for slot, (k, h) in codes.items()]
        key = self.add("compilation", attrs)
        return key, hashlib.sha256(manifest).digest(), hashlib.sha256(group_bytes["abi"]).digest()

    def contract(self, address, matchid, comp, codes=None, literal=None, proxy=False):
        ckey, chash, abihash = comp
        data = {"address": address, "chainId": "1", "matchId": str(matchid)}
        attrs = [("address", "addr", True, address.lower()), ("chainid", "u64", True, "0x1"),
                 ("matchid", "u64", True, hex(matchid)), ("verifiedat", "u64", True, "0x6543210f"),
                 ("match", "str", True, "exact_match"), ("name", "str", True, "C"),
                 ("compilerversion", "str", True, "0.8.0"), ("language", "str", True, "Solidity"),
                 ("evmversion", "str", True, "default"), ("optimizer", "bool", True, True),
                 ("optimizerruns", "i32", True, -1), ("viair", "bool", True, False),
                 ("deployer", "addr", True, "0x" + "00" * 20), ("blocknumber", "u64", True, "0x0"),
                 ("abihash", "bytes32", True, "0x" + abihash.hex()), ("compilationhash", "bytes32", True, "0x" + chash.hex()),
                 ("compilationref", "key", False, "0x" + ckey.hex())]
        for slot, (k, h) in (codes or {}).items():
            attrs += [(f"{slot}coderef", "key", False, "0x" + k.hex()), (f"{slot}codehash", "bytes32", True, "0x" + h.hex())]
        if literal is not None:
            data["creationBytecode"] = {"onchainBytecode": "0x" + literal.hex()}
            attrs.append(("creationcodehash", "bytes32", True, "0x" + keccak(literal).hex()))
        if proxy:
            attrs += [("isproxy", "bool", True, True), ("proxytype", "str", True, "EIP1967Proxy")]
        attrs.append(("data", "bytes", False, self.b(deflate(cjson(data)))))
        return self.add("contract", attrs)

    def write(self, root, part_size=4, source_complete=True, mutate=None):
        lines = copy.deepcopy(self.lines)
        if mutate:
            mutate(lines)
        write_bundle(root, lines, self.chain, self.owner, self.prefix, part_size, source_complete)


def b64len(value):
    try:
        return len(base64.b64decode(value, validate=True))
    except ValueError:
        return 0


def kind_of(line):
    return next(a["value"] for a in line["attributes"] if a["name"] == "arkiv.schema").rsplit(".", 1)[-1]


def units(line):
    total = 1460
    for a in line["attributes"]:
        t, v = a["type"], a["value"]
        size = {"bool": 1, "i32": 4, "u64": 8, "addr": 20, "bytes32": 32, "key": 32}.get(t)
        if size is None:
            size = b64len(v) if t == "bytes" else len(v.encode())
        total += len(a["name"]) + size + 2 + (128 if a["indexed"] else 0)
    return total


def write_bundle(root, lines, chain, owner, prefix, part_size=4, source_complete=True):
    root.mkdir(parents=True)
    (root / "entities").mkdir()
    parts, kinds = [], {}
    for start in range(0, len(lines), part_size):
        chunk = lines[start:start + part_size]
        data = b"".join(json.dumps(l, separators=(",", ":")).encode() + b"\n" for l in chunk)
        path = f"entities/{len(parts) + 1:06d}.jsonl"
        (root / path).write_bytes(data)
        parts.append({"path": path, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
                      "entities": len(chunk), "firstNonce": start})
    for l in lines:
        tag = next((a["value"] for a in l["attributes"] if a["name"] == "arkiv.schema"), f"{prefix}.contract")
        kind = tag.rsplit(".", 1)[-1]
        kinds[kind] = kinds.get(kind, 0) + 1
    bytes_values = sum(b64len(a["value"]) for l in lines for a in l["attributes"]
                       if a["type"] == "bytes" and isinstance(a["value"], str))
    report = {"schema": "sourcify-udc-import.v1", "status": "completed", "format": "sourcify-arkiv.udc-bundle.v1",
              "sourceChainId": 1, "arkivChainId": chain, "owner": "0x" + owner.hex(), "schemaPrefix": prefix,
              "records": kinds.get("contract", 0), "contracts": kinds.get("contract", 0), "entities": len(lines),
              "kinds": dict(sorted(kinds.items())),
              "distinct": {"sources": kinds.get("source", 0), "codes": kinds.get("code", 0),
                           "compilations": kinds.get("compilation", 0)},
              "bytesValues": bytes_values, "storageUnits": sum(units(l) for l in lines),
              "expiry": "permanent (expiresAt 2^64-1)", "parts": parts, "sourceCompleteRequired": source_complete}
    seed = {"version": 2, "format": "sourcify-arkiv.udc-bundle.v1", "chainId": chain, "owner": "0x" + owner.hex(),
            "schemaPrefix": prefix, "firstNonce": 0, "entities": [{"path": p["path"]} for p in parts]}
    for name, doc in (("report.json", report), ("seed.json", seed)):
        (root / name).write_text(json.dumps(doc, indent=2) + "\n")
    sums = "".join(f"{hashlib.sha256((root / p).read_bytes()).hexdigest()}  {p}\n"
                   for p in [p["path"] for p in parts] + ["report.json", "seed.json"])
    (root / "SHA256SUMS").write_text(sums)


def decode_stream(data):
    """An independent reader of ARKPFL02 || u32(3) || u64 rows || frames."""
    assert data[:8] == b"ARKPFL02" and struct.unpack(">I", data[8:12])[0] == 3
    rows = struct.unpack(">Q", data[12:20])[0]
    at, out = 20, []
    tags = {1: "bool", 2: "str", 3: "bytes", 4: "addr", 11: "bytes32", 13: "u64", 16: "i32", 64: "key"}
    for _ in range(rows):
        length = struct.unpack(">I", data[at:at + 4])[0]
        frame = data[at + 4:at + 4 + length]
        at += 4 + length
        creator, nonce = frame[:20], struct.unpack(">Q", frame[20:28])[0]
        salt, expiry, flags = int.from_bytes(frame[28:44], "big"), struct.unpack(">Q", frame[44:52])[0], frame[52]
        count, p, attrs = struct.unpack(">I", frame[53:57])[0], 57, []
        for _ in range(count):
            n = struct.unpack(">I", frame[p:p + 4])[0]
            name = frame[p + 4:p + 4 + n].decode()
            kind, tag = frame[p + 4 + n], frame[p + 5 + n]
            p += 6 + n
            t = tags[tag]
            if t in ("str", "bytes"):
                m = struct.unpack(">I", frame[p:p + 4])[0]
                value = frame[p + 4:p + 4 + m]
                p += 4 + m
                value = value.decode() if t == "str" else value
            else:
                width = {"bool": 1, "i32": 4, "u64": 8, "addr": 20, "bytes32": 32, "key": 32}[t]
                value = frame[p:p + width]
                p += width
                if t == "bool":
                    value = bool(value[0])
                elif t == "i32":
                    value = int.from_bytes(value, "big") - (1 << 31)
                elif t == "u64":
                    value = int.from_bytes(value, "big")
            attrs.append((name, kind, t, value))
        assert p == len(frame), "frame has trailing bytes"
        out.append({"creator": creator, "nonce": nonce, "salt": salt, "expiry": expiry, "flags": flags, "attributes": attrs})
    assert at == len(data), "stream has trailing bytes"
    return out


def line_cells(line):
    cells = []
    for a in line["attributes"]:
        t, v = a["type"], a["value"]
        if t == "bytes":
            v = base64.b64decode(v)
        elif t in ("addr", "bytes32", "key"):
            v = bytes.fromhex(v[2:])
        elif t == "u64":
            v = int(v, 16)
        cells.append((a["name"], 1 if a["indexed"] else 2, t, v))
    return cells


def build(bundle=None):
    s = bundle or SyntheticBundle()
    big = "".join(f"// line {i} {hashlib.sha256(str(i).encode()).hexdigest()}\n" for i in range(3000))
    s1 = s.source("contract A {}\n")
    s2 = s.source(big, chunk=True)
    creation = s.code(bytes.fromhex("6080604052") * 30)
    runtime = s.code(bytes.fromhex("60806040") * 25)
    comp = s.compilation([s1, s2], {"creation": creation, "runtime": runtime}, [{"type": "function", "name": "f"}])
    s.contract("0x" + "ab" * 20, 7, comp, codes={"creation": creation, "runtime": runtime})
    s.contract("0x" + "cd" * 20, 9, comp, literal=bytes.fromhex("600160020300"), proxy=True)
    lone = s.compilation([], {}, [])
    s.contract("0x" + "ef" * 20, 11, lone)
    return s


def run(args, check=True):
    done = subprocess.run([sys.executable, "-I", str(SCRIPT)] + [str(a) for a in args], capture_output=True, text=True)
    if check and done.returncode != 0:
        raise AssertionError(done.stderr)
    return done


class V4PrefillTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="v4-prefill-test-"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def convert(self, bundle, out="out", extra=()):
        done = run(["convert", "--bundle", bundle, "--out", self.tmp / out, "--workers", "2", "--reserve-gib", "0", *extra])
        return json.loads(done.stdout), (self.tmp / out / "prefill.bin").read_bytes()

    def refused(self, mutate, message, extra=(), **write):
        s = build()
        root = self.tmp / f"bad-{len(list(self.tmp.iterdir()))}"
        s.write(root, mutate=mutate, **write)
        done = run(["convert", "--bundle", root, "--out", self.tmp / (root.name + "-out"), "--workers", "2",
                    "--reserve-gib", "0", *extra], check=False)
        self.assertNotEqual(done.returncode, 0, f"accepted; expected: {message}")
        self.assertIn(message, done.stderr)
        self.assertFalse((self.tmp / (root.name + "-out")).exists())

    def test_keccak_and_safeproxy_key(self):
        self.assertEqual(conv.keccak().hexdigest(), "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        # docs/udc-examples/safeproxy in sourcify-arkiv-entity-processor: owner 0x11..11, nonce 0.
        self.assertEqual(conv.entity_key(CHAIN, OWNER, 0).hex(),
                         "a8a795295a5174e67c1a8a39b5a3c478f6d1f3f5b3979eca9ff8105e407b9b60")

    def test_cells_follow_the_pinned_codec(self):
        # docs/genesis-bundle-v1.md: arkiv.schema = "posts", indexed.
        name, kind, indexed, value, payload = conv.parse_attribute(
            {"name": "arkiv.schema", "type": "str", "indexed": True, "value": "posts"})
        cell = conv.sized(name.encode()) + bytes([1, conv.TAGS[kind]]) + payload
        self.assertEqual(cell.hex(), "0000000c61726b69762e736368656d61" + "0102" + "00000005706f737473")
        for attr, tag_payload in (({"type": "i32", "value": -1}, "107fffffff"), ({"type": "i32", "value": 0}, "1080000000"),
                                  ({"type": "u64", "value": "0x100"}, "0d0000000000000100"),
                                  ({"type": "bool", "value": True}, "0101"),
                                  ({"type": "addr", "value": "0x" + "ab" * 20}, "04" + "ab" * 20),
                                  ({"type": "bytes32", "value": "0x" + "01" * 32}, "0b" + "01" * 32),
                                  ({"type": "key", "value": "0x" + "02" * 32}, "40" + "02" * 32)):
            _, t, _, _, p = conv.parse_attribute({"name": "x", "indexed": True, **attr})
            self.assertEqual((bytes([conv.TAGS[t]]) + p).hex(), tag_payload)
        _, t, _, _, p = conv.parse_attribute({"name": "x", "type": "bytes", "indexed": False, "value": "AAEC"})
        self.assertEqual((bytes([conv.TAGS[t]]) + p).hex(), "0300000003000102")

    def test_synthetic_bundle_converts_line_for_line(self):
        s = build()
        s.write(self.tmp / "bundle")
        report, data = self.convert(self.tmp / "bundle")
        rows = decode_stream(data)
        self.assertEqual(len(rows), len(s.lines))
        for row, line in zip(rows, s.lines):
            self.assertEqual((row["creator"], row["nonce"], row["salt"], row["expiry"], row["flags"]),
                             (OWNER, line["nonce"], 0, MAX, 0))
            self.assertEqual(row["attributes"], line_cells(line))
        stream = hashlib.new("KECCAK-256", bytes([23]) + b"arkiv/prefill-stream/v2" + data).hexdigest()
        self.assertEqual(report["streamHash"], "0x" + stream)
        self.assertEqual(report["canonicalBytes"], len(data))
        self.assertEqual(report["prefillSha256"], hashlib.sha256(data).hexdigest())
        self.assertEqual(report["entitiesByKind"], {"chunk": 2, "code": 2, "compilation": 2, "contract": 3, "source": 2})
        self.assertEqual(int(report["storageUnits"]), sum(units(l) for l in s.lines))
        self.assertEqual(report["references"], {"chunk": 2, "code": 4, "compilation": 3, "source": 2, "total": 11})
        self.assertEqual(report["chunkListsJoined"], 1)
        self.assertFalse(report["rekeyed"])
        lock = json.loads((self.tmp / "out" / "source-lock.json").read_text())
        self.assertEqual([f["path"] for f in lock["files"][:3]], ["SHA256SUMS", "seed.json", "report.json"])
        self.assertEqual(sorted(p.name for p in (self.tmp / "out").iterdir()),
                         ["conversion-report.json", "prefill.bin", "source-lock.json"])
        # Deterministic across worker counts.
        again = run(["convert", "--bundle", self.tmp / "bundle", "--out", self.tmp / "again", "--workers", "1",
                     "--reserve-gib", "0"])
        self.assertEqual((self.tmp / "again" / "prefill.bin").read_bytes(), data)
        self.assertEqual(json.loads(again.stdout)["streamHash"], report["streamHash"])
        # inventory checks the same and publishes no stream.
        inv = json.loads(run(["inventory", "--bundle", self.tmp / "bundle", "--out", self.tmp / "inv",
                              "--reserve-gib", "0"]).stdout)
        self.assertNotIn("streamHash", inv)
        self.assertFalse((self.tmp / "inv" / "prefill.bin").exists())

    def test_rekey_rewrites_every_key(self):
        s = build()
        s.write(self.tmp / "bundle")
        owner, chain = bytes.fromhex("22" * 20), 9001
        report, data = self.convert(self.tmp / "bundle", extra=("--owner", "0x" + owner.hex(), "--chain-id", chain))
        self.assertTrue(report["rekeyed"])
        self.assertEqual(report["firstKey"], "0x" + key_of(chain, owner, 0).hex())
        old = {key_of(CHAIN, OWNER, n): n for n in range(len(s.lines))}
        new = lambda k: key_of(chain, owner, old[k])
        rows = decode_stream(data)
        for row, line in zip(rows, s.lines):
            self.assertEqual(row["creator"], owner)
            for (name, kind, t, value), (_n, _k, _t, was) in zip(row["attributes"], line_cells(line)):
                if t == "key":
                    self.assertEqual(value, new(was))
                elif name == "sourcerefs":
                    self.assertEqual(value, b"\x00" + b"".join(new(was[i:i + 32]) for i in range(1, len(was), 32)))
                elif t == "bytes" and was[:1] == b"\x02" and kind_of(line) != "chunk":
                    self.assertEqual(value[:37], was[:37])
                    self.assertEqual(value[37:], b"".join(new(was[i:i + 32]) for i in range(37, len(was), 32)))
                else:
                    self.assertEqual(value, was)

    def test_refuses_row_rule_violations(self):
        def attr(lines, n, name):
            return next(a for a in lines[n]["attributes"] if a["name"] == name)

        def first(kind):
            return lambda lines: next(i for i, l in enumerate(lines) if kind_of(l) == kind)
        contract, source, chunk = first("contract"), first("source"), first("chunk")
        cases = [
            (lambda l: l[1].update(nonce=5), "nonce 5, expected 1"),
            (lambda l: l[1].update(key="0x" + "00" * 32), "key is not the salt-0 key"),
            (lambda l: l[1].update(expiresAt="0x100"), "entity is not permanent"),
            (lambda l: l[1]["attributes"].reverse(), "not strictly ascending"),
            (lambda l: l[0]["attributes"].extend({"name": f"z{i:02d}", "type": "u64", "indexed": False, "value": "0x1"}
                                                 for i in range(31)), "more than 32 attributes"),
            (lambda l: attr(l, source(l), "content").update(indexed=True), "bytes cannot be indexed"),
            (lambda l: attr(l, contract(l), "name").update(value="a\nb"), "control characters"),
            (lambda l: attr(l, contract(l), "name").update(value="x" * 129), "longer than 128 bytes"),
            (lambda l: attr(l, contract(l), "matchid").update(value="0x07"), "compact 0x quantity"),
            (lambda l: attr(l, contract(l), "optimizerruns").update(value=1 << 31), "i32 value"),
            (lambda l: l[contract(l)]["attributes"].append({"name": "zzz", "type": "u64", "indexed": True, "value": "0x1"}),
             "contract has no attribute zzz"),
            (lambda l: l[contract(l)]["attributes"].remove(attr(l, contract(l), "deployer")), "contract is missing deployer"),
            (lambda l: attr(l, contract(l), "deployer").update(type="bytes32", value="0x" + "00" * 32), "must be indexed addr"),
            (lambda l: attr(l, contract(l), "match").update(indexed=False), "must be indexed str"),
            (lambda l: attr(l, chunk(l), "data").update(value=base64.b64encode(b"x" * 120001).decode()), "120,000-byte budget"),
            (lambda l: attr(l, source(l), "content").update(value=""), "empty bytes value"),
            (lambda l: attr(l, source(l), "content").update(value=base64.b64encode(b"\x03abc").decode()), "unknown codec 3"),
            (lambda l: attr(l, source(l), "content").update(value="AAE"), "invalid base64"),
            (lambda l: attr(l, source(l), "arkiv.schema").update(value="other.v4.chunk"), "is not a sourcify.v4 kind"),
            (lambda l: l[0].update(extra=1), "exactly nonce, key, expiresAt and attributes"),
        ]
        for mutate, message in cases:
            with self.subTest(message=message):
                self.refused(mutate, message)

    def test_refuses_reference_and_content_violations(self):
        def find(lines, kind, n=0):
            return [l for l in lines if any(a["value"] == f"{PREFIX}.{kind}" for a in l["attributes"])][n]

        def attr(line, name):
            return next(a for a in line["attributes"] if a["name"] == name)

        def flip(a):
            raw = bytearray(base64.b64decode(a["value"]))
            raw[-1] ^= 1
            a["value"] = base64.b64encode(bytes(raw)).decode()

        def set_bytes(a, raw):
            a["value"] = base64.b64encode(raw).decode()

        def later_ref(lines):
            c = find(lines, "contract")
            attr(c, "compilationref")["value"] = "0x" + key_of(CHAIN, OWNER, len(lines) - 1).hex()

        def wrong_kind(lines):
            c = find(lines, "contract")
            attr(c, "compilationref")["value"] = find(lines, "code")["key"]

        def sourcerefs_codec(lines):
            a = attr(find(lines, "compilation"), "sourcerefs")
            raw = bytearray(base64.b64decode(a["value"]))
            raw[0] = 1
            set_bytes(a, bytes(raw))

        def manifest_change(lines):
            a = attr(find(lines, "compilation"), "manifest")
            set_bytes(a, b"\x00" + b'{"codes":{},"digests":{},"refs":[],"sources":[]}')

        def group_change(lines):
            set_bytes(attr(find(lines, "compilation"), "artifacts"), b"\x00{}")

        def chunk_change(lines):
            flip(attr(find(lines, "chunk"), "data"))

        def compilationhash(lines):
            attr(find(lines, "contract"), "compilationhash")["value"] = "0x" + "77" * 32

        def abihash(lines):
            attr(find(lines, "contract"), "abihash")["value"] = "0x" + "77" * 32

        def codehash(lines):
            attr(find(lines, "contract"), "runtimecodehash")["value"] = "0x" + "77" * 32

        def literal(lines):
            attr(find(lines, "contract", 1), "creationcodehash")["value"] = "0x" + "77" * 32

        def matchid(lines):
            attr(find(lines, "contract", 1), "matchid")["value"] = "0x7"

        def address(lines):
            attr(find(lines, "contract", 1), "address")["value"] = "0x" + "ab" * 20

        def chainid(lines):
            attr(find(lines, "contract"), "chainid")["value"] = "0x2"

        def source_hash(lines):
            attr(find(lines, "source"), "hash")["value"] = "0x" + "77" * 32

        def code_content(lines):
            set_bytes(attr(find(lines, "code"), "code"), b"\x00\x60\x00")

        def coderef_missing(lines):
            comp = find(lines, "compilation")
            comp["attributes"].remove(attr(comp, "runtimecoderef"))

        def chunk_list_len(lines):
            a = attr(find(lines, "source", 1), "content")
            raw = bytearray(base64.b64decode(a["value"]))
            raw[33:37] = (int.from_bytes(raw[33:37], "big") - 1).to_bytes(4, "big")
            set_bytes(a, bytes(raw))

        def duplicate_hash(lines):
            find(lines, "code", 1)["attributes"] = copy.deepcopy(find(lines, "code")["attributes"])

        cases = [
            (later_ref, "before it exists"),
            (wrong_kind, "but it is a code"),
            (sourcerefs_codec, "malformed sourcerefs"),
            (manifest_change, "compilation hash mismatch"),
            (group_change, "group artifacts digest mismatch"),
            (chunk_change, "chunk integrity mismatch"),
            (compilationhash, "without its compilationhash"),
            (abihash, "abihash does not match"),
            (codehash, "without its code hash"),
            (literal, "literal code hash mismatch"),
            (matchid, "two contracts with matchid 7"),
            (address, "two contracts for"),
            (chainid, "another source chain"),
            (source_hash, "source hash mismatch"),
            (code_content, "code hash mismatch"),
            (coderef_missing, "code slot runtime and its coderef disagree"),
            (chunk_list_len, "chunk"),
            (duplicate_hash, "two code entities with hash"),
        ]
        for mutate, message in cases:
            with self.subTest(message=message):
                self.refused(mutate, message)

    def test_refuses_bundle_violations(self):
        s = build()
        s.write(self.tmp / "ok")
        cases = []

        def copy_bundle(name, change):
            root = self.tmp / name
            shutil.copytree(self.tmp / "ok", root)
            change(root)
            return root

        def part_bytes(root):
            p = root / "entities" / "000001.jsonl"
            p.write_bytes(p.read_bytes().replace(b'"nonce":0', b'"nonce": 0', 1))

        def resum(root):
            sums = "".join(f"{hashlib.sha256((root / p).read_bytes()).hexdigest()}  {p}\n"
                           for p in [l.split("  ")[1] for l in (root / "SHA256SUMS").read_text().splitlines()])
            (root / "SHA256SUMS").write_text(sums)

        def report_field(field, value):
            def change(root):
                r = json.loads((root / "report.json").read_text())
                r[field] = value
                (root / "report.json").write_text(json.dumps(r))
                resum(root)
            return change

        def seed_version(root):
            d = json.loads((root / "seed.json").read_text())
            d["version"] = 1
            (root / "seed.json").write_text(json.dumps(d))
            resum(root)

        def extra_file(root):
            (root / "SHA256SUMS").write_text((root / "SHA256SUMS").read_text() + "0" * 64 + "  payloads.bin\n")

        cases = [
            ("bytes", part_bytes, "checksum mismatch"),
            ("units", report_field("storageUnits", 1), "storage units differ"),
            ("values", report_field("bytesValues", 1), "bytes values differ"),
            ("kinds", report_field("kinds", {"contract": 3}), "kind counts differ"),
            ("eof", report_field("sourceCompleteRequired", False), "without a source-complete marker"),
            ("seed", seed_version, "is not a version-2"),
            ("sums", extra_file, "SHA256SUMS must list exactly"),
        ]
        for name, change, message in cases:
            with self.subTest(message=message):
                root = copy_bundle(name, change)
                done = run(["convert", "--bundle", root, "--out", self.tmp / f"{name}-out", "--reserve-gib", "0"], check=False)
                self.assertNotEqual(done.returncode, 0)
                self.assertIn(message, done.stderr)
        root = copy_bundle("eof-allowed", report_field("sourceCompleteRequired", False))
        self.convert(root, "eof-allowed-out", ("--allow-eof-source",))
        done = run(["convert", "--bundle", self.tmp / "ok", "--out", self.tmp / "eof-allowed-out", "--reserve-gib", "0"],
                   check=False)
        self.assertIn("output already exists", done.stderr)

    @unittest.skipUnless(os.environ.get("SOURCIFY_V4_BUNDLE"), "SOURCIFY_V4_BUNDLE not set")
    def test_real_bundle(self):
        root = Path(os.environ["SOURCIFY_V4_BUNDLE"])
        report, data = self.convert(root)
        bundle_report = json.loads((root / "report.json").read_text())
        self.assertEqual(report["rows"], bundle_report["entities"])
        self.assertEqual(report["entitiesByKind"], bundle_report["kinds"])
        self.assertEqual(int(report["storageUnits"]), bundle_report["storageUnits"])
        seed = json.loads((root / "seed.json").read_text())
        owner = bytes.fromhex(seed["owner"][2:])
        rows = iter(decode_stream(data))
        for part in seed["entities"]:
            for raw in (root / part["path"]).open("rb"):
                line, row = json.loads(raw), next(rows)
                self.assertEqual((row["creator"], row["nonce"], row["expiry"], row["flags"], row["salt"]),
                                 (owner, line["nonce"], MAX, 0, 0))
                self.assertEqual(row["attributes"], line_cells(line))
        self.assertIsNone(next(rows, None))

    @unittest.skipUnless(os.environ.get("ARKIV_PROPOSER_BIN"), "ARKIV_PROPOSER_BIN not set")
    def test_stream_build_accepts_the_stream(self):
        s = build()
        s.write(self.tmp / "bundle")
        report, _ = self.convert(self.tmp / "bundle")
        binary = os.environ["ARKIV_PROPOSER_BIN"]
        src = self.tmp / "genesis-src"
        src.mkdir()
        sponsor = "0x" + "33" * 20
        (src / "chain.toml").write_text(f"""schema = "arkiv-genesis-bundle-v1"
protocol = "arkiv-launch-prefill-v1"
name = "v4 converter test"
[profile]
preset = "arkiv-entity-v3/glm-v2@4"
[chain]
id = "{CHAIN}"
run_id = "0x000000000000000000000000000000aa"
start_time = "2026-10-29T00:00:00Z"
block_period_ms = "2000"
proposer = "@proposer"
[allocations]
expected_initial_supply_wei = "1000000000000000000000000000000"
[[allocations.entry]]
label = "proposer"
address = "{sponsor}"
balance_wei = "1000000000000000000000000000000"
[prefill]
builder = "arkiv-prefill-v1"
namespace = "entities"
rent_policy = "sponsored-rent-v1"
sponsor = "@proposer"
allow_permanent = true
[prefill.limits]
max_entities = "256"
max_canonical_bytes = "1048576"
max_storage_units = "8388608"
""")
        for args in (["genesis", "lock", "chain.toml"], ["genesis", "build", "chain.toml", "--out", "base", "--locked"]):
            subprocess.run([binary, *args], cwd=src, check=True, capture_output=True)
        done = subprocess.run([binary, "genesis", "stream-build", src / "base" / "genesis.bin", "--prefill",
                               self.tmp / "out" / "prefill.bin", "--out", self.tmp / "node-bundle", "--sponsor", sponsor],
                              capture_output=True, text=True, env={**os.environ, "ARKIV_PREFILL_RESERVE_GIB": "0"})
        self.assertEqual(done.returncode, 0, done.stderr[-2000:])
        built = json.loads(done.stdout.strip().splitlines()[-1])
        self.assertEqual((built["rowCount"], built["storageUnits"], built["streamHash"], built["version"], built["burnedWei"]),
                         (str(report["rows"]), report["storageUnits"], report["streamHash"], 4, "0"))
        verified = subprocess.run([binary, "genesis", "stream-verify", self.tmp / "node-bundle"], capture_output=True, text=True)
        self.assertEqual(verified.returncode, 0, verified.stderr[-2000:])


if __name__ == "__main__":
    unittest.main()
