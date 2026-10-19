#!/usr/bin/env python3
"""Bounded converter checks; optional real processor-bundle integration.

Run: python3 scripts/test-sourcify-prefill.py
Set SOURCIFY_SMOKE_BUNDLE to exercise the preserved 263-entity source fixture.
"""

import base64
import collections
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import struct
import tempfile
import unittest


spec = importlib.util.spec_from_file_location("prefill", Path(__file__).with_name("sourcify-prefill.py"))
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)
CREATOR = "0x" + "12" * 20


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def entity(content, creator=CREATOR):
    payload = canonical({"schema": "sourcify.sourcefile.v1", "content": content})
    attributes = {"projectId": {"type": "str", "value": "sourcify"},
                  "kind": {"type": "str", "value": "sourcefile"},
                  "hash": {"type": "bytes32", "value": "0x" + hashlib.sha256(content.encode()).hexdigest()},
                  "size": {"type": "i32", "value": len(content.encode())}}
    identity = {"kind": "sourcefile", "content": {"schema": "sourcify-arkiv.staging-entity.v1",
                 "contentType": "application/json", "payloadEncoding": "hex", "payload": "0x" + payload.hex(),
                 "attributes": attributes, "keyReferences": "staging-entity-id"}}
    sid = hashlib.sha256(canonical(identity)).hexdigest()
    row = {"$key": "0x" + hashlib.sha256(b"arkiv-prefill.imported-staging.v1:" + sid.encode()).hexdigest(),
           "$creator": creator, "$owner": creator, "$expiresAt": "never", "$creationFlags": [],
           "$contentType": "application/json", "$payload": base64.b64encode(payload).decode(),
           "contentencoding": "str:identity", "originalcontenttype": "str:application/json",
           "payloadsha256": "bytes32:0x" + hashlib.sha256(payload).hexdigest(), "stagingid": "bytes32:0x" + sid,
           "uploadschema": "str:sourcify-arkiv.upload.v1"}
    for name, attr in attributes.items():
        row["projectid" if name == "projectId" else name] = f"{attr['type']}:{attr['value']}"
    return row


def bundle(root, rows, lines=None):
    root.mkdir()
    (root / "entities").mkdir()
    data = lines if lines is not None else b"".join(canonical(row) + b"\n" for row in rows)
    part = {"path": "entities/000001.jsonl", "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    (root / part["path"]).write_bytes(data)
    seed = {"version": 1, "chainId": 7706815, "entities": [{"path": part["path"]}]}
    report = {"schema": "sourcify-offline-import.v1", "status": "completed", "sourceChainId": 1,
              "sourceCompleteRequired": True, "sourceSkippedCount": 0, "contracts": 0, "entities": len(rows),
              "expiry": "never", "parts": [part]}
    for name, value in (("seed.json", seed), ("report.json", report)):
        (root / name).write_bytes(canonical(value) + b"\n")
    files = ["seed.json", "report.json", part["path"]]
    (root / "SHA256SUMS").write_text("".join(hashlib.sha256((root / name).read_bytes()).hexdigest() + "  " + name + "\n" for name in files))


def helper_bundle(root, rows):
    """A new test manifest around fixture rows; no claim of upload verification."""
    bundle(root, rows)
    (root / "report.json").unlink()
    contracts = [{"address": row["address"][5:].lower(), "root_entity_key": row["$key"],
                  "contract_id": "ab" * 32, "completed_at": "2026-10-02 01:02:03+00"}
                 for row in rows if row["kind"] == "str:verified_contract"]
    (root / "contracts.jsonl").write_bytes(b"".join(canonical(row) + b"\n" for row in contracts))
    files = []
    for name in ("entities/000001.jsonl", "contracts.jsonl", "seed.json"):
        raw = (root / name).read_bytes()
        files.append({"path": name, "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
    files[0]["entities"] = len(rows)
    output = {"chainId": 7706815, "entities": len(rows), "parts": 1, "dependencyClosed": True,
              "kinds": dict(collections.Counter(row["kind"][4:] for row in rows)),
              "keyReferences": sum(value.startswith("key:") for row in rows for name, value in row.items() if not name.startswith("$")),
              "payloadBytes": sum(len(base64.b64decode(row["$payload"])) for row in rows),
              "maxPayloadBytes": max(len(base64.b64decode(row["$payload"])) for row in rows)}
    source = {"chainId": 7706815, "target": "sorbet", "readOnly": True, "count": len(rows),
              "unique_keys": len(rows), "publishers": len({row["$creator"] for row in rows}),
              "requested": {"mode": "contracts", "count": len(contracts)}, "contracts": {"count": len(contracts)}}
    original = {"schema": "arkiv-prefill.postgres-uploaded-seed.v1", "status": "completed", "source": source,
                "output": output, "files": files}
    provenance = {"schema": "arkiv-prefill.derived-permanent-seed.v1", "status": "completed", "source": original,
                  "output": output, "files": files[:1], "derivation": {"operation": "replace every entity $expiresAt with never",
                  "sourceChecksumsSha256": "cd" * 32, "sourceProvenanceSha256": "ef" * 32}}
    (root / "provenance.json").write_bytes(canonical(provenance) + b"\n")
    pins = [file["path"] for file in files] + ["provenance.json"]
    (root / "SHA256SUMS").write_text("".join(hashlib.sha256((root / name).read_bytes()).hexdigest() + "  " + name + "\n" for name in pins))


def uploaded_entity(kind, document, attrs, dependencies=None):
    payload = canonical(document) if isinstance(document, dict) else document
    ctype = "application/json" if isinstance(document, dict) else "application/octet-stream"
    attrs = {"projectId": {"type": "str", "value": "sourcify"}, "kind": {"type": "str", "value": kind}, **attrs}
    staged_attrs = json.loads(json.dumps(attrs))
    for value in staged_attrs.values():
        if value["type"] == "key":
            value["value"] = dependencies[value["value"]]["stagingid"][10:]
    content = {"schema": "sourcify-arkiv.staging-entity.v1", "contentType": ctype, "payloadEncoding": "hex",
               "payload": "0x" + payload.hex(), "attributes": staged_attrs, "keyReferences": "staging-entity-id"}
    sid = hashlib.sha256(p.json_bytes({"kind": kind, "content": content})).hexdigest()
    row = {"$key": "0x" + hashlib.sha256(b"actual-upload-key:" + sid.encode()).hexdigest(), "$creator": CREATOR,
           "$owner": CREATOR, "$expiresAt": "never", "$creationFlags": [], "$contentType": ctype,
           "$payload": base64.b64encode(payload).decode(), "contentencoding": "str:identity", "originalcontenttype": "str:" + ctype,
           "payloadsha256": "bytes32:0x" + hashlib.sha256(payload).hexdigest(), "stagingid": "bytes32:0x" + sid,
           "uploadschema": "str:sourcify-arkiv.upload.v1"}
    for name, value in attrs.items():
        row["projectid" if name == "projectId" else name] = f"{value['type']}:{value['value']}"
    return row


def uploaded_fixture():
    source = entity("contract Example {}")
    source["$key"] = "0x" + "ab" * 32
    code_hash = "0x" + p.keccak(b"code").hexdigest()
    code = uploaded_entity("code", b"code", {"hash": {"type": "bytes32", "value": code_hash}})
    fingerprint = ["solc", "0.8.0", "Solidity", "Example.sol:Example", {}, [["Example.sol", source["hash"][8:]]], code_hash, code_hash, code_hash, code_hash]
    compiled = {"schema": "sourcify.compilation.v2", "compiler": "solc", "compilerVersion": "0.8.0", "language": "Solidity",
                "fullyQualifiedName": "Example.sol:Example", "compilerSettings": {}, "sources": {"Example.sol": source["hash"][8:]},
                "recompiledCreationHash": code_hash, "recompiledRuntimeHash": code_hash}
    compilation = uploaded_entity("compilation", compiled, {"hash": {"type": "bytes32", "value": "0x" + hashlib.sha256(canonical(fingerprint)).hexdigest()}})
    document = {"schema": "sourcify.verifiedContract.v2", "chainId": "1", "address": CREATOR,
                "codeRefs": {field: code_hash for field in ("creationOnchain", "creationRecompiled", "runtimeOnchain", "runtimeRecompiled")}}
    contract = uploaded_entity("verified_contract", document, {"address": {"type": "addr", "value": CREATOR},
                               "chainid": {"type": "u64", "value": "1"}, "compilationref": {"type": "key", "value": compilation["$key"]}},
                               {compilation["$key"]: compilation})
    return [contract, source, compilation, code]


def options(source, out, *extra):
    return p.parser().parse_args(["convert", "--bundle", str(source), "--out", str(out), "--reserve-gib", "0", *extra])


def frames(path):
    """Independent structural reader, intentionally not the converter's encoder."""
    with path.open("rb") as stream:
        assert stream.read(12) == b"ARKPFL02\0\0\0\2"
        count = int.from_bytes(stream.read(8), "big")
        for _ in range(count):
            size = int.from_bytes(stream.read(4), "big")
            raw = stream.read(size)
            assert len(raw) == size and size <= 8 << 20
            offset = 53
            fields = []
            for _ in range(2):
                length = int.from_bytes(raw[offset:offset + 4], "big")
                offset += 4
                fields.append(raw[offset:offset + length])
                offset += length
            attrs = {}
            amount = int.from_bytes(raw[offset:offset + 4], "big")
            offset += 4
            for _ in range(amount):
                length = int.from_bytes(raw[offset:offset + 4], "big")
                offset += 4
                name = raw[offset:offset + length].decode()
                offset += length
                tag = raw[offset]
                offset += 1
                if tag == 2:
                    length = int.from_bytes(raw[offset:offset + 4], "big")
                    offset += 4
                else:
                    length = {1: 1, 4: 20, 11: 32, 13: 8, 16: 4, 64: 32}[tag]
                attrs[name] = (tag, raw[offset:offset + length])
                offset += length
            assert offset == size
            yield {"creator": raw[:20], "nonce": int.from_bytes(raw[20:28], "big"),
                   "salt": raw[28:44], "expiry": int.from_bytes(raw[44:52], "big"),
                   "flags": raw[52], "contentType": fields[0], "payload": fields[1], "attrs": attrs}
        assert stream.read(1) == b""


class ConversionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_ethereum_keccak_known_vectors(self):
        self.assertEqual(p.keccak(b"abc").hexdigest(), "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45")
        self.assertNotEqual(p.keccak(b"abc").digest(), hashlib.sha3_256(b"abc").digest())

    def test_repeatable_across_cache_sizes_and_multiple_creators(self):
        source = self.root / "source"
        rows = [entity("first", "0x" + "34" * 20), entity("second"), entity("third", "0x" + "34" * 20)]
        bundle(source, rows)
        outputs = []
        for size in (1, 8):
            out = self.root / f"out{size}"
            result = p.run(options(source, out, "--cache-mib", str(size)))
            outputs.append((out / "prefill.bin").read_bytes())
            self.assertEqual(result["entities"], 3)
            self.assertEqual(result["dependencyEdges"], 0)
        self.assertEqual(*outputs)
        decoded = list(frames(self.root / "out1/prefill.bin"))
        self.assertEqual([(r["creator"].hex(), r["nonce"]) for r in decoded], [("12" * 20, 0), ("34" * 20, 0), ("34" * 20, 1)])
        self.assertEqual(decoded[0]["payload"], base64.b64decode(rows[1]["$payload"]))
        self.assertEqual(list(decoded[0]["attrs"]), sorted(decoded[0]["attrs"]))
        self.assertEqual(p.keccak(bytes([len(p.STREAM_DOMAIN)]) + p.STREAM_DOMAIN + outputs[0]).hexdigest(), result["streamHash"][2:])
        with self.assertRaisesRegex(p.Invalid, "already exists"):
            p.run(options(source, self.root / "out1"))

    def test_inventory_publishes_no_stream(self):
        source, out = self.root / "source", self.root / "inventory"
        bundle(source, [entity("hello")])
        args = options(source, out)
        args.command = "inventory"
        result = p.run(args)
        self.assertEqual(result["entities"], 1)
        self.assertFalse((out / "prefill.bin").exists())
        self.assertTrue((out / "source-lock.json").is_file())

    def test_helper_uploaded_keys_payloads_and_dependency_expiry(self):
        source, out = self.root / "helper", self.root / "out"
        rows = uploaded_fixture()
        helper_bundle(source, rows)
        result = p.run(options(source, out))
        self.assertEqual((result["entities"], result["dependencyEdges"], result["sourceAdapter"]), (4, 4, "helper-uploaded-v1"))
        decoded = list(frames(out / "prefill.bin"))
        self.assertEqual([row["payload"] for row in decoded], [base64.b64decode(row["$payload"]) for row in rows])
        keys = [json.loads(line) for line in (out / "keys.jsonl").read_text().splitlines()]
        self.assertEqual(decoded[0]["attrs"]["compilationref"], (64, bytes.fromhex(keys[2]["key"][2:])))
        self.assertEqual(keys[0]["stagingId"], rows[0]["stagingid"][8:])
        self.assertLessEqual(int(keys[0]["expiresAtHeight"]), int(keys[2]["expiresAtHeight"]))
        lock = json.loads((out / "source-lock.json").read_text())
        self.assertIn("contracts.jsonl", [file["path"] for file in lock["files"]])

    def test_helper_fingerprint_validation_and_processor_key_rule(self):
        rows = uploaded_fixture()
        source = self.root / "processor"
        bundle(source, rows)
        with self.assertRaisesRegex(p.Invalid, "source key"):
            p.run(options(source, self.root / "wrong-profile"))
        original = rows[2]
        compiled = json.loads(base64.b64decode(original["$payload"]))
        compiled["compilerVersion"] = "0.9.0"
        changed = uploaded_entity("compilation", compiled, {"hash": {"type": "bytes32", "value": original["hash"][8:]}})
        contract = json.loads(base64.b64decode(rows[0]["$payload"]))
        rows[0] = uploaded_entity("verified_contract", contract, {"address": {"type": "addr", "value": CREATOR},
                                  "chainid": {"type": "u64", "value": "1"}, "compilationref": {"type": "key", "value": changed["$key"]}},
                                  {changed["$key"]: changed})
        rows[2] = changed
        source = self.root / "helper"
        helper_bundle(source, rows)
        with self.assertRaisesRegex(p.Invalid, "compilation fingerprint"):
            p.run(options(source, self.root / "wrong-hash"))

    def test_helper_manifest_and_contract_audit_are_verified(self):
        source = self.root / "helper"
        helper_bundle(source, uploaded_fixture())
        audit = source / "contracts.jsonl"
        audit.write_bytes(audit.read_bytes().replace(CREATOR.encode(), ("0x" + "ff" * 20).encode()))
        with self.assertRaisesRegex(p.Invalid, "contract audit"):
            p.run(options(source, self.root / "wrong-audit"))
        provenance = json.loads((source / "provenance.json").read_bytes())
        provenance["source"]["schema"] = "arkiv-prefill.postgres-uploaded-seed.v99"
        with self.assertRaisesRegex(p.Invalid, "uploaded helper schema"):
            p.helper_manifest(provenance, 7706815, {})

    @unittest.skipUnless(os.environ.get("SOURCIFY_HELPER_BUNDLE"), "set SOURCIFY_HELPER_BUNDLE for bounded actual helper-row integration")
    def test_actual_helper_first_contract_closure(self):
        root = Path(os.environ["SOURCIFY_HELPER_BUNDLE"])
        # Bound this fixture selection to 1000 rows, never load the full dataset.
        part = json.loads((root / "seed.json").read_text())["entities"][0]["path"]
        rows = []
        with (root / part).open() as source:
            for _ in range(1000):
                line = source.readline(p.LINE_CAP + 1)
                if not line:
                    break
                self.assertLessEqual(len(line), p.LINE_CAP)
                rows.append(json.loads(line))
        by_key = {row["$key"]: row for row in rows}
        by_hash = {(row["kind"][4:], row.get("hash", "")[8:]): row for row in rows}
        contract = next(row for row in rows if row["kind"] == "str:verified_contract")
        required, pending = set(), [contract]
        while pending:
            row = pending.pop()
            if row["$key"] in required:
                continue
            required.add(row["$key"])
            for name, value in row.items():
                if not name.startswith("$") and value.startswith("key:"):
                    pending.append(by_key[value[4:]])
            if row["kind"] not in ("str:compilation", "str:verified_contract"):
                continue
            _, raw = p.decode_payload(row)
            doc = json.loads(raw)
            self.assertNotIn("$spill", doc)
            for digest in doc.get("sources", {}).values():
                pending.append(by_hash[("sourcefile", digest)])
            for digest in [*doc.get("codeRefs", {}).values(), doc.get("recompiledCreationHash"), doc.get("recompiledRuntimeHash")]:
                if digest:
                    pending.append(by_hash[("code", digest)])
        selected = [row for row in rows if row["$key"] in required]
        fixture, out = self.root / "actual-helper", self.root / "actual-helper-out"
        helper_bundle(fixture, selected)
        result = p.run(options(fixture, out, "--cache-mib", "1"))
        self.assertEqual(result["inputContracts"], 1)
        self.assertGreaterEqual(result["entities"], 8)
        actual_payloads = {row["stagingid"][8:]: base64.b64decode(row["$payload"]) for row in selected}
        for row in frames(out / "prefill.bin"):
            self.assertEqual(row["payload"], actual_payloads["0x" + row["attrs"]["stagingid"][1].hex()])

    def test_rejects_changed_source_and_publishes_nothing(self):
        source, out = self.root / "source", self.root / "out"
        bundle(source, [entity("hello")])
        path = source / "entities/000001.jsonl"
        path.write_bytes(path.read_bytes() + b" ")
        with self.assertRaises(p.Invalid):
            p.run(options(source, out))
        self.assertFalse(out.exists())
        self.assertEqual(list(self.root.glob(".sourcify-prefill-*")), [])

    def test_rejects_semantically_tampered_payload_even_with_new_file_pins(self):
        source, out = self.root / "source", self.root / "out"
        row = entity("hello")
        row["size"] = "i32:6"
        bundle(source, [row])
        with self.assertRaisesRegex(p.Invalid, "semantic staging identity"):
            p.run(options(source, out))
        self.assertFalse(out.exists())

    def test_rejects_duplicate_identity_and_unsupported_schema(self):
        for name, rows in (("duplicate", [entity("a"), entity("a")]), ("unsupported", [entity("a")])):
            if name == "unsupported":
                rows[0]["uploadschema"] = "str:future"
            source = self.root / name
            bundle(source, rows)
            with self.assertRaises((p.Invalid, sqlite3.IntegrityError)):
                p.run(options(source, self.root / (name + "-out")))

    def test_line_bound_and_duplicate_json_members(self):
        row = entity("hello")
        for name, raw in (("large", b" " * (p.LINE_CAP + 1) + b"\n"), ("duplicate", b'{"x":1,"x":2}\n')):
            source = self.root / name
            bundle(source, [row], raw)
            with self.assertRaises(p.Invalid):
                p.run(options(source, self.root / (name + "-out")))

    def test_no_symlink_following(self):
        source = self.root / "source"
        bundle(source, [entity("hello")])
        path = source / "entities/000001.jsonl"
        outside = self.root / "outside"
        path.rename(outside)
        path.symlink_to(outside)
        with self.assertRaises(OSError):
            p.run(options(source, self.root / "out"))
        with self.assertRaises(p.Invalid):
            p.open_relative(source, "../outside")

    def test_dependency_safe_expiry_and_cycles(self):
        db = p.database(self.root / "graph.sqlite", 1)
        try:
            # 34 rows, edge 0->33 and 33->2; source order is not topological.
            for i in range(34):
                key = i.to_bytes(32, "big")
                db.execute("INSERT INTO entities(seq,old_key,sid,creator,nonce,new_key,kind,part,raw) VALUES(?,?,?,?,?,?,?,?,?)",
                           (i, key, key, bytes(20), i, key, "code", -1, b"{}"))
            p.add_edge(db, 0, 33)
            p.add_edge(db, 33, 2)
            self.assertEqual(p.assign_expiry(db, 34, 100), 98)
            values = dict(db.execute("SELECT seq,expiry FROM entities"))
            self.assertLessEqual(values[0], values[33])
            self.assertLessEqual(values[33], values[2])
            self.assertLessEqual(max(dict(db.execute("SELECT expiry,count(*) FROM entities GROUP BY expiry")).values()), 16)
            db.execute("DELETE FROM ready")
            p.add_edge(db, 2, 0)
            with self.assertRaisesRegex(p.Invalid, "cyclic"):
                p.assign_expiry(db, 34, 100)
        finally:
            db.close()

    def test_expiry_capacity_failure_and_bounded_gzip(self):
        source = self.root / "source"
        bundle(source, [entity(str(i)) for i in range(17)])
        with self.assertRaisesRegex(p.Invalid, "capacity"):
            p.run(options(source, self.root / "out", "--min-expiry", "100", "--max-expiry", "100"))
        import gzip
        row = entity("hello")
        row["contentencoding"] = "str:gzip"
        row["$contentType"] = "application/gzip"
        row["$payload"] = base64.b64encode(gzip.compress(bytes(p.PAYLOAD_CAP + 1))).decode()
        with self.assertRaises(p.Invalid):
            p.decode_payload(row)

    @unittest.skipUnless(os.environ.get("SOURCIFY_SMOKE_BUNDLE"), "set SOURCIFY_SMOKE_BUNDLE for real dataset integration")
    def test_real_263_entity_bundle_preserves_payloads_and_rewrites_references(self):
        source, out = Path(os.environ["SOURCIFY_SMOKE_BUNDLE"]), self.root / "real"
        result = p.run(options(source, out, "--cache-mib", "1"))
        self.assertEqual((result["entities"], result["maxUserAttributes"], result["dependencyEdges"]), (263, 29, 288))
        self.assertEqual(result["streamHash"], "0x6f8634a91b73423f6b5ff462ccc08cbeb6b5b6ae2e799f67f057e5b25ec37e05")
        with (source / "entities/000001.jsonl").open() as stream:
            source_rows = [json.loads(line) for line in stream]
        with (out / "keys.jsonl").open() as stream:
            key_rows = [json.loads(line) for line in stream]
        mapping = {r["sourceKey"]: r["key"] for r in key_rows}
        for original, emitted in zip(source_rows, frames(out / "prefill.bin")):
            self.assertEqual(emitted["payload"], base64.b64decode(original["$payload"]))
            self.assertEqual(emitted["attrs"]["stagingid"][1].hex(), original["stagingid"].split("0x")[1])
            for name, value in original.items():
                if isinstance(value, str) and value.startswith("key:"):
                    self.assertEqual("0x" + emitted["attrs"][name][1].hex(), mapping[value[4:]])


if __name__ == "__main__":
    unittest.main()
