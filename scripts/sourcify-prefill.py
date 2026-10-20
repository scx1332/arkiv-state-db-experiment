#!/usr/bin/env python3
"""Convert a pinned Sourcify processor/helper bundle to experimental v2 prefill.

Only this tool's new output/scratch directory is written. Dataset-sized indexes,
rows, dependency edges and sort queues live in SQLite, not Python collections.
The source bundle is never changed; conversion does not start a node.

--contract-limit selects the first N imported contract audit ordinals and their
complete dependency closure. All source checksums and wire inventory are checked;
semantic payload validation applies to that closure. Source ordinals are retained,
but selected creator nonces are reassigned, so nested subsets need not share the
same derived destination keys.

--expiry-policy far-future gives every entity a finite far-future expiry: the
heights ending at --max-expiry (default 10**12), filled in emission order with
at most 16 entities per height. No entity can expire within any realistic chain
horizon, so the dependency graph and dependency-safe expiry ordering are
skipped. The signed GLM protocol rejects the u64::MAX permanent sentinel, so
this is the closest admissible form; the base genesis max_lifetime must be at
least --max-expiry and the sponsor must fund the proportionally larger rent.
Without --contract-limit this also skips semantic payload validation (staging
identity, content hashes, payload schemas and payload hash references); source
checksums and per-row wire validation remain enforced, and convert still
requires every typed key reference to resolve. The base genesis must also keep
expiry_per_block >= 16.
"""

import argparse
import base64
import collections
import ctypes
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sqlite3
import struct
import sys
import tempfile
import time
import zlib


GIB = 1 << 30
LINE_CAP = 1 << 20
MANIFEST_CAP = 16 << 20
PAYLOAD_CAP = 122880
FRAME_CAP = 8 << 20
ATTR_CAP = 55
MAX_EXPIRY = 6307200
# A finite far-future horizon: 10**12 blocks is ~63,000 years at 2 s blocks.
FAR_FUTURE_EXPIRY = 10 ** 12
# At most 64 cells of 128 KiB keep a row below 9 million storage units, so its
# rent gas ceil(units * expiry * 8 / 17280) fits u64 for any expiry <= 10**15.
FAR_FUTURE_EXPIRY_LIMIT = 10 ** 15
EXPIRY_CAP = 16
STREAM_DOMAIN = b"arkiv/prefill-stream/v2"
KINDS = {"sourcefile", "code", "blob", "compilation", "verified_contract"}
WIRE_META = {"contentencoding", "originalcontenttype", "payloadsha256", "stagingid", "uploadschema"}
SYSTEM = {"$key", "$creator", "$owner", "$expiresAt", "$contentType", "$payload", "$creationFlags"}
RESERVED = {"and", "or", "not", "true", "false", "startswith", "exists", "typeof", "bool", "i32", "u64", "u256", "dec", "bytes32", "bytes", "str", "addr", "key"}


class Invalid(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Invalid(message)


def keccak(data=b""):
    # OpenSSL 3.5 exposes legacy Ethereum Keccak separately from NIST SHA3.
    try:
        return hashlib.new("KECCAK-256", data)
    except ValueError as error:
        raise Invalid("Python/OpenSSL must expose KECCAK-256 (OpenSSL 3.5+); SHA3-256 is not interchangeable") from error


def canonical(value):
    if isinstance(value, dict):
        return {key: canonical(value[key]) for key in sorted(value, key=lambda s: s.encode("utf-16-be"))}
    if isinstance(value, list):
        return [canonical(v) for v in value]
    return value


def json_bytes(value):
    return json.dumps(canonical(value), ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        require(key not in obj, f"duplicate JSON member {key}")
        obj[key] = value
    return obj


def parse_json(raw):
    return json.loads(raw, object_pairs_hook=unique_object,
                      parse_constant=lambda value: (_ for _ in ()).throw(Invalid(f"invalid JSON number {value}")))


def hex_bytes(value, size):
    require(isinstance(value, str) and re.fullmatch(r"0x[0-9a-fA-F]{%d}" % (size * 2), value), f"expected {size}-byte hex value")
    return bytes.fromhex(value[2:])


def typed(row, name, tag):
    value = row.get(name)
    require(isinstance(value, str) and value.startswith(tag + ":"), f"missing/invalid {name}:{tag}")
    return value[len(tag) + 1:]


def number(value, low, high, label):
    require(type(value) is int and low <= value <= high, f"invalid {label}")
    return value


def open_relative(root, relative):
    """Walk directory fds without following a symlink at any component."""
    p = PurePosixPath(relative)
    require(isinstance(relative, str) and relative and not p.is_absolute()
            and all(part not in ("", ".", "..") for part in relative.split("/")), "unsafe source path")
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in p.parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        result = os.open(p.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
        import stat
        if not stat.S_ISREG(os.fstat(result).st_mode):
            os.close(result)
            raise Invalid("source must be a regular file")
        return os.fdopen(result, "rb")
    finally:
        os.close(fd)


def read_small(root, name, cap=MANIFEST_CAP):
    with open_relative(root, name) as stream:
        raw = stream.read(cap + 1)
    require(len(raw) <= cap, f"{name} exceeds metadata limit")
    return raw


def manifest(root, chain_id, max_skipped):
    sums_raw = read_small(root, "SHA256SUMS")
    sums = {}
    for line in sums_raw.decode().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
        require(match is not None, "malformed SHA256SUMS")
        digest, path = match.groups()
        require(path not in sums, "duplicate checksum path")
        sums[path] = digest
    documents = {}
    locks = []
    require(("report.json" in sums) != ("provenance.json" in sums), "ambiguous/unsupported source manifest")
    audit_name = "report.json" if "report.json" in sums else "provenance.json"
    for name in ("seed.json", audit_name):
        raw = read_small(root, name)
        digest = hashlib.sha256(raw).hexdigest()
        require(sums.get(name) == digest, f"{name}: checksum mismatch")
        documents[name] = parse_json(raw)
        locks.append({"path": name, "bytes": len(raw), "sha256": digest})
    seed = documents["seed.json"]
    require(set(seed) == {"version", "chainId", "entities"} and seed["version"] == 1, "unsupported seed schema/defaults")
    require(seed["chainId"] == chain_id, "destination chain ID differs from source seed")
    if audit_name == "report.json":
        report = documents[audit_name]
        require(report.get("schema") == "sourcify-offline-import.v1" and report.get("status") == "completed", "unsupported/incomplete processor report")
        require(report.get("sourceChainId") == 1 and report.get("sourceCompleteRequired") is True, "complete Ethereum-mainnet source required")
        number(report.get("sourceSkippedCount", 0), 0, max_skipped, "source skip count")
        require(report.get("expiry") == "never", "unsupported source expiry policy")
        report = {**report, "adapter": "processor-v1"}
    else:
        report = helper_manifest(documents[audit_name], chain_id, sums)
    parts = seed["entities"]
    require(isinstance(parts, list) and len(parts) <= 100000 and len(parts) == len(report.get("parts", [])), "invalid part inventory")
    paths = set()
    for part, audit in zip(parts, report["parts"]):
        require(isinstance(part, dict) and set(part) == {"path"}, "unsupported entity part defaults")
        path = part["path"]
        require(isinstance(path, str) and path not in paths and path == audit.get("path") and path.endswith(".jsonl"), "invalid/duplicate entity part")
        paths.add(path)
        require(sums.get(path) == audit.get("sha256"), f"{path}: report checksum mismatch")
        number(audit.get("bytes"), 0, (1 << 63) - 1, "part bytes")
        locks.append({"path": path, "bytes": audit["bytes"], "sha256": sums[path]})
    number(report.get("entities"), 0, (1 << 63) - 1, "entity count")
    number(report.get("contracts"), 0, (1 << 63) - 1, "contract count")
    part_locks = locks[2:]
    if report["adapter"].startswith("helper-"):
        locks.append(report["contractsFile"])
    lock = {"schema": "arkiv-sourcify-source-lock-v1", "sourceChainId": 1,
            "destinationChainId": chain_id, "sha256sums": hashlib.sha256(sums_raw).hexdigest(),
            "files": locks, "sourceSkippedCount": report.get("sourceSkippedCount", 0)}
    return report, part_locks, lock


def helper_manifest(provenance, chain_id, sums):
    """The pinned October export predates the processor and uses real chain keys.

    Its permanent derivation replaces part hashes, while contracts/seed hashes
    remain those of the embedded original provenance. Never mix the two sets.
    """
    native_imported = isinstance(provenance, dict) and provenance.get("schema") == "arkiv-prefill.postgres-imported-seed.v1"
    if native_imported:
        require(provenance.get("status") == "completed" and provenance.get("expiryPolicy") == "never", "incomplete/nonpermanent imported provenance")
        original = provenance
    else:
        require(isinstance(provenance, dict) and provenance.get("schema") == "arkiv-prefill.derived-permanent-seed.v1"
                and provenance.get("status") == "completed", "unsupported/incomplete helper provenance")
        derivation = provenance.get("derivation", {})
        require(derivation.get("operation") == "replace every entity $expiresAt with never", "unsupported helper derivation")
        for field in ("sourceChecksumsSha256", "sourceProvenanceSha256"):
            require(isinstance(derivation.get(field), str) and re.fullmatch(r"[0-9a-f]{64}", derivation[field]), "invalid helper source pin")
        original = provenance.get("source", {})
    imported = original.get("schema") == "arkiv-prefill.postgres-imported-seed.v1"
    require(original.get("schema") in ("arkiv-prefill.postgres-uploaded-seed.v1", "arkiv-prefill.postgres-imported-seed.v1")
            and original.get("status") == "completed", "unsupported/incomplete uploaded helper schema")
    source, output = original.get("source", {}), provenance.get("output", {})
    require(output == original.get("output") and output.get("chainId") == chain_id
            and output.get("dependencyClosed") is True, "inconsistent helper output inventory")
    require((source.get("chainId") == 1 if imported else source.get("chainId") == chain_id and source.get("target") == "sorbet")
            and source.get("readOnly") is True, "unsupported helper source")
    requested, contracts = source.get("requested", {}), source.get("contracts", {})
    require(requested.get("mode") == ("imported-contracts" if imported else "contracts") and requested.get("count") == contracts.get("count"), "unsupported helper selection")
    count = number(output.get("entities"), 0, (1 << 63) - 1, "helper entity count")
    number(contracts.get("count"), 0, count, "helper contract count")
    require(source.get("count") == count and (imported or source.get("unique_keys") == count), "inconsistent helper entity counts")
    kinds = output.get("kinds")
    require(isinstance(kinds, dict) and set(kinds) <= KINDS, "unsupported helper kinds")
    for value in kinds.values():
        number(value, 0, count, "helper kind count")
    require(sum(kinds.values()) == count and kinds.get("verified_contract", 0) == contracts["count"], "inconsistent helper kind counts")
    files = provenance.get("files")
    if native_imported:
        require(isinstance(files, list), "missing imported files")
        files = [file for file in files if file.get("path") not in ("seed.json", "contracts.jsonl")]
    require(isinstance(files, list) and output.get("parts") == len(files), "inconsistent helper part count")
    original_files = original.get("files")
    require(isinstance(original_files, list), "missing original helper files")
    by_name = {}
    for file in original_files:
        require(isinstance(file, dict) and isinstance(file.get("path"), str) and file["path"] not in by_name, "invalid helper file inventory")
        by_name[file["path"]] = file
    require(set(by_name) == {f.get("path") for f in files} | {"seed.json", "contracts.jsonl"}, "unexpected helper file inventory")
    for name in ("seed.json", "contracts.jsonl"):
        require(by_name[name].get("sha256") == sums.get(name), f"{name}: helper checksum mismatch")
        number(by_name[name].get("bytes"), 0, (1 << 63) - 1, "helper file bytes")
    for file in files:
        number(file.get("entities"), 0, count, "helper part entity count")
    require(sum(file["entities"] for file in files) == count, "inconsistent helper part entity counts")
    if imported:
        policy = original.get("identityPolicy", {})
        require(policy.get("domain") == "arkiv-prefill.imported-staging.v1:", "unsupported imported identity domain")
        owner = hex_bytes(policy.get("owner"), 20)
        require(any(owner), "zero imported owner")
        uploaded = number(source.get("uploaded"), 0, count, "uploaded identity count")
        synthetic = number(source.get("synthetic"), 0, count, "synthetic identity count")
        require(uploaded + synthetic == count and output.get("contracts") == contracts["count"], "imported identity/contract count mismatch")
    return {"adapter": "helper-imported-v1" if imported else "helper-uploaded-v1",
            "identityCounts": (uploaded, synthetic) if imported else None,
            "syntheticOwner": owner if imported else None, "entities": count, "contracts": contracts["count"],
            "parts": files, "contractsFile": by_name["contracts.jsonl"], "helperOutput": output,
            "publishers": None if imported else number(source.get("publishers"), 1, count, "helper publisher count")}


def decode_payload(row):
    encoded = row.get("$payload")
    require(isinstance(encoded, str) and len(encoded) <= 4 * ((120000 + 2) // 3), "encoded payload exceeds limit")
    payload = base64.b64decode(encoded, validate=True)
    require(len(payload) <= 120000, "wire payload exceeds limit")
    encoding = typed(row, "contentencoding", "str")
    original = typed(row, "originalcontenttype", "str")
    if encoding == "identity":
        raw = payload
        require(row["$contentType"] == original, "identity content type mismatch")
    elif encoding == "gzip":
        require(row["$contentType"] == "application/gzip", "gzip content type mismatch")
        decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
        raw = decoder.decompress(payload, PAYLOAD_CAP + 1)
        require(decoder.eof and not decoder.unused_data and not decoder.unconsumed_tail, "invalid, concatenated or oversized gzip payload")
    else:
        raise Invalid("unsupported payload encoding")
    require(len(raw) <= PAYLOAD_CAP and hashlib.sha256(raw).digest() == hex_bytes(typed(row, "payloadsha256", "bytes32"), 32), "payload checksum/size mismatch")
    return payload, raw


def validate_row(row, adapter="processor-v1", decode=True):
    require(isinstance(row, dict) and {k for k in row if k.startswith("$")} == SYSTEM, "unsupported entity system fields")
    require(row["$expiresAt"] == "never" and row["$creationFlags"] == [], "unsupported source flags/expiry")
    creator = hex_bytes(row["$creator"], 20)
    require(any(creator) and hex_bytes(row["$owner"], 20) == creator, "owner must equal nonzero creator")
    require(typed(row, "uploadschema", "str") == "sourcify-arkiv.upload.v1"
            and typed(row, "projectid", "str") == "sourcify", "unsupported upload schema/project")
    kind = typed(row, "kind", "str")
    require(kind in KINDS, "unsupported entity kind")
    sid = hex_bytes(typed(row, "stagingid", "bytes32"), 32)
    old_key = hex_bytes(row["$key"], 32)
    expected = hashlib.sha256(b"arkiv-prefill.imported-staging.v1:" + sid.hex().encode()).digest()
    require(adapter.startswith("helper-") or old_key == expected, "source key does not match staging identity")
    attrs = {k: v for k, v in row.items() if not k.startswith("$")}
    require(len(attrs) <= ATTR_CAP, "too many user attributes")
    for name, value in attrs.items():
        require(re.fullmatch(r"[A-Za-z][A-Za-z0-9._-]{0,31}", name)
                and "--" not in name and name.lower() not in RESERVED, f"invalid attribute name {name}")
        encode_value(value)
    encode_value("str:" + row["$contentType"])
    payload, raw = decode_payload(row) if decode else (base64.b64decode(row["$payload"], validate=True), b"")
    require(len(payload) <= 120000, "wire payload exceeds limit")
    return creator, kind, sid, old_key, attrs, payload, raw


def sized(raw):
    return struct.pack(">I", len(raw)) + raw


def encode_value(value, key_map=None):
    require(isinstance(value, str) and ":" in value, "invalid typed attribute")
    tag, raw = value.split(":", 1)
    if tag == "str":
        data = raw.encode()
        require(len(data) <= 128 and all(ord(c) > 31 and ord(c) != 127 for c in raw), "invalid string attribute")
        return b"\x02" + sized(data)
    if tag in ("addr", "bytes32", "key"):
        data = hex_bytes(raw, 20 if tag == "addr" else 32)
        if tag == "key" and key_map is not None:
            data = key_map(data)
        return bytes([{"addr": 4, "bytes32": 11, "key": 64}[tag]]) + data
    if tag == "bool":
        require(raw in ("true", "false"), "invalid bool attribute")
        return bytes([1, int(raw == "true")])
    if tag in ("i32", "u64"):
        require(re.fullmatch(r"-?[0-9]+", raw), "invalid integer attribute")
        value = int(raw)
        if tag == "i32":
            number(value, -(1 << 31), (1 << 31) - 1, "i32")
            return b"\x10" + (value + (1 << 31)).to_bytes(4, "big")
        number(value, 0, (1 << 64) - 1, "u64")
        return b"\x0d" + value.to_bytes(8, "big")
    raise Invalid(f"unsupported source attribute type {tag}")


def staging_identity(row, raw, db):
    attrs = {}
    for name, wire in row.items():
        if name.startswith("$") or name in WIRE_META:
            continue
        tag, value = wire.split(":", 1)
        if tag == "key":
            target = db.execute("SELECT sid FROM entities WHERE old_key=?", (hex_bytes(value, 32),)).fetchone()
            require(target is not None, "missing typed-key dependency")
            value = target[0].hex()
        elif tag == "i32":
            value = int(value)
        elif tag == "bool":
            value = value == "true"
        attrs["projectId" if name == "projectid" else name] = {"type": tag, "value": value}
    kind = typed(row, "kind", "str")
    content = {"schema": "sourcify-arkiv.staging-entity.v1", "contentType": typed(row, "originalcontenttype", "str"),
               "payloadEncoding": "hex", "payload": "0x" + raw.hex(), "attributes": attrs, "keyReferences": "staging-entity-id"}
    require(hashlib.sha256(json_bytes({"kind": kind, "content": content})).digest()
            == hex_bytes(typed(row, "stagingid", "bytes32"), 32), "semantic staging identity mismatch")


def derived_key(chain, creator, nonce):
    return keccak(chain.to_bytes(32, "big") + bytes.fromhex("44" + "00" * 18 + "44")
                  + creator + nonce.to_bytes(8, "big") + bytes(16)).digest()


def check_disk(directory, reserve):
    require(shutil.disk_usage(directory).free >= reserve, "free disk is below configured reserve")


_progress_time = 0.0
_progress_phase = None


def progress(phase, rows, force=False):
    global _progress_time, _progress_phase
    now = time.monotonic()
    if force or phase != _progress_phase or now - _progress_time >= 10:
        print(f"sourcify-prefill phase={phase} rows={rows}", file=sys.stderr, flush=True)
        _progress_time, _progress_phase = now, phase


def database(path, cache_mib):
    db = sqlite3.connect(path)
    db.execute("PRAGMA journal_mode=DELETE")
    db.execute("PRAGMA synchronous=FULL")
    db.execute("PRAGMA temp_store=FILE")
    db.execute("PRAGMA mmap_size=0")
    db.execute(f"PRAGMA cache_size=-{cache_mib * 1024}")
    db.executescript("""
      CREATE TABLE entities(seq INTEGER PRIMARY KEY,old_key BLOB UNIQUE NOT NULL,sid BLOB UNIQUE NOT NULL,
        creator BLOB NOT NULL,nonce INTEGER NOT NULL,new_key BLOB UNIQUE NOT NULL,kind TEXT NOT NULL,
        hash BLOB,part INTEGER NOT NULL,raw BLOB NOT NULL,expiry INTEGER,remaining INTEGER DEFAULT 0);
      CREATE UNIQUE INDEX entity_order ON entities(creator,nonce);
      CREATE UNIQUE INDEX locators ON entities(kind,hash,part) WHERE hash IS NOT NULL;
      CREATE TABLE creators(creator BLOB PRIMARY KEY,nonce INTEGER NOT NULL) WITHOUT ROWID;
      CREATE TABLE edges(parent INTEGER NOT NULL,dependency INTEGER NOT NULL,PRIMARY KEY(parent,dependency)) WITHOUT ROWID;
      CREATE INDEX dependency_edges ON edges(dependency,parent);
      CREATE TABLE ready(seq INTEGER PRIMARY KEY);
      CREATE TABLE contract_audit(address BLOB PRIMARY KEY,root_key BLOB UNIQUE NOT NULL) WITHOUT ROWID;
      CREATE TABLE verified_compilations(seq INTEGER PRIMARY KEY);
      CREATE TABLE selected(seq INTEGER PRIMARY KEY,done INTEGER NOT NULL);
      CREATE INDEX selected_work ON selected(done,seq);
    """)
    return db


def ingest(root, parts, report, db, args, scratch):
    counts = collections.Counter()
    attributes = collections.Counter()
    total = payload_bytes = wire_bytes = max_wire = key_references = 0
    identity_counts = [0, 0]
    for part in parts:
        digest, size = hashlib.sha256(), 0
        with open_relative(root, part["path"]) as source:
            while True:
                raw_line = source.readline(LINE_CAP + 1)
                if not raw_line:
                    break
                require(len(raw_line) <= LINE_CAP and raw_line.endswith(b"\n"), "oversized or unterminated entity line")
                digest.update(raw_line)
                size += len(raw_line)
                require(raw_line.strip(), "blank entity line")
                row = parse_json(raw_line)
                creator, kind, sid, key, attrs, payload, raw = validate_row(row, report["adapter"], decode=not args.contract_limit)
                if report["adapter"] == "helper-imported-v1":
                    synthetic = key == hashlib.sha256(b"arkiv-prefill.imported-staging.v1:" + sid.hex().encode()).digest()
                    identity_counts[int(synthetic)] += 1
                    require(not synthetic or creator == report["syntheticOwner"], "synthetic identity owner mismatch")
                nonce_row = db.execute("SELECT nonce FROM creators WHERE creator=?", (creator,)).fetchone()
                nonce = nonce_row[0] if nonce_row else 0
                db.execute("INSERT INTO creators VALUES(?,?) ON CONFLICT(creator) DO UPDATE SET nonce=excluded.nonce", (creator, nonce + 1))
                content_hash = hex_bytes(typed(row, "hash", "bytes32"), 32) if kind != "verified_contract" else None
                part_number = int(typed(row, "part", "i32")) if kind == "blob" else -1
                require(part_number >= 0 or kind != "blob", "negative blob part")
                db.execute("INSERT INTO entities(seq,old_key,sid,creator,nonce,new_key,kind,hash,part,raw) VALUES(?,?,?,?,?,?,?,?,?,?)",
                           (total, key, sid, creator, nonce, derived_key(args.chain_id, creator, nonce), kind, content_hash, part_number, raw_line))
                total += 1
                counts[kind] += 1
                attributes[len(attrs)] += 1
                payload_bytes += len(raw)
                wire_bytes += len(payload)
                max_wire = max(max_wire, len(payload))
                key_references += sum(value.startswith("key:") for value in attrs.values())
                if total % 1024 == 0:
                    db.commit()
                    check_disk(scratch, args.reserve_gib * GIB)
                    progress("ingest", total)
        require(size == part["bytes"] and digest.hexdigest() == part["sha256"], f"{part['path']}: source checksum/length mismatch")
        db.commit()
        progress("ingest", total, force=True)
    require(total == report["entities"], "entity count differs from source report")
    if report["adapter"].startswith("helper-"):
        output = report["helperOutput"]
        require(dict(counts) == output["kinds"] and wire_bytes == output.get("payloadBytes")
                and max_wire == output.get("maxPayloadBytes") and key_references == output.get("keyReferences"), "helper row inventory mismatch")
        if report["publishers"] is not None:
            require(db.execute("SELECT count(*) FROM creators").fetchone()[0] == report["publishers"], "helper publisher count mismatch")
        if report["identityCounts"] is not None:
            require(tuple(identity_counts) == report["identityCounts"], "imported identity counts mismatch")
        helper_contracts(root, report, db, args, scratch)
    require(args.contract_limit or total <= EXPIRY_CAP * (args.max_expiry - args.min_expiry + 1), "dataset exceeds finite-expiry capacity")
    return {"entities": total, "entitiesByKind": dict(sorted(counts.items())),
            "attributeCounts": {str(k): v for k, v in sorted(attributes.items())},
            "maxUserAttributes": max(attributes, default=0), "decodedPayloadBytes": payload_bytes,
            "wirePayloadBytes": wire_bytes, "inputContracts": report["contracts"],
            "sourceIdentityCounts": dict(zip(("uploaded", "synthetic"), identity_counts)) if report["adapter"] == "helper-imported-v1" else None}


def helper_contracts(root, report, db, args, scratch):
    audit = report["contractsFile"]
    digest, size, count = hashlib.sha256(), 0, 0
    with open_relative(root, audit["path"]) as source:
        while True:
            raw = source.readline(LINE_CAP + 1)
            if not raw:
                break
            require(len(raw) <= LINE_CAP and raw.endswith(b"\n"), "oversized or unterminated contract audit line")
            digest.update(raw)
            size += len(raw)
            row = parse_json(raw)
            imported = report["adapter"] == "helper-imported-v1"
            fields = {"address", "contract_id", "root_entity_key", "ordinal", "inserted_at"} if imported else {"address", "contract_id", "root_entity_key", "completed_at"}
            require(isinstance(row, dict) and set(row) == fields, "unsupported contract audit schema")
            if imported:
                require(row["ordinal"] == str(count + 1), "noncontiguous contract audit ordinal")
            address, key = hex_bytes(row["address"], 20), hex_bytes(row["root_entity_key"], 32)
            require(isinstance(row["contract_id"], str) and re.fullmatch(r"[0-9a-f]{64}", row["contract_id"])
                    and isinstance(row["inserted_at" if imported else "completed_at"], str) and row["inserted_at" if imported else "completed_at"], "invalid contract audit identity")
            found = db.execute("SELECT kind,raw FROM entities WHERE old_key=?", (key,)).fetchone()
            require(found is not None and found[0] == "verified_contract", "missing contract audit root")
            entity = parse_json(found[1])
            require(hex_bytes(typed(entity, "address", "addr"), 20) == address
                    and typed(entity, "chainid", "u64") == "1", "contract audit address/chain mismatch")
            db.execute("INSERT INTO contract_audit VALUES(?,?)", (address, key))
            if args.contract_limit and count < args.contract_limit:
                db.execute("INSERT INTO selected VALUES(?,0)", (db.execute("SELECT seq FROM entities WHERE old_key=?", (key,)).fetchone()[0],))
            count += 1
            if count % 1024 == 0:
                db.commit()
                check_disk(scratch, args.reserve_gib * GIB)
                progress("contract-audit", count)
    require(size == audit["bytes"] and digest.hexdigest() == audit["sha256"], "contract audit checksum/length mismatch")
    require(count == report["contracts"], "contract audit count mismatch")
    db.commit()
    progress("contract-audit", count, force=True)


def add_edge(db, parent, dependency):
    require(parent != dependency, "self-referencing entity")
    db.execute("INSERT OR IGNORE INTO edges VALUES(?,?)", (parent, dependency))


def resolve(db, kind, digest, part=-1):
    found = db.execute("SELECT seq,raw FROM entities WHERE kind=? AND hash=? AND part=?", (kind, hex_bytes(digest, 32), part)).fetchone()
    require(found is not None, f"missing {kind} payload dependency")
    return found


def logical_payload(db, seq, row, raw, cap):
    if typed(row, "originalcontenttype", "str") != "application/json":
        return raw
    value = parse_json(raw)
    if not (isinstance(value, dict) and set(value) == {"$spill"}):
        return raw
    spill = value["$spill"]
    require(isinstance(spill, dict) and set(spill) == {"hash", "parts", "bytes"}, "unsupported spill schema")
    size = number(spill["bytes"], PAYLOAD_CAP + 1, cap, "logical spill bytes")
    parts = number(spill["parts"], 1, (cap + 99999) // 100000, "spill parts")
    require(parts == (size + 99999) // 100000, "invalid spill dimensions")
    out = io.BytesIO()
    digest = hashlib.sha256()
    for index in range(parts):
        target, wire = resolve(db, "blob", spill["hash"], index)
        blob = parse_json(wire)
        _, chunk = decode_payload(blob)
        expected = min(100000, size - index * 100000)
        require(len(chunk) == expected and int(typed(blob, "size", "i32")) == expected
                and int(typed(blob, "parts", "i32")) == parts, "blob dimensions mismatch")
        add_edge(db, seq, target)
        digest.update(chunk)
        out.write(chunk)
    require(digest.digest() == hex_bytes(spill["hash"], 32), "spill checksum mismatch")
    return out.getvalue()


def helper_dependencies(db, seq, row, document, args):
    kind = typed(row, "kind", "str")
    if kind == "compilation":
        sources = document.get("sources")
        require(isinstance(sources, dict), "invalid helper compilation sources")
        for digest in sources.values():
            target, _ = resolve(db, "sourcefile", digest)
            add_edge(db, seq, target)
        for field in ("recompiledCreationHash", "recompiledRuntimeHash"):
            require(field in document, "missing helper compilation code locator")
            if document[field] is not None:
                target, _ = resolve(db, "code", document[field])
                add_edge(db, seq, target)
        return
    require(str(document.get("chainId")) == "1"
            and hex_bytes(document.get("address"), 20) == hex_bytes(typed(row, "address", "addr"), 20), "helper payload contract identity mismatch")
    code_refs = document.get("codeRefs")
    fields = ("creationOnchain", "creationRecompiled", "runtimeOnchain", "runtimeRecompiled")
    require(isinstance(code_refs, dict) and set(code_refs) == set(fields), "invalid helper contract code references")
    for digest in code_refs.values():
        if digest is not None:
            target, _ = resolve(db, "code", digest)
            add_edge(db, seq, target)
    found = db.execute("SELECT seq,kind,raw FROM entities WHERE old_key=?",
                       (hex_bytes(typed(row, "compilationref", "key"), 32),)).fetchone()
    require(found is not None and found[1] == "compilation", "invalid helper compilation reference")
    compilation = parse_json(found[2])
    _, raw = decode_payload(compilation)
    compiled = parse_json(logical_payload(db, found[0], compilation, raw, args.max_logical_payload_mib << 20))
    require(compiled.get("schema") == "sourcify.compilation.v2"
            and isinstance(compiled.get("sources"), dict), "unsupported helper compilation payload")
    require(compiled.get("recompiledCreationHash") == code_refs["creationRecompiled"]
            and compiled.get("recompiledRuntimeHash") == code_refs["runtimeRecompiled"], "helper compilation/code mismatch")
    # This historical fingerprint hashes the transform's ordered JSON array,
    # not the compilation payload. Preserve source/settings key insertion order.
    fingerprint = [compiled.get(field) for field in ("compiler", "compilerVersion", "language", "fullyQualifiedName")]
    fingerprint.extend([compiled.get("compilerSettings") or {}, list(map(list, compiled["sources"].items()))])
    fingerprint.extend(code_refs[field] for field in fields)
    encoded = json.dumps(fingerprint, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()
    require(hashlib.sha256(encoded).digest() == hex_bytes(typed(compilation, "hash", "bytes32"), 32), "helper compilation fingerprint mismatch")
    db.execute("INSERT OR IGNORE INTO verified_compilations VALUES(?)", (found[0],))


def dependency_rows(db, subset):
    if not subset:
        yield from db.execute("SELECT seq,raw FROM entities ORDER BY seq")
        return
    while True:
        row = db.execute("SELECT e.seq,e.raw FROM selected s JOIN entities e ON s.seq=e.seq WHERE s.done=0 ORDER BY s.seq LIMIT 1").fetchone()
        if row is None:
            return
        yield row
        db.execute("UPDATE selected SET done=1 WHERE seq=?", (row[0],))
        db.execute("INSERT OR IGNORE INTO selected SELECT dependency,0 FROM edges WHERE parent=?", (row[0],))


def dependencies(db, args, scratch, adapter="processor-v1"):
    for processed, (seq, raw_line) in enumerate(dependency_rows(db, args.contract_limit), 1):
        row = parse_json(raw_line)
        validate_row(row, adapter)
        _, raw = decode_payload(row)
        staging_identity(row, raw, db)
        for name, value in row.items():
            if not name.startswith("$") and value.startswith("key:"):
                target = db.execute("SELECT seq FROM entities WHERE old_key=?", (hex_bytes(value[4:], 32),)).fetchone()
                require(target is not None, "missing direct-key dependency")
                add_edge(db, seq, target[0])
        logical = logical_payload(db, seq, row, raw, args.max_logical_payload_mib << 20)
        kind = typed(row, "kind", "str")
        if kind == "code":
            require(keccak(logical).digest() == hex_bytes(typed(row, "hash", "bytes32"), 32), "code content hash mismatch")
        if kind in ("sourcefile", "compilation", "verified_contract"):
            document = parse_json(logical)
            schemas = {"sourcefile": "sourcify.sourcefile.v1", "compilation": "sourcify.compilation.v3", "verified_contract": "sourcify.verified-contract.v3"}
            if adapter.startswith("helper-"):
                schemas.update(compilation="sourcify.compilation.v2", verified_contract="sourcify.verifiedContract.v2")
            schema = schemas[kind]
            require(isinstance(document, dict) and document.get("schema") == schema, f"unsupported {kind} payload schema")
            if kind == "sourcefile":
                require(isinstance(document.get("content"), str) and hashlib.sha256(document["content"].encode()).digest()
                        == hex_bytes(typed(row, "hash", "bytes32"), 32), "source content hash mismatch")
            elif adapter.startswith("helper-"):
                helper_dependencies(db, seq, row, document, args)
            else:
                require(isinstance(document.get("data"), dict) and isinstance(document.get("references"), list), "invalid payload envelope")
                if kind == "compilation":
                    require(hashlib.sha256(logical).digest() == hex_bytes(typed(row, "hash", "bytes32"), 32), "compilation content hash mismatch")
                for reference in document["references"]:
                    require(isinstance(reference, dict), "invalid payload reference")
                    ref_kind = reference.get("kind")
                    if ref_kind == "local":
                        require(kind == "compilation", "local reference outside compilation")
                        continue
                    require(ref_kind in ("sourcefile", "code", "compilation"), "unsupported payload reference kind")
                    target, _ = resolve(db, ref_kind, reference.get("hash"))
                    add_edge(db, seq, target)
        if processed % 1024 == 0:
            db.commit()
            check_disk(scratch, args.reserve_gib * GIB)
            progress("dependencies", processed)
    if adapter.startswith("helper-"):
        require(db.execute("SELECT 1 FROM entities e LEFT JOIN verified_compilations v ON e.seq=v.seq WHERE e.kind='compilation' AND v.seq IS NULL AND (?=0 OR e.seq IN (SELECT seq FROM selected)) LIMIT 1", (args.contract_limit or 0,)).fetchone() is None,
                "helper compilation has no verified contract fingerprint")
    db.commit()


def select_inventory(db, args, original):
    scratch = args.out.absolute().parent
    for lower in range(0, original["entities"], 1024):
        db.execute("DELETE FROM entities WHERE seq>=? AND seq<? AND seq NOT IN (SELECT seq FROM selected)", (lower, lower + 1024))
        db.execute("UPDATE entities SET nonce=nonce+? WHERE seq>=? AND seq<?", (original["entities"], lower, lower + 1024))
        db.commit()
        check_disk(scratch, args.reserve_gib * GIB)
        progress("subset-prune", min(lower + 1024, original["entities"]))
    db.execute("DELETE FROM creators")
    # Pruning moved nonces out of the new range; now assign compact nonces.
    counts, attributes = collections.Counter(), collections.Counter()
    payload_bytes = wire_bytes = 0
    identities = collections.Counter()
    for processed, (seq, creator, raw) in enumerate(db.execute("SELECT seq,creator,raw FROM entities ORDER BY seq"), 1):
        row = parse_json(raw)
        _, kind, sid, key, attrs, payload, decoded = validate_row(row, "helper-imported-v1")
        synthetic = key == hashlib.sha256(b"arkiv-prefill.imported-staging.v1:" + sid.hex().encode()).digest()
        identities["synthetic" if synthetic else "uploaded"] += 1
        prior = db.execute("SELECT nonce FROM creators WHERE creator=?", (creator,)).fetchone()
        nonce = prior[0] if prior else 0
        # Old derived keys can collide with new ones until all are reassigned.
        db.execute("UPDATE entities SET new_key=?,nonce=? WHERE seq=?", (b"temporary:" + seq.to_bytes(8, "big"), nonce, seq))
        db.execute("INSERT INTO creators VALUES(?,?) ON CONFLICT(creator) DO UPDATE SET nonce=excluded.nonce", (creator, nonce + 1))
        counts[kind] += 1
        attributes[len(attrs)] += 1
        payload_bytes += len(decoded)
        wire_bytes += len(payload)
        if processed % 1024 == 0:
            db.commit()
            check_disk(scratch, args.reserve_gib * GIB)
            progress("subset-nonces", processed)
    db.commit()
    for processed, (seq, creator, nonce) in enumerate(db.execute("SELECT seq,creator,nonce FROM entities ORDER BY seq"), 1):
        db.execute("UPDATE entities SET new_key=? WHERE seq=?", (derived_key(args.chain_id, creator, nonce), seq))
        if processed % 1024 == 0:
            db.commit()
            check_disk(scratch, args.reserve_gib * GIB)
            progress("subset-keys", processed)
    require(counts["verified_contract"] == args.contract_limit, "closure contains extra contract roots")
    require(sum(counts.values()) <= EXPIRY_CAP * (args.max_expiry - args.min_expiry + 1), "dataset exceeds finite-expiry capacity")
    db.commit()
    return {"entities": sum(counts.values()), "entitiesByKind": dict(sorted(counts.items())),
            "attributeCounts": {str(k): v for k, v in sorted(attributes.items())},
            "maxUserAttributes": max(attributes, default=0), "decodedPayloadBytes": payload_bytes,
            "wirePayloadBytes": wire_bytes, "inputContracts": args.contract_limit,
            "sourceEntities": original["entities"], "sourceContracts": original["inputContracts"],
            "sourceIdentityCounts": original["sourceIdentityCounts"],
            "selectedIdentityCounts": {kind: identities[kind] for kind in ("uploaded", "synthetic")},
            "validationScope": "all-source-checksums-and-wire-inventory/selected-closure-semantic-validation",
            "selectionPolicy": "first-audited-contract-ordinals-and-complete-dependency-closure-v1"}


def assign_expiry(db, count, maximum):
    # parent -> dependency: start at entities nobody depends on, so a dependency
    # receives an expiry >= every parent's expiry. Disk queue tie-break is seq.
    minimum = maximum - ((count - 1) // EXPIRY_CAP) if count else maximum
    db.execute("UPDATE entities SET remaining=(SELECT count(*) FROM edges WHERE dependency=entities.seq)")
    db.execute("INSERT INTO ready SELECT seq FROM entities WHERE remaining=0")
    for rank in range(count):
        found = db.execute("SELECT seq FROM ready ORDER BY seq LIMIT 1").fetchone()
        require(found is not None, "cyclic entity dependency graph")
        seq = found[0]
        db.execute("DELETE FROM ready WHERE seq=?", (seq,))
        db.execute("UPDATE entities SET expiry=? WHERE seq=?", (minimum + rank // EXPIRY_CAP, seq))
        for (dependency,) in db.execute("SELECT dependency FROM edges WHERE parent=? ORDER BY dependency", (seq,)):
            db.execute("UPDATE entities SET remaining=remaining-1 WHERE seq=?", (dependency,))
            db.execute("INSERT OR IGNORE INTO ready SELECT seq FROM entities WHERE seq=? AND remaining=0", (dependency,))
        if rank % 1024 == 0:
            db.commit()
            progress("expiry", rank + 1)
    db.commit()
    require(db.execute("SELECT 1 FROM edges JOIN entities p ON p.seq=parent JOIN entities d ON d.seq=dependency WHERE p.expiry>d.expiry LIMIT 1").fetchone() is None, "unsafe dependency expiry")
    return minimum


def uniform_minimum(count, maximum):
    """First occupied height when count rows fill EXPIRY_CAP slots ending at maximum."""
    return maximum - ((count - 1) // EXPIRY_CAP) if count else maximum


def emit(db, count, out_dir, args, far_future_minimum=None):
    digest = keccak(bytes([len(STREAM_DOMAIN)]) + STREAM_DOMAIN)
    sha = hashlib.sha256()
    total_bytes = units = rent = 0
    def key_map(old):
        found = db.execute("SELECT new_key FROM entities WHERE old_key=?", (old,)).fetchone()
        require(found is not None, "missing reference while encoding")
        return found[0]
    with (out_dir / "prefill.bin").open("xb") as stream, (out_dir / "keys.jsonl").open("xb") as keys:
        def write(data):
            nonlocal total_bytes
            stream.write(data)
            digest.update(data)
            sha.update(data)
            total_bytes += len(data)
        write(b"ARKPFL02" + struct.pack(">IQ", 2, count))
        for i, (seq, creator, nonce, key, old_key, sid, expiry, line) in enumerate(db.execute(
                "SELECT seq,creator,nonce,new_key,old_key,sid,expiry,raw FROM entities ORDER BY creator,nonce")):
            if far_future_minimum is not None:
                # Emission order fills each height with at most EXPIRY_CAP rows.
                expiry = far_future_minimum + i // EXPIRY_CAP
            require(type(expiry) is int and 1 <= expiry <= args.max_expiry, "entity has no admissible expiry")
            row = parse_json(line)
            payload, _ = decode_payload(row)
            ctype = row["$contentType"].encode()
            attrs = sorted((name, encode_value(value, key_map)) for name, value in row.items() if not name.startswith("$"))
            frame = creator + struct.pack(">Q", nonce) + bytes(16) + struct.pack(">QB", expiry, 0)
            frame += sized(ctype) + sized(payload) + struct.pack(">I", len(attrs))
            frame += b"".join(sized(name.encode()) + value for name, value in attrs)
            require(len(frame) <= FRAME_CAP, "canonical frame exceeds limit")
            write(sized(frame))
            # Value::encoded_len excludes its type/length framing. All user
            # attributes and all system cells except payload/flags are indexed.
            system = [("$key", 32, True), ("$creator", 20, True), ("$owner", 20, True),
                      ("$createdAt", 8, True), ("$updatedAt", 8, True), ("$expiresAt", 8, True),
                      ("$creationFlags", 8, False), ("$contentType", len(ctype), True)]
            row_units = 512 + len(payload) + sum(len(n.encode()) + size + 2 + (128 if indexed else 0) for n, size, indexed in system)
            row_units += sum(len(n.encode()) + len(v) - (5 if v[0] == 2 else 1) + 2 + 128 for n, v in attrs)
            row_rent = (row_units * expiry * 8 + 17279) // 17280
            require(row_rent < 1 << 64, "row rent exceeds u64")
            units += row_units
            rent += row_rent * 1000000000
            require(units < 1 << 64 and rent < 1 << 256, "aggregate units/rent overflow")
            keys.write(json_bytes({"sourceOrdinal": seq, "sourceKey": "0x" + old_key.hex(), "stagingId": "0x" + sid.hex(),
                                   "creator": "0x" + creator.hex(), "createNonce": str(nonce), "salt": "0", "key": "0x" + key.hex(),
                                   "expiresAtHeight": str(expiry), "storageUnits": str(row_units), "rentWei": str(row_rent * 1000000000)}) + b"\n")
            if i % 1024 == 0:
                check_disk(out_dir, args.reserve_gib * GIB)
                progress("emit", i + 1)
        for file in (stream, keys):
            file.flush()
            os.fsync(file.fileno())
    return {"streamHash": "0x" + digest.hexdigest(), "prefillSha256": sha.hexdigest(),
            "canonicalBytes": total_bytes, "storageUnits": str(units), "rentWei": str(rent)}


def sync_json(path, value):
    with path.open("xb") as file:
        file.write(json_bytes(value) + b"\n")
        file.flush()
        os.fsync(file.fileno())


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def publish(stage, destination):
    # Linux no-replace rename: a racing creator cannot be overwritten, including
    # an empty directory. There is deliberately no unsafe os.rename fallback.
    libc = ctypes.CDLL(None, use_errno=True)
    rename = getattr(libc, "renameat2", None)
    require(rename is not None, "atomic no-replace directory publication requires renameat2")
    rename.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    rename.restype = ctypes.c_int
    if rename(-100, os.fsencode(stage), -100, os.fsencode(destination), 1):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(destination))
    sync_dir(destination.parent)


def run(args):
    number(args.chain_id, 1, (1 << 64) - 1, "chain ID")
    far_future = args.expiry_policy == "far-future"
    require(args.expiry_policy in ("dependency-safe", "far-future"), "unsupported expiry policy")
    if args.max_expiry is None:
        args.max_expiry = FAR_FUTURE_EXPIRY if far_future else MAX_EXPIRY
    ceiling = FAR_FUTURE_EXPIRY_LIMIT if far_future else MAX_EXPIRY
    number(args.min_expiry, 1, ceiling, "minimum expiry")
    number(args.max_expiry, args.min_expiry, ceiling, "maximum expiry")
    number(args.cache_mib, 1, 1024, "SQLite cache MiB")
    number(args.max_logical_payload_mib, 1, 256, "logical payload MiB")
    number(args.reserve_gib, 0, 1 << 20, "disk reserve GiB")
    require(keccak().hexdigest() == "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470", "Keccak provider failed self-test")
    root = args.bundle.absolute()
    require(root.is_dir() and not root.is_symlink(), "bundle must be a real directory")
    destination = args.out.absolute()
    require(not destination.exists() and not destination.is_symlink(), "output already exists")
    require(destination.parent.is_dir(), "output parent must already exist")
    check_disk(destination.parent, args.reserve_gib * GIB)
    report, parts, lock = manifest(root, args.chain_id, args.max_skipped)
    if args.contract_limit is not None:
        number(args.contract_limit, 1, report["contracts"], "contract limit")
        require(report["adapter"] == "helper-imported-v1", "contract limit requires imported helper audit ordinals")
        lock["selection"] = {"policy": "first-audited-contract-ordinals-and-complete-dependency-closure-v1", "contracts": args.contract_limit}
    with tempfile.TemporaryDirectory(prefix=".sourcify-prefill-", dir=destination.parent) as temporary:
        scratch = Path(temporary)
        staged = scratch / "published"
        staged.mkdir()
        db = database(scratch / "index.sqlite", args.cache_mib)
        try:
            inventory = ingest(root, parts, report, db, args, scratch)
            # A --contract-limit closure is discovered through dependency edges,
            # so it keeps the graph and its semantic validation in every policy.
            graph = not far_future or bool(args.contract_limit)
            if graph:
                dependencies(db, args, scratch, report["adapter"])
            if args.contract_limit:
                inventory = select_inventory(db, args, inventory)
            if far_future:
                minimum = uniform_minimum(inventory["entities"], args.max_expiry)
                require(minimum >= args.min_expiry, "dataset exceeds finite-expiry capacity")
                if not graph:
                    # Typed key references are resolved only while emitting.
                    checked = "-and-key-references" if args.command == "convert" else ""
                    inventory["validationScope"] = f"all-source-checksums-wire-inventory{checked}/no-semantic-payload-validation"
            else:
                minimum = assign_expiry(db, inventory["entities"], args.max_expiry)
            inventory.update({"schema": "arkiv-sourcify-conversion-v2", "chainId": args.chain_id,
                              "sourceLockSha256": hashlib.sha256(json_bytes(lock)).hexdigest(),
                              "sourceSkippedCount": report.get("sourceSkippedCount", 0),
                              "sourceAdapter": report["adapter"],
                              # None: no graph was built, not a claim of zero dependencies.
                              "dependencyEdges": db.execute("SELECT count(*) FROM edges").fetchone()[0] if graph else None,
                              "expiryPolicy": "far-future-uniform-v1" if far_future else "dependency-safe-v1",
                              "expiryPerHeight": EXPIRY_CAP,
                              "minimumExpiryHeight": minimum, "maximumExpiryHeight": args.max_expiry,
                              "keyPolicy": "source-order-per-creator/nonce-from-zero/salt-zero-v1",
                              "maxTotalCells": 64, "maxUserAttributesSupported": ATTR_CAP,
                              "bounds": {"sqliteCacheMiB": args.cache_mib, "lineBytes": LINE_CAP,
                                         "logicalPayloadMiB": args.max_logical_payload_mib}})
            if args.command == "convert":
                inventory.update(emit(db, inventory["entities"], staged, args, minimum if far_future else None))
            sync_json(staged / "source-lock.json", lock)
            sync_json(staged / "conversion-report.json", inventory)
            sync_dir(staged)
        finally:
            db.close()
        publish(staged, destination)
    return inventory


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("command", choices=("inventory", "convert"))
    result.add_argument("--bundle", type=Path, required=True)
    result.add_argument("--out", type=Path, required=True, help="new directory; never overwritten")
    result.add_argument("--chain-id", type=int, default=7706815)
    result.add_argument("--expiry-policy", choices=("dependency-safe", "far-future"), default="dependency-safe",
                        help="dependency-safe: graph-ordered expiries up to 6307200; far-future: uniform expiries "
                             "ending at --max-expiry without a dependency graph (needs base genesis max_lifetime >= it)")
    result.add_argument("--min-expiry", type=int, default=1, help="minimum acceptable dataset lifetime in blocks")
    result.add_argument("--max-expiry", type=int, default=None,
                        help=f"last expiry height (default {MAX_EXPIRY}, or {FAR_FUTURE_EXPIRY} for far-future)")
    result.add_argument("--max-skipped", type=int, default=5)
    result.add_argument("--contract-limit", type=int, help="first N imported audit ordinals, retaining complete dependency closure")
    result.add_argument("--cache-mib", type=int, default=64)
    result.add_argument("--max-logical-payload-mib", type=int, default=64)
    result.add_argument("--reserve-gib", type=int, default=80)
    return result


if __name__ == "__main__":
    try:
        print(json.dumps(run(parser().parse_args()), sort_keys=True))
    except (Invalid, OSError, sqlite3.Error, ValueError, KeyError, TypeError, RecursionError, zlib.error) as error:
        print(f"sourcify-prefill: {error}", file=sys.stderr)
        sys.exit(1)
