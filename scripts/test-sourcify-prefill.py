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


def imported_bundle(root, rows):
    helper_bundle(root, rows)
    provenance = json.loads((root / "provenance.json").read_bytes())
    original = provenance["source"]
    original["schema"] = "arkiv-prefill.postgres-imported-seed.v1"
    count = original["source"]["contracts"]["count"]
    original["source"] = {"chainId": 1, "readOnly": True, "requested": {"mode": "imported-contracts", "count": count},
                          "contracts": {"count": count}, "count": len(rows), "uploaded": sum(row["$key"] != "0x" + hashlib.sha256(b"arkiv-prefill.imported-staging.v1:" + row["stagingid"][10:].encode()).hexdigest() for row in rows), "synthetic": sum(row["$key"] == "0x" + hashlib.sha256(b"arkiv-prefill.imported-staging.v1:" + row["stagingid"][10:].encode()).hexdigest() for row in rows)}
    original["identityPolicy"] = {"domain": "arkiv-prefill.imported-staging.v1:", "owner": CREATOR}
    original["output"]["contracts"] = count
    provenance["output"]["contracts"] = count
    audit = [json.loads(line) for line in (root / "contracts.jsonl").read_bytes().splitlines()]
    for i, row in enumerate(audit):
        row["ordinal"] = str(i + 1)
        row["inserted_at"] = row.pop("completed_at")
    (root / "contracts.jsonl").write_bytes(b"".join(canonical(row) + b"\n" for row in audit))
    for file in original["files"]:
        raw = (root / file["path"]).read_bytes()
        file.update(bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest())
    (root / "provenance.json").write_bytes(canonical(provenance) + b"\n")
    files = [file["path"] for file in original["files"]] + ["provenance.json"]
    (root / "SHA256SUMS").write_text("".join(hashlib.sha256((root / name).read_bytes()).hexdigest() + "  " + name + "\n" for name in files))


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


# --expiry-policy unlimited over the preserved 263-entity fixture.
REAL_UNLIMITED_STREAM_HASH = "0x3b39bb8f248464c10cdabfdc3a8400b055d132b0959783489b7b433ca66e351d"


class ConversionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_native_imported_manifest_and_subset(self):
        source = self.root / "native"
        imported_bundle(source, [entity("unused")] + uploaded_fixture())
        provenance = json.loads((source / "provenance.json").read_bytes())["source"]
        provenance["expiryPolicy"] = "never"
        (source / "provenance.json").write_bytes(canonical(provenance) + b"\n")
        pins = [file["path"] for file in provenance["files"]] + ["provenance.json"]
        (source / "SHA256SUMS").write_text("".join(hashlib.sha256((source / name).read_bytes()).hexdigest() + "  " + name + "\n" for name in pins))
        result = p.run(options(source, self.root / "out", "--contract-limit", "1"))
        self.assertEqual((result["entities"], result["sourceEntities"]), (4, 5))

    def test_imported_subset_complete_closure_and_renumber(self):
        rows = uploaded_fixture()
        unused = entity("unselected source")
        source = self.root / "imported"
        imported_bundle(source, [unused] + rows)
        result = p.run(options(source, self.root / "out", "--contract-limit", "1"))
        self.assertEqual(result["entities"], 4)
        self.assertEqual(result["sourceEntities"], 5)
        self.assertEqual(result["inputContracts"], 1)
        self.assertEqual([f["nonce"] for f in frames(self.root / "out/prefill.bin")], list(range(4)))
        keys = [json.loads(line) for line in (self.root / "out/keys.jsonl").read_bytes().splitlines()]
        self.assertEqual({k["sourceKey"] for k in keys}, {r["$key"] for r in rows})

    def test_imported_subset_excludes_second_contract_root(self):
        rows = uploaded_fixture()
        first = rows[0]
        document = json.loads(base64.b64decode(first["$payload"]))
        document["address"] = "0x" + "34" * 20
        attrs = {"address": {"type": "addr", "value": document["address"]},
                 "chainid": {"type": "u64", "value": "1"},
                 "compilationref": {"type": "key", "value": rows[2]["$key"]}}
        second = uploaded_entity("verified_contract", document, attrs, {rows[2]["$key"]: rows[2]})
        source = self.root / "imported"
        imported_bundle(source, [entity("unused")] + rows + [second])
        one = p.run(options(source, self.root / "one", "--contract-limit", "1"))
        two = p.run(options(source, self.root / "two", "--contract-limit", "2"))
        self.assertEqual((one["entities"], two["entities"]), (4, 5))
        self.assertEqual((one["sourceContracts"], one["inputContracts"]), (2, 1))

    def test_imported_subset_follows_transitive_spill_parts(self):
        rows = uploaded_fixture()
        content = "x" * 130000
        logical = canonical({"schema": "sourcify.sourcefile.v1", "content": content})
        digest = "0x" + hashlib.sha256(logical).hexdigest()
        blobs = [uploaded_entity("blob", logical[i:i + 100000], {
            "hash": {"type": "bytes32", "value": digest}, "part": {"type": "i32", "value": i // 100000},
            "parts": {"type": "i32", "value": 2}, "size": {"type": "i32", "value": len(logical[i:i + 100000])}})
                 for i in range(0, len(logical), 100000)]
        spill = uploaded_entity("sourcefile", {"$spill": {"hash": digest, "bytes": len(logical), "parts": 2}},
                                {"hash": {"type": "bytes32", "value": "0x" + hashlib.sha256(content.encode()).hexdigest()}})
        rows[3] = uploaded_entity("code", b"code", {
            "hash": {"type": "bytes32", "value": rows[3]["hash"][8:]},
            "extra": {"type": "key", "value": spill["$key"]}}, {spill["$key"]: spill})
        source = self.root / "imported"
        imported_bundle(source, [entity("unused")] + rows + [spill] + blobs)
        result = p.run(options(source, self.root / "out", "--contract-limit", "1"))
        self.assertEqual(result["entities"], 7)
        self.assertEqual(result["entitiesByKind"]["blob"], 2)

    def test_imported_synthetic_owner_rejected(self):
        source = self.root / "imported"
        imported_bundle(source, [entity("unused", "0x" + "34" * 20)] + uploaded_fixture())
        with self.assertRaisesRegex(p.Invalid, "synthetic identity owner mismatch"):
            p.run(options(source, self.root / "out", "--contract-limit", "1"))

    def test_subset_expiry_capacity_uses_selected_count(self):
        source = self.root / "imported"
        imported_bundle(source, [entity(str(i)) for i in range(20)] + uploaded_fixture())
        result = p.run(options(source, self.root / "out", "--contract-limit", "1", "--min-expiry", "1", "--max-expiry", "1"))
        self.assertEqual((result["entities"], result["sourceEntities"]), (4, 24))

    def test_imported_subset_rejects_selected_payload_corruption(self):
        rows = uploaded_fixture()
        rows[1]["$payload"] = base64.b64encode(b"wrong").decode()
        source = self.root / "imported"
        imported_bundle(source, [entity("unused")] + rows)
        with self.assertRaisesRegex(p.Invalid, "payload checksum"):
            p.run(options(source, self.root / "out", "--contract-limit", "1"))

    def test_imported_subset_missing_dependency_rejected(self):
        rows = uploaded_fixture()
        source = self.root / "imported"
        imported_bundle(source, [entity("unused")] + rows[:-1])
        with self.assertRaisesRegex(p.Invalid, "missing code payload dependency"):
            p.run(options(source, self.root / "out", "--contract-limit", "1"))

    def test_imported_identity_counts_rejected(self):
        rows = uploaded_fixture()
        source = self.root / "imported"
        imported_bundle(source, rows)
        report, parts, _ = p.manifest(source, 7706815, 5)
        report["identityCounts"] = (0, len(rows))
        db = p.database(self.root / "test.sqlite", 1)
        with self.assertRaisesRegex(p.Invalid, "imported identity counts mismatch"):
            p.ingest(source, parts, report, db, options(source, self.root / "out", "--contract-limit", "1"), self.root)
        db.close()


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

    def test_far_future_skips_graph_and_fills_heights_in_emission_order(self):
        source, out = self.root / "source", self.root / "out"
        rows = [entity(str(i), CREATOR if i % 3 else "0x" + "34" * 20) for i in range(37)]
        bundle(source, rows)
        def forbidden(*_args, **_kwargs):
            raise AssertionError("far-future conversion must not build or order a dependency graph")
        original = (p.dependencies, p.assign_expiry)
        p.dependencies = p.assign_expiry = forbidden
        try:
            result = p.run(options(source, out, "--expiry-policy", "far-future"))
        finally:
            p.dependencies, p.assign_expiry = original
        far = p.FAR_FUTURE_EXPIRY
        self.assertEqual((result["expiryPolicy"], result["dependencyEdges"]), ("far-future-uniform-v1", None))
        self.assertEqual((result["minimumExpiryHeight"], result["maximumExpiryHeight"]), (far - 36 // 16, far))
        self.assertIn("no-semantic-payload-validation", result["validationScope"])
        decoded = list(frames(out / "prefill.bin"))
        self.assertEqual([row["expiry"] for row in decoded], [far - 2 + i // 16 for i in range(37)])
        self.assertLessEqual(max(collections.Counter(row["expiry"] for row in decoded).values()), p.EXPIRY_CAP)
        self.assertEqual([(row["creator"].hex(), row["nonce"]) for row in decoded],
                         sorted((row["creator"].hex(), row["nonce"]) for row in decoded))
        keys = [json.loads(line) for line in (out / "keys.jsonl").read_bytes().splitlines()]
        self.assertEqual([int(k["expiresAtHeight"]) for k in keys], [row["expiry"] for row in decoded])
        for k in keys:
            rent = (int(k["storageUnits"]) * int(k["expiresAtHeight"]) * 8 + 17279) // 17280 * 1000000000
            self.assertEqual(int(k["rentWei"]), rent)
        self.assertEqual(int(result["rentWei"]), sum(int(k["rentWei"]) for k in keys))
        # Same rows, same keys/order: only expiry heights and rent differ.
        finite = p.run(options(source, self.root / "finite"))
        self.assertEqual([(r["creator"], r["nonce"], r["payload"], r["attrs"]) for r in frames(self.root / "finite/prefill.bin")],
                         [(r["creator"], r["nonce"], r["payload"], r["attrs"]) for r in decoded])
        self.assertGreater(int(result["rentWei"]), int(finite["rentWei"]) * 100000)

    def test_far_future_keeps_wire_and_key_checks_without_semantic_validation(self):
        tampered = entity("hello")
        tampered["size"] = "i32:6"
        source = self.root / "semantic"
        bundle(source, [tampered])
        with self.assertRaisesRegex(p.Invalid, "semantic staging identity"):
            p.run(options(source, self.root / "finite"))
        # Documented trade-off: semantic payload identity is not re-derived.
        self.assertEqual(p.run(options(source, self.root / "far", "--expiry-policy", "far-future"))["entities"], 1)
        corrupt = entity("hello")
        corrupt["$payload"] = base64.b64encode(b"wrong").decode()
        source = self.root / "corrupt"
        bundle(source, [corrupt])
        with self.assertRaisesRegex(p.Invalid, "payload checksum"):
            p.run(options(source, self.root / "corrupt-out", "--expiry-policy", "far-future"))
        contract, sourcefile, _compilation, code = uploaded_fixture()
        source = self.root / "dangling"
        helper_bundle(source, [contract, sourcefile, code])
        with self.assertRaisesRegex(p.Invalid, "missing reference while encoding"):
            p.run(options(source, self.root / "dangling-out", "--expiry-policy", "far-future"))
        self.assertFalse((self.root / "dangling-out").exists())
        # Inventory emits nothing, so it must not claim key references were resolved.
        args = options(source, self.root / "dangling-inventory", "--expiry-policy", "far-future")
        args.command = "inventory"
        self.assertEqual(p.run(args)["validationScope"],
                         "all-source-checksums-wire-inventory/no-semantic-payload-validation")

    def test_far_future_contract_limit_keeps_complete_closure(self):
        rows = uploaded_fixture()
        source = self.root / "imported"
        imported_bundle(source, [entity("unused")] + rows)
        result = p.run(options(source, self.root / "out", "--contract-limit", "1", "--expiry-policy", "far-future"))
        self.assertEqual((result["entities"], result["sourceEntities"], result["dependencyEdges"]), (4, 5, 4))
        self.assertEqual(result["expiryPolicy"], "far-future-uniform-v1")
        self.assertIn("selected-closure-semantic-validation", result["validationScope"])
        self.assertEqual({f["expiry"] for f in frames(self.root / "out/prefill.bin")}, {p.FAR_FUTURE_EXPIRY})
        source = self.root / "missing"
        imported_bundle(source, [entity("unused")] + rows[:-1])
        with self.assertRaisesRegex(p.Invalid, "missing code payload dependency"):
            p.run(options(source, self.root / "missing-out", "--contract-limit", "1", "--expiry-policy", "far-future"))

    def test_expiry_policy_bounds_and_capacity(self):
        source = self.root / "source"
        bundle(source, [entity(str(i)) for i in range(17)])
        for name, extra, message in (
                ("finite-ceiling", ("--max-expiry", str(p.MAX_EXPIRY + 1)), "maximum expiry"),
                ("far-ceiling", ("--expiry-policy", "far-future", "--max-expiry", str(p.FAR_FUTURE_EXPIRY_LIMIT + 1)), "maximum expiry"),
                ("far-capacity", ("--expiry-policy", "far-future", "--min-expiry", "100", "--max-expiry", "100"), "capacity")):
            with self.assertRaisesRegex(p.Invalid, message):
                p.run(options(source, self.root / name, *extra))
        result = p.run(options(source, self.root / "far-edge", "--expiry-policy", "far-future",
                               "--min-expiry", "99", "--max-expiry", "100"))
        self.assertEqual((result["minimumExpiryHeight"], result["maximumExpiryHeight"]), (99, 100))
        top = p.run(options(source, self.root / "far-top", "--expiry-policy", "far-future",
                            "--max-expiry", str(p.FAR_FUTURE_EXPIRY_LIMIT)))
        self.assertEqual(max(f["expiry"] for f in frames(self.root / "far-top/prefill.bin")), p.FAR_FUTURE_EXPIRY_LIMIT)
        self.assertLess(max(int(json.loads(k)["rentWei"]) // 1000000000
                            for k in (self.root / "far-top/keys.jsonl").read_bytes().splitlines()), 1 << 64)
        self.assertEqual(top["entities"], 17)

    def test_unlimited_makes_every_entity_permanent_without_graph_or_rent(self):
        source, out = self.root / "source", self.root / "out"
        # 37 rows exceed any 16-per-height expiry capacity; permanence has none.
        rows = [entity(str(i), CREATOR if i % 3 else "0x" + "34" * 20) for i in range(37)]
        bundle(source, rows)
        def forbidden(*_args, **_kwargs):
            raise AssertionError("unlimited conversion must not build or order a dependency graph")
        original = (p.dependencies, p.assign_expiry, p.uniform_minimum)
        p.dependencies = p.assign_expiry = p.uniform_minimum = forbidden
        try:
            result = p.run(options(source, out, "--expiry-policy", "unlimited"))
        finally:
            p.dependencies, p.assign_expiry, p.uniform_minimum = original
        self.assertEqual((result["expiryPolicy"], result["dependencyEdges"], result["expiryPerHeight"]),
                         ("unlimited-v1", None, None))
        self.assertEqual((result["minimumExpiryHeight"], result["maximumExpiryHeight"]), ("never", "never"))
        self.assertEqual((result["permanentExpiry"], result["permanentPrefillRequired"]), (str(2 ** 64 - 1), True))
        self.assertIn("no-semantic-payload-validation", result["validationScope"])
        self.assertEqual(result["rentWei"], "0")
        decoded = list(frames(out / "prefill.bin"))
        self.assertEqual({row["expiry"] for row in decoded}, {p.PERMANENT_EXPIRY})
        self.assertEqual(p.PERMANENT_EXPIRY, 0xFFFFFFFFFFFFFFFF)
        keys = [json.loads(line) for line in (out / "keys.jsonl").read_bytes().splitlines()]
        self.assertEqual({(k["expiresAtHeight"], k["rentWei"]) for k in keys}, {(str(2 ** 64 - 1), "0")})
        self.assertEqual(int(result["storageUnits"]), sum(int(k["storageUnits"]) for k in keys))
        # Same rows, keys, order and storage units as the finite policy; only
        # the expiry sentinel and the (absent) rent differ.
        finite = p.run(options(source, self.root / "finite"))
        same = lambda path: [(r["creator"], r["nonce"], r["salt"], r["flags"], r["contentType"], r["payload"], r["attrs"])
                             for r in frames(path / "prefill.bin")]
        self.assertEqual(same(out), same(self.root / "finite"))
        self.assertEqual(result["storageUnits"], finite["storageUnits"])
        self.assertGreater(int(finite["rentWei"]), 0)
        finite_keys = [json.loads(line) for line in (self.root / "finite/keys.jsonl").read_bytes().splitlines()]
        self.assertEqual([k["key"] for k in keys], [k["key"] for k in finite_keys])
        # Exactly one frame field differs: the 8-byte expiry at offset 44.
        self.assertEqual((out / "prefill.bin").stat().st_size, (self.root / "finite/prefill.bin").stat().st_size)

    def test_unlimited_rejects_finite_expiry_options_and_keeps_closure_checks(self):
        source = self.root / "source"
        bundle(source, [entity(str(i)) for i in range(17)])
        for name, extra in (("max", ("--max-expiry", "100")), ("min", ("--min-expiry", "1"))):
            with self.assertRaisesRegex(p.Invalid, "do not apply"):
                p.run(options(source, self.root / name, "--expiry-policy", "unlimited", *extra))
            self.assertFalse((self.root / name).exists())
        # The finite capacity bound does not constrain permanent rows.
        with self.assertRaisesRegex(p.Invalid, "capacity"):
            p.run(options(source, self.root / "finite", "--min-expiry", "100", "--max-expiry", "100"))
        self.assertEqual(p.run(options(source, self.root / "unlimited", "--expiry-policy", "unlimited"))["entities"], 17)
        # The node's streaming row limit still applies, before any output.
        limit = p.STREAM_ROW_LIMIT
        p.STREAM_ROW_LIMIT = 16
        try:
            with self.assertRaisesRegex(p.Invalid, "row limit"):
                p.run(options(source, self.root / "too-many", "--expiry-policy", "unlimited"))
        finally:
            p.STREAM_ROW_LIMIT = limit
        self.assertFalse((self.root / "too-many").exists())
        self.assertEqual(p.STREAM_ROW_LIMIT, 16 * p.MAX_EXPIRY)
        rows = uploaded_fixture()
        source = self.root / "imported"
        imported_bundle(source, [entity("unused")] + rows)
        result = p.run(options(source, self.root / "subset", "--contract-limit", "1", "--expiry-policy", "unlimited"))
        self.assertEqual((result["entities"], result["dependencyEdges"], result["expiryPolicy"]), (4, 4, "unlimited-v1"))
        self.assertIn("selected-closure-semantic-validation", result["validationScope"])
        self.assertEqual({f["expiry"] for f in frames(self.root / "subset/prefill.bin")}, {p.PERMANENT_EXPIRY})
        source = self.root / "missing"
        imported_bundle(source, [entity("unused")] + rows[:-1])
        with self.assertRaisesRegex(p.Invalid, "missing code payload dependency"):
            p.run(options(source, self.root / "missing-out", "--contract-limit", "1", "--expiry-policy", "unlimited"))

    @unittest.skipUnless(os.environ.get("SOURCIFY_SMOKE_BUNDLE"), "set SOURCIFY_SMOKE_BUNDLE for real dataset integration")
    def test_real_263_entity_bundle_unlimited(self):
        source, out = Path(os.environ["SOURCIFY_SMOKE_BUNDLE"]), self.root / "real-unlimited"
        result = p.run(options(source, out, "--cache-mib", "1", "--expiry-policy", "unlimited"))
        self.assertEqual((result["entities"], result["maxUserAttributes"], result["dependencyEdges"]), (263, 29, None))
        self.assertEqual((result["expiryPolicy"], result["rentWei"]), ("unlimited-v1", "0"))
        self.assertEqual(result["streamHash"], REAL_UNLIMITED_STREAM_HASH)
        finite = self.root / "real-finite"
        finite_result = p.run(options(source, finite, "--cache-mib", "1"))
        same = lambda path: [(r["creator"], r["nonce"], r["payload"], r["attrs"]) for r in frames(path / "prefill.bin")]
        self.assertEqual(same(out), same(finite))
        self.assertEqual({r["expiry"] for r in frames(out / "prefill.bin")}, {p.PERMANENT_EXPIRY})
        self.assertEqual(result["storageUnits"], finite_result["storageUnits"])

    @unittest.skipUnless(os.environ.get("SOURCIFY_SMOKE_BUNDLE"), "set SOURCIFY_SMOKE_BUNDLE for real dataset integration")
    def test_real_263_entity_bundle_far_future(self):
        source, out = Path(os.environ["SOURCIFY_SMOKE_BUNDLE"]), self.root / "real-far"
        result = p.run(options(source, out, "--cache-mib", "1", "--expiry-policy", "far-future"))
        self.assertEqual((result["entities"], result["maxUserAttributes"], result["dependencyEdges"]), (263, 29, None))
        self.assertEqual((result["minimumExpiryHeight"], result["maximumExpiryHeight"]),
                         (p.FAR_FUTURE_EXPIRY - 262 // 16, p.FAR_FUTURE_EXPIRY))
        self.assertEqual((result["streamHash"], result["rentWei"]),
                         ("0xd6d893668d463a3f944792caa69af23db779e4d5ba0e7ce66dfcf6891eb08042", "715027777772040000000000"))
        finite = self.root / "real-finite"
        p.run(options(source, finite, "--cache-mib", "1"))
        same = lambda path: [(r["creator"], r["nonce"], r["payload"], r["attrs"]) for r in frames(path / "prefill.bin")]
        self.assertEqual(same(out), same(finite))

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
