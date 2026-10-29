#!/usr/bin/env python3
"""Convert a v4 (UDC) Sourcify bundle into a row-codec-3 streaming prefill.

A v4 bundle is what the Sourcify processor's `process --format udc` writes
(sourcify-arkiv-entity-processor, docs/udc-proposal.md): seed.json version 2
(`sourcify-arkiv.udc-bundle.v1`), report.json (`sourcify-udc-import.v1`),
SHA256SUMS and entities/NNNNNN.jsonl, one UDC entity per line in creation order:

  {"nonce":N,"key":"0x..","expiresAt":"0xffffffffffffffff",
   "attributes":[{"name","type","indexed","value"}, ...]}

Every line becomes exactly one prefill row, in the same order, with nothing to
reinterpret (there is no payload):

  prefill.bin = "ARKPFL02" || u32(3) || u64(rows) || rows x (u32(len) || row)
  row         = creator[20] || nonce u64 || salt u128 || expiry u64 || flags u8
                || u32(count) || count x (u32(len) || name || kind u8 || tag u8 || payload)

creator = the bundle owner (or --owner), creation nonce = the line's nonce
(0..N-1), salt 0, expiry u64::MAX (a permanent-prefill launch, versions 3/4,
rent sponsored), flags 0. Each attribute is a cell of kind 01 (indexed) or 02
(stored) whose value carries the engine tag: bool 01, str 02, bytes 03, addr 04,
bytes32 0b, u64 0d, i32 10, key 40 (sign bit flipped for i32), names strictly
ascending by their bytes.

Before anything is published the converter checks, line by line:

- the bundle: SHA256SUMS, seed.json and report.json (format, chain, owner,
  prefix, firstNonce 0, the source-complete policy), every part's SHA-256, size,
  entity count and first nonce, and at the end the report's entity, kind,
  contract and distinct counts, bytes values and storage units;
- the node's row rules: at most 32 attributes, SDK-safe names strictly
  ascending, only the format's SDK types, str of at most 128 bytes without
  controls, bytes unindexed and at most 128 KiB per value and per row;
- the format: the 120,000-byte budget per entity, `arkiv.schema` = prefix.kind,
  each kind's attributes, types, indexed flags and required set, the key of each
  line = keccak(chain || 0x44..44 || owner || nonce || salt 0), permanence, one
  contract per address and matchid, one entity per kind and content hash, the
  codec byte of every bytes value, `sourcerefs` inline;
- references: every key a row names (key attributes, `sourcerefs`, chunk lists)
  is an earlier row of the right kind;
- content: chunk lists (length, chunk count, SHA-256 of the joined value), the
  source's SHA-256, the code's Keccak-256, the compilation's manifest hash,
  canonical manifest, group digests, source and code hashes behind sourcerefs
  and coderefs, a contract's compilationhash, code hashes (also of literal
  on-chain bytecode) and abihash (when no reference points into the ABI).

- decoding: every bytes value decodes, each compilation group and the contract
  data parse as JSON, source text is UTF-8, manifest references are as the
  reader requires. Left to the processor's hydrate: abihash behind a reference
  into the ABI (counted) and the contract attributes that project its record.

convert needs --chain-id, the chain of the base genesis: rows do not say which
chain derives the keys they name. A chain or --owner other than the bundle's
re-keys the dataset: every key is recomputed from (chain, owner, nonce, 0), in
the key attributes, the sourcerefs and the chunk lists. Content hashes and
digests do not change.

Output (a new directory, published with an atomic no-replace rename):
prefill.bin, conversion-report.json and source-lock.json. The converter writes
nothing else and starts no node. Its memory is a key index and content-hash set
of the referenceable rows (sources, codes, compilations, chunks): about 430 bytes
each, about 5.4 GB for the full Sourcify corpus, plus 2 x --workers part files in
flight. Chunk data waits in a scratch file next to the output until it is joined.
An interrupted run leaves a hidden .sourcify-v4-prefill-* directory in the
output's parent, never a published output; delete it and run again.
"""

import argparse
import base64
import concurrent.futures
import ctypes
import hashlib
import json
import multiprocessing
import os
from pathlib import Path, PurePosixPath
import re
import resource
import shutil
import stat
import struct
import sys
import tempfile
import time
import zlib


# Contract data may hold integers longer than Python's default 4,300-digit parse limit.
sys.set_int_max_str_digits(0)
GIB = 1 << 30
LINE_CAP = 1 << 20
MANIFEST_CAP = 64 << 20
FRAME_CAP = 8 << 20
STREAM_ROW_LIMIT = 100915200
STREAM_DOMAIN = b"arkiv/prefill-stream/v2"
ROW_CODEC = 3
PERMANENT = (1 << 64) - 1
# The node's rules (arkiv-node src/arkiv/schema.rs, genesis_streaming.rs).
MAX_ATTRIBUTES = 32
MAX_STR_BYTES = 128
MAX_BYTES_LENGTH = 128 * 1024
MAX_ENTITY_BYTES = 128 * 1024
# Storage units of an entity's seven system cells and its fixed overhead
# (arkiv-node gas::EMPTY_ENTITY_UNITS), and of one index posting.
EMPTY_ENTITY_UNITS = 1460
INDEX_UNITS = 128
RESERVED = {"and", "or", "not", "true", "false", "startswith", "exists", "typeof", "bool", "i32", "u64",
            "u256", "dec", "bytes32", "bytes", "str", "addr", "key"}
# The format (sourcify-arkiv-entity-processor src/udc.rs).
SEED_VERSION = 2
FORMAT = "sourcify-arkiv.udc-bundle.v1"
REPORT_SCHEMA = "sourcify-udc-import.v1"
BUDGET = 120_000
CHUNK = 120_000
MAX_VALUE = 64 << 20
IDENTITY, DEFLATE, CHUNKED = 0, 1, 2
KINDS = ("contract", "compilation", "source", "code", "chunk")
CONTRACT, COMPILATION, SOURCE, CODE, CHUNK_KIND = range(5)
# Engine tags of the SDK types the format uses (arkiv-db TypeTag ids).
TAGS = {"bool": 0x01, "str": 0x02, "bytes": 0x03, "addr": 0x04, "bytes32": 0x0B, "u64": 0x0D,
        "i32": 0x10, "key": 0x40}
# kind, name, type, indexed, required (udc::ATTRIBUTES); every kind also has an
# indexed str arkiv.schema.
ATTRIBUTES = (
    ("contract", "abihash", "bytes32", True, True),
    ("contract", "address", "addr", True, True),
    ("contract", "blocknumber", "u64", True, True),
    ("contract", "chainid", "u64", True, True),
    ("contract", "compilationhash", "bytes32", True, True),
    ("contract", "compilationref", "key", False, True),
    ("contract", "compilerversion", "str", True, True),
    ("contract", "creationcodehash", "bytes32", True, False),
    ("contract", "creationcoderef", "key", False, False),
    ("contract", "data", "bytes", False, True),
    ("contract", "deployer", "addr", True, True),
    ("contract", "evmversion", "str", True, True),
    ("contract", "isproxy", "bool", True, False),
    ("contract", "language", "str", True, True),
    ("contract", "match", "str", True, True),
    ("contract", "matchid", "u64", True, True),
    ("contract", "name", "str", True, True),
    ("contract", "optimizer", "bool", True, True),
    ("contract", "optimizerruns", "i32", True, True),
    ("contract", "proxytype", "str", True, False),
    ("contract", "runtimecodehash", "bytes32", True, False),
    ("contract", "runtimecoderef", "key", False, False),
    ("contract", "verifiedat", "u64", True, True),
    ("contract", "viair", "bool", True, True),
    ("compilation", "abi", "bytes", False, True),
    ("compilation", "artifacts", "bytes", False, False),
    ("compilation", "compilation", "bytes", False, True),
    ("compilation", "creationcoderef", "key", False, False),
    ("compilation", "hash", "bytes32", True, True),
    ("compilation", "manifest", "bytes", False, True),
    ("compilation", "runtimecoderef", "key", False, False),
    ("compilation", "sourcerefs", "bytes", False, False),
    ("source", "content", "bytes", False, True),
    ("source", "hash", "bytes32", True, True),
    ("code", "code", "bytes", False, True),
    ("code", "hash", "bytes32", True, True),
    ("chunk", "data", "bytes", False, True),
)
SCHEMA = {}
for _kind, _name, _type, _indexed, _required in ATTRIBUTES:
    SCHEMA.setdefault(_kind, {})[_name] = (_type, _indexed, _required)
# The kind a key attribute names (udc::key_target).
KEY_TARGETS = {"compilationref": COMPILATION, "creationcoderef": CODE, "runtimecoderef": CODE}
GROUPS = ("abi", "compilation", "artifacts")
SLOTS = ("creation", "runtime")
ONCHAIN = (("creationBytecode", "creationcoderef", "creationcodehash"),
           ("runtimeBytecode", "runtimecoderef", "runtimecodehash"))
ARKIV_ADDRESS = bytes.fromhex("44" + "00" * 18 + "44")
LINE_KEYS = {"nonce", "key", "expiresAt", "attributes"}
ATTRIBUTE_KEYS = {"name", "type", "indexed", "value"}
SEED_KEYS = {"version", "format", "chainId", "owner", "schemaPrefix", "firstNonce", "entities"}
PART_PATH = re.compile(r"entities/[0-9]{6}\.jsonl")
HEX32 = re.compile(r"0x[0-9a-fA-F]{64}")
HEX20 = re.compile(r"0x[0-9a-fA-F]{40}")
QUANTITY = re.compile(r"0x(0|[1-9a-fA-F][0-9a-fA-F]*)")
NAME = re.compile(r"[A-Za-z][A-Za-z0-9._-]*")
PREFIX = re.compile(r"[A-Za-z0-9._-]{1,116}")


class Invalid(ValueError):
    pass


class NeedsChunks(Exception):
    """A value is a chunk list: its row is checked where the chunks are."""


def require(condition, message):
    if not condition:
        raise Invalid(message)


def keccak(data=b""):
    try:
        return hashlib.new("KECCAK-256", data)
    except ValueError as error:
        raise Invalid("Python/OpenSSL must expose KECCAK-256 (OpenSSL 3.5+); SHA3-256 is not interchangeable") from error


def entity_key(chain_id, owner, nonce, salt=0):
    return keccak(chain_id.to_bytes(32, "big") + ARKIV_ADDRESS + owner + nonce.to_bytes(8, "big")
                  + salt.to_bytes(16, "big")).digest()


def hx(data):
    return "0x" + data.hex()


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        require(key not in obj, f"duplicate JSON member {key}")
        obj[key] = value
    return obj


def no_constant(value):
    raise Invalid(f"invalid JSON number {value}")


def strict_int(text):
    require(text != "-0", "invalid JSON number -0")
    return int(text)


def json_text(raw):
    """UTF-8 text of a JSON document, as serde_json reads it: no BOM, no other encoding."""
    if isinstance(raw, str):
        return raw
    try:
        return bytes(raw).decode("utf-8")
    except UnicodeDecodeError as error:
        raise Invalid(f"JSON is not UTF-8: {error}") from error


def parse_json(raw):
    """A bundle or manifest document: duplicate members, -0, NaN and Infinity refused."""
    return json.loads(json_text(raw), object_pairs_hook=unique_object, parse_constant=no_constant,
                      parse_int=strict_int)


def parse_value(raw, label):
    """A stored JSON value (contract data, compilation groups) that the reader parses."""
    try:
        return json.loads(json_text(raw))
    except (ValueError, RecursionError) as error:
        raise Invalid(f"{label} is not JSON: {error}") from error


def utf16_key(text):
    return text.encode("utf-16-be")


def canonical_bytes(value):
    """The processor's json_bytes: object keys in UTF-16 order, compact, raw UTF-8."""
    def canonical(v):
        if isinstance(v, dict):
            return {k: canonical(v[k]) for k in sorted(v, key=utf16_key)}
        if isinstance(v, list):
            return [canonical(x) for x in v]
        return v
    return json.dumps(canonical(value), ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()


def sized(data):
    return struct.pack(">I", len(data)) + data


def sdk_safe_name(name):
    return (0 < len(name) <= 32 and NAME.fullmatch(name) is not None and "--" not in name
            and name.lower() not in RESERVED)


def hex_value(text, size, label):
    require(isinstance(text, str) and (HEX32 if size == 32 else HEX20).fullmatch(text), f"invalid {label}")
    return bytes.fromhex(text[2:])


def decode_value(value):
    """An identity or raw-DEFLATE bytes value (udc::decode); a chunk list raises NeedsChunks."""
    require(len(value) > 0, "empty bytes value")
    codec = value[0]
    if codec == IDENTITY:
        require(len(value) - 1 <= MAX_VALUE, "value exceeds limit")
        return value[1:]
    if codec == DEFLATE:
        inflater = zlib.decompressobj(-15)
        try:
            out = inflater.decompress(value[1:], MAX_VALUE + 1)
        except zlib.error as error:
            raise Invalid(f"invalid DEFLATE value: {error}") from error
        require(len(out) <= MAX_VALUE and not inflater.unconsumed_tail, "decoded value exceeds limit")
        require(inflater.eof and not inflater.unused_data, "DEFLATE value is truncated or has trailing bytes")
        return out
    if codec == CHUNKED:
        raise NeedsChunks()
    raise Invalid(f"unknown codec {codec}")


def chunk_list(value):
    """`02 || sha256(joined) || u32 length || keys` (udc::chunk_manifest)."""
    require(len(value) >= 37 and (len(value) - 37) % 32 == 0, "malformed chunk list")
    length = int.from_bytes(value[33:37], "big")
    require(0 < length <= MAX_VALUE + 1, "chunk list length out of range")
    keys = [bytes(value[i:i + 32]) for i in range(37, len(value), 32)]
    require(len(keys) == -(-length // CHUNK), "chunk count mismatch")
    return bytes(value[1:33]), length, keys


def parse_attribute(a):
    require(isinstance(a, dict) and set(a) == ATTRIBUTE_KEYS, "attribute must have exactly name, type, indexed and value")
    name, kind, indexed, raw = a["name"], a["type"], a["indexed"], a["value"]
    require(isinstance(name, str) and sdk_safe_name(name), f"invalid attribute name {name!r}")
    require(type(indexed) is bool, f"{name}: indexed must be a bool")
    require(kind in TAGS, f"{name}: unsupported attribute type {kind!r}")
    if kind == "bool":
        require(type(raw) is bool, f"{name}: bool value")
        payload = bytes([raw])
    elif kind == "i32":
        require(type(raw) is int and -(1 << 31) <= raw < (1 << 31), f"{name}: i32 value")
        payload = (raw + (1 << 31)).to_bytes(4, "big")
    elif kind == "u64":
        require(isinstance(raw, str) and QUANTITY.fullmatch(raw), f"{name}: u64 must be a compact 0x quantity")
        number = int(raw[2:], 16)
        require(number < 1 << 64, f"{name}: u64 out of range")
        raw = number
        payload = number.to_bytes(8, "big")
    elif kind in ("bytes32", "key"):
        raw = payload = hex_value(raw, 32, name)
    elif kind == "addr":
        raw = payload = hex_value(raw, 20, name)
    elif kind == "str":
        require(isinstance(raw, str), f"{name}: str value")
        data = raw.encode("utf-8")
        require(len(data) <= MAX_STR_BYTES, f"{name}: str longer than 128 bytes")
        require(not any(c <= "\x1f" or c == "\x7f" for c in raw), f"{name}: str with control characters")
        payload = sized(data)
    else:  # bytes, base64 in the bundle
        require(isinstance(raw, str), f"{name}: bytes must be base64")
        try:
            data = base64.b64decode(raw, validate=True)
        except ValueError as error:
            raise Invalid(f"{name}: invalid base64") from error
        require(base64.b64encode(data).decode() == raw, f"{name}: noncanonical base64")
        require(not indexed, f"{name}: bytes cannot be indexed")
        require(len(data) <= MAX_BYTES_LENGTH, f"{name}: bytes value exceeds 128 KiB")
        raw = data
        payload = sized(data)
    return name, kind, indexed, raw, payload


def encoded_len(kind, value):
    """Value::encoded_len: the bytes storage units count."""
    if kind in ("str",):
        return len(value.encode("utf-8"))
    if kind == "bytes":
        return len(value)
    return {"bool": 1, "i32": 4, "u64": 8, "addr": 20, "bytes32": 32, "key": 32}[kind]


def check_schema(prefix, attrs):
    """udc::check_schema: the kind under the prefix, each attribute's type and flag, the required set."""
    by_name = {a[0]: a for a in attrs}
    schema = by_name.get("arkiv.schema")
    require(schema is not None and schema[1] == "str", "no arkiv.schema")
    require(schema[2], "arkiv.schema must be indexed")
    tag = schema[3]
    require(tag.startswith(prefix + ".") and tag[len(prefix) + 1:] in KINDS, f"arkiv.schema {tag} is not a {prefix} kind")
    kind = tag[len(prefix) + 1:]
    spec = SCHEMA[kind]
    for name, kind_type, indexed, _value, _payload in attrs:
        if name == "arkiv.schema":
            continue
        require(name in spec, f"{kind} has no attribute {name}")
        require(spec[name][0] == kind_type and spec[name][1] == indexed,
                f"{kind} attribute {name} must be {'indexed ' if spec[name][1] else 'stored '}{spec[name][0]}")
    for name, (_t, _i, required) in spec.items():
        require(not required or name in by_name, f"{kind} is missing {name}")
    return KINDS.index(kind)


class Content:
    """Decoded bytes values of one row; chunk lists are joined by `join` (None: defer)."""

    def __init__(self, values, join=None):
        self.values = values
        self.join = join
        self.cache = {}

    def get(self, name):
        if name not in self.values:
            return None
        if name not in self.cache:
            raw = self.values[name]
            if raw[0] == CHUNKED:
                if self.join is None:
                    raise NeedsChunks()
                joined = self.join(raw)
                require(joined[:1] != bytes([CHUNKED]), f"{name}: a chunk list of a chunk list")
                self.cache[name] = decode_value(joined)
            else:
                self.cache[name] = decode_value(raw)
        return self.cache[name]


def within(path, parent):
    """hydrate::within: `path` is `parent` or below it."""
    return path == parent or (path.startswith(parent) and path[len(parent):].startswith("/"))


def valid_pointer(text):
    """hydrate::validate_pointer (no root): `/` first, `~` only as `~0` or `~1`."""
    if not isinstance(text, str) or not text.startswith("/"):
        return False
    i = 0
    while i < len(text):
        if text[i] == "~":
            if i + 1 >= len(text) or text[i + 1] not in "01":
                return False
            i += 1
        i += 1
    return True


def check_refs(refs, source_count, code_slots):
    """The manifest's references as udc.rs Compilation::load checks them; True if one is in /abi."""
    require(isinstance(refs, list), "manifest refs")
    paths = set()
    abi_refs = False
    for r in refs:
        require(isinstance(r, dict) and isinstance(r.get("path"), str), "manifest reference path")
        path = r["path"]
        require(valid_pointer(path), f"invalid reference path {path!r}")
        require(path not in paths, f"duplicate reference path {path!r}")
        paths.add(path)
        abi_refs = abi_refs or within(path, "/abi")
        kind = r.get("kind")
        if kind == "local":
            target = r.get("pointer")
            require(valid_pointer(target), "invalid local pointer")
            std = lambda q: within(q, "/stdJsonInput") or within(q, "/stdJsonOutput")
            require(std(path) and not std(target), "invalid or cyclic local reference")
        elif kind == "source":
            require(type(r.get("source")) is int and 0 <= r["source"] < source_count, "source index out of range")
        elif kind == "code":
            require(r.get("code") in code_slots, "unknown code slot")
        else:
            raise Invalid(f"unsupported reference kind {kind!r}")
    for path in paths:
        parent = path
        while "/" in parent:
            parent = parent.rsplit("/", 1)[0]
            require(parent not in paths, "overlapping references")
    return abi_refs


HEX_TEXT = re.compile(r"(?:[0-9a-fA-F]{2})*")


def trim_0x(text):
    while text.startswith("0x"):
        text = text[2:]
    return text


def pointer(document, path):
    for part in path.strip("/").split("/"):
        if not isinstance(document, dict) or part not in document:
            return None
        document = document[part]
    return document


def content_info(kind, plain, values, join=None):
    """The content checks one row can make alone, and what later rows need from it.

    Returns the kind's info: contract (address, matchid, compilationhash, abihash, code hashes),
    compilation (source hashes, code hashes by slot, ABI digest or None), chunk data, or None.
    """
    content = Content(values, join)
    if kind != CHUNK_KIND:
        # Every value is decoded (chunk lists joined and checked), whether or not a check reads it.
        for name in values:
            if name != "sourcerefs":
                content.get(name)
    if kind == SOURCE:
        text = content.get("content")
        require(hashlib.sha256(text).digest() == plain["hash"], "source hash mismatch")
        try:
            bytes(text).decode("utf-8")
        except UnicodeDecodeError as error:
            raise Invalid("source content is not UTF-8") from error
        return None
    if kind == CODE:
        require(keccak(content.get("code")).digest() == plain["hash"], "code hash mismatch")
        return None
    if kind == CHUNK_KIND:
        data = values["data"]
        require(0 < len(data) <= CHUNK, "chunk size out of range")
        return data
    if kind == COMPILATION:
        manifest_bytes = content.get("manifest")
        require(hashlib.sha256(manifest_bytes).digest() == plain["hash"], "compilation hash mismatch")
        try:
            manifest = parse_json(manifest_bytes)
        except (ValueError, UnicodeDecodeError) as error:
            raise Invalid(f"invalid manifest: {error}") from error
        require(isinstance(manifest, dict) and set(manifest) == {"codes", "digests", "refs", "sources"},
                "manifest must have exactly codes, digests, refs and sources")
        require(canonical_bytes(manifest) == manifest_bytes, "manifest is not canonical")
        digests, codes, refs, sources = manifest["digests"], manifest["codes"], manifest["refs"], manifest["sources"]
        require(isinstance(digests, dict) and set(digests) <= set(GROUPS), "invalid manifest digests")
        for group in GROUPS:
            present = group in values
            require(present == (group in digests), f"group {group} and its digest disagree")
            if present:
                raw = content.get(group)
                require(digests[group] == hx(hashlib.sha256(raw).digest()), f"group {group} digest mismatch")
                parse_value(raw, f"group {group}")
        require(isinstance(sources, list), "manifest sources")
        source_hashes = [hex_value(s, 32, "source hash") for s in sources]
        sourcerefs = values.get("sourcerefs")
        require((sourcerefs is not None) == bool(source_hashes), "sourcerefs do not match the manifest")
        if sourcerefs is not None:
            require(len(sourcerefs) - 1 == 32 * len(source_hashes), "sourcerefs do not match the manifest")
        require(isinstance(codes, dict) and set(codes) <= set(SLOTS), "invalid manifest codes")
        code_hashes = {}
        for slot in SLOTS:
            require((slot in codes) == (f"{slot}coderef" in plain), f"code slot {slot} and its coderef disagree")
            if slot in codes:
                code_hashes[slot] = hex_value(codes[slot], 32, "code hash")
        abi_refs = check_refs(refs, len(source_hashes), code_hashes)
        # abihash is over the hydrated ABI; without references into it, that is the stored group.
        # With one, only hydration can check it (the converter counts those contracts).
        return source_hashes, code_hashes, None if abi_refs else bytes.fromhex(digests["abi"][2:])
    # contract
    data = parse_value(content.get("data"), "contract data")
    require(isinstance(data, dict), "contract data must be an object")
    for lane, ref, hash_name in ONCHAIN:
        if ref in plain:
            require(hash_name in plain, f"code reference {ref} without {hash_name}")
        elif hash_name in plain:
            # Without a reference the code hash describes literal on-chain bytecode.
            literal = pointer(data, f"/{lane}/onchainBytecode")
            require(isinstance(literal, str), f"{hash_name} without code reference or literal")
            literal = trim_0x(literal)
            require(HEX_TEXT.fullmatch(literal) is not None, "literal on-chain bytecode is not hex")
            require(keccak(bytes.fromhex(literal)).digest() == plain[hash_name], "literal code hash mismatch")
    require(plain["chainid"] == 1, "contract of another source chain")
    return (plain["address"], plain["matchid"], plain["compilationhash"], plain["abihash"],
            plain.get("creationcodehash"), plain.get("runtimecodehash"))


def scan_line(line, nonce, chain_id, owner, prefix, creator):
    """One bundle line: the node's and the format's row rules, its frame and what main checks.

    `chain_id` and `owner` are the bundle's (they derive the line's key); `creator` is the row's.
    """
    try:
        row = parse_json(line)
    except (ValueError, UnicodeDecodeError) as error:
        raise Invalid(f"invalid JSON: {error}") from error
    require(isinstance(row, dict) and set(row) == LINE_KEYS, "line must have exactly nonce, key, expiresAt and attributes")
    require(type(row["nonce"]) is int and row["nonce"] == nonce, f"nonce {row['nonce']}, expected {nonce}")
    key = hex_value(row["key"], 32, "key")
    require(key == entity_key(chain_id, owner, nonce), f"key is not the salt-0 key of nonce {nonce}")
    require(row["expiresAt"] == "0xffffffffffffffff", "entity is not permanent")
    raw_attrs = row["attributes"]
    require(isinstance(raw_attrs, list) and len(raw_attrs) <= MAX_ATTRIBUTES, "more than 32 attributes")
    attrs = [parse_attribute(a) for a in raw_attrs]
    names = [a[0].encode() for a in attrs]
    require(all(a < b for a, b in zip(names, names[1:])), "attributes not strictly ascending")
    kind = check_schema(prefix, attrs)
    bytes_len = sum(len(a[3]) for a in attrs if a[1] == "bytes")
    require(bytes_len <= MAX_ENTITY_BYTES, "entity bytes exceed 128 KiB")
    require(bytes_len <= BUDGET, "entity exceeds the 120,000-byte budget")
    plain, values = {}, {}
    for name, kind_type, _indexed, value, _payload in attrs:
        if kind_type == "bytes":
            require(len(value) > 0, f"empty bytes value {name}")
            require(kind == CHUNK_KIND or value[0] <= CHUNKED, f"bytes value {name} has unknown codec {value[0]}")
            values[name] = value
        else:
            plain[name] = value
    if "sourcerefs" in values:
        body = values["sourcerefs"]
        require(body[0] == IDENTITY and len(body) > 1 and (len(body) - 1) % 32 == 0,
                "malformed sourcerefs (must be codec 0 and 32-byte keys)")
    # The frame, with the offset of every key it names (for the reference check and re-keying).
    head = creator + struct.pack(">Q", nonce) + bytes(16) + struct.pack(">QB", PERMANENT, 0) + struct.pack(">I", len(attrs))
    parts, refs, offset, units = [head], [], len(head), EMPTY_ENTITY_UNITS
    chunked = False
    for name, kind_type, indexed, value, payload in attrs:
        encoded_name = name.encode()
        cell = sized(encoded_name) + bytes([1 if indexed else 2, TAGS[kind_type]]) + payload
        value_at = offset + 4 + len(encoded_name) + 2 + (4 if kind_type in ("str", "bytes") else 0)
        if kind_type == "key":
            require(name in KEY_TARGETS, f"unknown key attribute {name}")
            refs.append((value_at, value, KEY_TARGETS[name], name))
        elif kind_type == "bytes" and kind != CHUNK_KIND:
            if name == "sourcerefs":
                refs.extend((value_at + 1 + i, value[1 + i:33 + i], SOURCE, name) for i in range(0, len(value) - 1, 32))
            elif value[0] == CHUNKED:
                _digest, _length, keys = chunk_list(value)
                refs.extend((value_at + 37 + 32 * i, k, CHUNK_KIND, name) for i, k in enumerate(keys))
                chunked = True
        units += len(encoded_name) + encoded_len(kind_type, value) + 2 + (INDEX_UNITS if indexed else 0)
        parts.append(cell)
        offset += len(cell)
    frame = b"".join(parts)
    require(len(frame) <= FRAME_CAP, "canonical frame exceeds 8 MiB")
    if chunked:
        info, deferred = None, (plain, values)
    else:
        info, deferred = content_info(kind, plain, values), None
    content_hash = plain.get("hash")
    return frame, (nonce, key, kind, content_hash, refs, info, deferred, units, bytes_len, len(attrs))


def open_relative(root, relative):
    """Walk directory fds without following a symlink at any component."""
    p = PurePosixPath(relative)
    require(isinstance(relative, str) and relative and not p.is_absolute()
            and all(part not in ("", ".", "..") for part in relative.split("/")), "unsafe bundle path")
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in p.parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        result = os.open(p.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
        if not stat.S_ISREG(os.fstat(result).st_mode):
            os.close(result)
            raise Invalid(f"{relative} must be a regular file")
        return os.fdopen(result, "rb")
    finally:
        os.close(fd)


def scan_part(task):
    """One part file: its digest, size and lines, as frames and row summaries for main."""
    root, part, chain_id, owner, prefix, creator = task
    digest, size, count, longest = hashlib.sha256(), 0, 0, 0
    frames, rows = [], []
    at = 0
    with open_relative(root, part["path"]) as stream:
        while True:
            line = stream.readline(LINE_CAP + 1)
            if not line:
                break
            require(len(line) <= LINE_CAP, f"{part['path']} line {count + 1} exceeds 1 MiB")
            require(line.endswith(b"\n"), f"{part['path']}: last line unterminated")
            digest.update(line)
            size += len(line)
            try:
                frame, summary = scan_line(line, part["firstNonce"] + count, chain_id, owner, prefix, creator)
            except (Invalid, NeedsChunks, ValueError, KeyError, TypeError, zlib.error) as error:
                raise Invalid(f"{part['path']} line {count + 1}: {error}") from None
            # Offsets become positions in this part's frame blob.
            base = at + 4
            summary = summary[:4] + ([(base + o, k, t, n) for o, k, t, n in summary[4]],) + summary[5:]
            frames.append(sized(frame))
            at += 4 + len(frame)
            longest = max(longest, len(frame))
            rows.append(summary)
            count += 1
    return digest.hexdigest(), size, count, longest, b"".join(frames), rows


def read_small(root, name):
    with open_relative(root, name) as stream:
        raw = stream.read(MANIFEST_CAP + 1)
    require(len(raw) <= MANIFEST_CAP, f"{name} exceeds the metadata limit")
    return raw


def manifest(root, allow_eof_source):
    """SHA256SUMS, seed.json and report.json, checked against each other (udc_bundle::manifest)."""
    sums_raw = read_small(root, "SHA256SUMS")
    sums = {}
    for line in sums_raw.decode().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
        require(match is not None, "malformed SHA256SUMS")
        digest, path = match.groups()
        require(path not in sums, f"{path} listed twice in SHA256SUMS")
        sums[path] = digest
    documents, lock = {}, [{"path": "SHA256SUMS", "bytes": len(sums_raw), "sha256": hashlib.sha256(sums_raw).hexdigest()}]
    for name in ("seed.json", "report.json"):
        raw = read_small(root, name)
        digest = hashlib.sha256(raw).hexdigest()
        require(sums.get(name) == digest, f"{name}: checksum mismatch")
        documents[name] = parse_json(raw)
        lock.append({"path": name, "bytes": len(raw), "sha256": digest})
    seed, report = documents["seed.json"], documents["report.json"]
    require(isinstance(seed, dict) and set(seed) == SEED_KEYS and type(seed["version"]) is int
            and seed["version"] == SEED_VERSION and seed["format"] == FORMAT, f"seed.json is not a version-2 {FORMAT} manifest")
    require(isinstance(report, dict) and report.get("schema") == REPORT_SCHEMA and report.get("status") == "completed"
            and report.get("format") == FORMAT, f"report.json is not a completed {REPORT_SCHEMA} report")
    chain_id = seed["chainId"]
    require(type(chain_id) is int and 0 < chain_id < 1 << 64, "seed chainId")
    owner = hex_value(seed["owner"], 20, "seed owner")
    require(owner != bytes(20), "seed owner must be nonzero")
    prefix = seed["schemaPrefix"]
    require(isinstance(prefix, str) and PREFIX.fullmatch(prefix), "invalid schema prefix")
    require(type(report.get("arkivChainId")) is int and report["arkivChainId"] == chain_id and isinstance(report.get("owner"), str)
            and report["owner"].lower() == seed["owner"].lower() and report.get("schemaPrefix") == prefix,
            "report.json describes another chain, owner or prefix than seed.json")
    require(type(report.get("sourceChainId")) is int and report["sourceChainId"] == 1,
            "the bundle's source chain must be Ethereum mainnet (1)")
    require(type(seed["firstNonce"]) is int and seed["firstNonce"] == 0, "seed.json firstNonce must be 0")
    require(report.get("expiry") == "permanent (expiresAt 2^64-1)", "the bundle's entities must be permanent")
    require(report.get("sourceCompleteRequired") is True or allow_eof_source,
            "the bundle was processed without a source-complete marker (--allow-eof-source accepts it)")
    listed, audits = seed["entities"], report.get("parts")
    require(isinstance(listed, list) and isinstance(audits, list) and len(listed) == len(audits) > 0,
            "seed.json and report.json list different parts")
    parts, nonce = [], 0
    for entry, audit in zip(listed, audits):
        require(isinstance(entry, dict) and set(entry) == {"path"} and isinstance(audit, dict), "malformed part entry")
        path = entry["path"]
        require(isinstance(path, str) and PART_PATH.fullmatch(path), f"unsafe part path {path!r}")
        require(audit.get("path") == path, "report part order differs")
        require(sums.get(path) == audit.get("sha256"), f"{path}: report checksum differs from SHA256SUMS")
        for field in ("bytes", "entities", "firstNonce"):
            require(type(audit.get(field)) is int and audit[field] >= 0, f"{path}: part {field}")
        require(audit["firstNonce"] == nonce and audit["entities"] > 0, f"{path}: nonces do not continue")
        nonce += audit["entities"]
        parts.append({"path": path, "sha256": audit["sha256"], "bytes": audit["bytes"],
                      "entities": audit["entities"], "firstNonce": audit["firstNonce"]})
    require(set(sums) == {p["path"] for p in parts} | {"seed.json", "report.json"},
            "SHA256SUMS must list exactly the parts, report.json and seed.json")
    require(len({p["path"] for p in parts}) == len(parts), "a part is listed twice")
    totals = {}
    for field in ("entities", "contracts", "records", "bytesValues", "storageUnits"):
        require(type(report.get(field)) is int and report[field] >= 0, f"report {field}")
        totals[field] = report[field]
    require(totals["entities"] == nonce and 0 < nonce <= STREAM_ROW_LIMIT,
            "parts hold another number of entities than the report, or more than the stream allows")
    kinds, distinct = report.get("kinds"), report.get("distinct")
    require(isinstance(kinds, dict) and set(kinds) <= set(KINDS) and all(type(v) is int for v in kinds.values()),
            "report kinds")
    require(isinstance(distinct, dict) and set(distinct) == {"sources", "codes", "compilations"}
            and all(type(v) is int for v in distinct.values()), "report distinct")
    return {"chainId": chain_id, "owner": owner, "prefix": prefix, "parts": parts, "totals": totals,
            "kinds": kinds, "distinct": distinct, "seed": seed, "report": report, "lock": lock,
            "sourceCompleteRequired": report.get("sourceCompleteRequired") is True}


def check_disk(directory, reserve):
    require(shutil.disk_usage(directory).free >= reserve, "free disk is below the configured reserve")


def sync_json(path, value):
    with path.open("xb") as file:
        file.write(json.dumps(value, indent=2, sort_keys=True).encode() + b"\n")
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


class Converter:
    """The ordered pass: references, cross-row content, totals, and the stream."""

    def __init__(self, m, chain_id, owner, chunk_file):
        self.m = m
        self.chain_id, self.owner = chain_id, owner
        self.rekey = (chain_id, owner) != (m["chainId"], m["owner"])
        # key -> (kind, nonce, content hash, extra) of every referenceable row
        self.index = {}
        # Chunk data lives in a scratch file (key -> offset, length), read back when joined.
        self.chunk_file = chunk_file
        self.chunks = {}
        self.chunks_joined = set()
        self.chunk_bytes = 0
        self.abihash_unchecked = 0
        self.hashes = set()
        self.addresses = set()
        self.matchids = set()
        self.kinds = [0] * len(KINDS)
        self.references = [0] * len(KINDS)
        self.rows = self.units = self.bytes_values = self.chunk_lists = 0
        self.max_attributes = self.max_frame = self.max_row_bytes = 0
        self.first_key = self.last_key = None

    def new_key(self, nonce):
        return entity_key(self.chain_id, self.owner, nonce)

    def target(self, key, expected, name):
        found = self.index.get(key)
        require(found is not None, f"{name} names {hx(key)} before it exists")
        require(found[0] == expected, f"{name} names {hx(key)} as a {KINDS[expected]}, but it is a {KINDS[found[0]]}")
        return found

    def join(self, value):
        digest, length, keys = chunk_list(value)
        joined = bytearray()
        for key in keys:
            at = self.chunks.get(key)
            require(at is not None, f"chunk {hx(key)} is not a chunk row")
            require(len(joined) + at[1] <= length, "chunk size mismatch")
            data = os.pread(self.chunk_file.fileno(), at[1], at[0])
            require(len(data) == at[1], "chunk scratch read short")
            joined += data
            self.chunks_joined.add(key)
        require(len(joined) == length and hashlib.sha256(joined).digest() == digest, "chunk integrity mismatch")
        self.chunk_lists += 1
        return bytes(joined)

    def row(self, blob, summary):
        nonce, key, kind, content_hash, refs, info, deferred, units, bytes_len, attributes = summary
        require(nonce == self.rows, "rows out of order")
        targets = {}
        for offset, ref, expected, name in refs:
            found = self.target(ref, expected, name)
            self.references[expected] += 1
            targets.setdefault(name, []).append(found)
            if self.rekey:
                blob[offset:offset + 32] = self.new_key(found[1])
        if deferred is not None:
            info = content_info(kind, deferred[0], deferred[1], self.join)
        if kind == CONTRACT:
            address, matchid, compilationhash, abihash, creation, runtime = info
            compilation = targets["compilationref"][0]
            require(compilation[2] == compilationhash, "contract's compilationref names an entity without its compilationhash")
            if compilation[3] is not None:
                require(compilation[3] == abihash, "abihash does not match the compilation's abi digest")
            else:
                # A reference into the ABI: only hydration can check abihash.
                self.abihash_unchecked += 1
            for ref, expected in (("creationcoderef", creation), ("runtimecoderef", runtime)):
                if ref in targets:
                    require(targets[ref][0][2] == expected, f"contract's {ref} names an entity without its code hash")
            require(address not in self.addresses, f"two contracts for {hx(address)}")
            require(matchid not in self.matchids, f"two contracts with matchid {matchid}")
            self.addresses.add(address)
            self.matchids.add(matchid)
        elif kind == COMPILATION:
            sources, codes, abi_digest = info
            got = [t[2] for t in targets.get("sourcerefs", [])]
            require(got == sources, "a source behind sourcerefs does not have the manifest's hash")
            for slot, expected in codes.items():
                require(targets[f"{slot}coderef"][0][2] == expected, f"the {slot} code does not have the manifest's hash")
            self.index[key] = (kind, nonce, content_hash, abi_digest)
        elif kind == CHUNK_KIND:
            self.chunk_file.seek(0, os.SEEK_END)
            self.chunks[key] = (self.chunk_file.tell(), len(info))
            self.chunk_file.write(info)
            self.chunk_file.flush()
            self.chunk_bytes += len(info)
            self.index[key] = (kind, nonce, None, None)
        else:
            self.index[key] = (kind, nonce, content_hash, None)
        if content_hash is not None:
            marker = bytes([kind]) + content_hash
            require(marker not in self.hashes, f"two {KINDS[kind]} entities with hash {hx(content_hash)}")
            self.hashes.add(marker)
        self.kinds[kind] += 1
        self.units += units
        self.bytes_values += bytes_len
        self.max_attributes = max(self.max_attributes, attributes)
        self.max_row_bytes = max(self.max_row_bytes, bytes_len)
        new = self.new_key(nonce) if self.rekey else key
        if self.first_key is None:
            self.first_key = new
        self.last_key = new
        self.rows += 1

    def finish(self):
        m, t = self.m, self.m["totals"]
        kinds = {KINDS[i]: n for i, n in enumerate(self.kinds) if n}
        require(self.rows == t["entities"], "entity count differs from report.json")
        require(kinds == m["kinds"], "kind counts differ from report.json")
        require(self.kinds[CONTRACT] == t["contracts"], "contract count differs from report.json")
        require(m["distinct"] == {"sources": self.kinds[SOURCE], "codes": self.kinds[CODE],
                                  "compilations": self.kinds[COMPILATION]}, "distinct counts differ from report.json")
        require(self.bytes_values == t["bytesValues"], "bytes values differ from report.json")
        require(self.units == t["storageUnits"], "storage units differ from report.json")
        return kinds


def progress(phase, rows, total, started):
    print(json.dumps({"event": "sourcify_v4_prefill", "phase": phase, "rows": rows, "totalRows": total,
                      "elapsedSeconds": round(time.monotonic() - started, 1)}), file=sys.stderr, flush=True)


def run(args):
    started = time.monotonic()
    require(keccak().hexdigest() == "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            "Keccak provider failed self-test")
    require(1 <= args.workers <= 256, "--workers must be 1 to 256")
    require(0 <= args.reserve_gib <= 1 << 20, "invalid --reserve-gib")
    # Nothing in a row says which chain derives its keys: the base genesis chain must be named.
    require(args.command != "convert" or args.chain_id is not None,
            "convert needs --chain-id: the chain ID of the base genesis the stream is built on")
    root = args.bundle.absolute()
    require(root.is_dir() and not root.is_symlink(), "bundle must be a real directory")
    destination = args.out.absolute()
    require(not destination.exists() and not destination.is_symlink(), "output already exists")
    require(destination.parent.is_dir(), "output parent must already exist")
    reserve = args.reserve_gib * GIB
    check_disk(destination.parent, reserve)
    m = manifest(root, args.allow_eof_source)
    chain_id = m["chainId"] if args.chain_id is None else args.chain_id
    require(type(chain_id) is int and 0 < chain_id < 1 << 64, "invalid --chain-id")
    owner = m["owner"] if args.owner is None else hex_value(args.owner, 20, "--owner")
    require(owner != bytes(20), "owner must be nonzero")
    total = m["totals"]["entities"]
    convert = args.command == "convert"
    with tempfile.TemporaryDirectory(prefix=".sourcify-v4-prefill-", dir=destination.parent) as temporary:
        staged = Path(temporary) / "published"
        staged.mkdir()
        stream_hash = keccak(bytes([len(STREAM_DOMAIN)]) + STREAM_DOMAIN)
        sha = hashlib.sha256()
        written = 0
        out = chunk_file = None
        tasks = [(str(root), part, m["chainId"], m["owner"], m["prefix"], owner) for part in m["parts"]]
        context = multiprocessing.get_context("fork")
        window = max(2, 2 * args.workers)
        last = time.monotonic()
        pool = concurrent.futures.ProcessPoolExecutor(args.workers, mp_context=context)
        try:
            # The first submissions fork every worker (fork context), before any output is open.
            pending = []
            queued = iter(enumerate(tasks))
            for i, task in queued:
                pending.append((i, pool.submit(scan_part, task)))
                if len(pending) >= window:
                    break
            chunk_file = open(Path(temporary) / "chunks.bin", "x+b")
            c = Converter(m, chain_id, owner, chunk_file)
            out = (staged / "prefill.bin").open("xb") if convert else None

            def write(data):
                nonlocal written
                if out is not None:
                    out.write(data)
                stream_hash.update(data)
                sha.update(data)
                written += len(data)
            write(b"ARKPFL02" + struct.pack(">IQ", ROW_CODEC, total))
            while pending:
                i, future = pending.pop(0)
                nxt = next(queued, None)
                if nxt is not None:
                    pending.append((nxt[0], pool.submit(scan_part, nxt[1])))
                part = m["parts"][i]
                digest, size, count, longest, frames, rows = future.result()
                require(size == part["bytes"] and digest == part["sha256"], f"{part['path']}: checksum mismatch")
                require(count == part["entities"], f"{part['path']}: entity count differs")
                blob = bytearray(frames) if c.rekey else frames
                for line, summary in enumerate(rows):
                    try:
                        c.row(blob, summary)
                    except (Invalid, ValueError, KeyError, TypeError, zlib.error) as error:
                        raise Invalid(f"{part['path']} line {line + 1}: {error}") from None
                c.max_frame = max(c.max_frame, longest)
                write(bytes(blob) if c.rekey else blob)
                if time.monotonic() - last >= 10:
                    check_disk(destination.parent, reserve)
                    progress("rows", c.rows, total, started)
                    last = time.monotonic()
            pool.shutdown()
            kinds = c.finish()
            if out is not None:
                out.flush()
                os.fsync(out.fileno())
        except BaseException:
            # Stop handing out parts; the ones in flight finish and are discarded.
            pool.shutdown(wait=True, cancel_futures=True)
            raise
        finally:
            for file in (out, chunk_file):
                if file is not None:
                    file.close()
        progress("rows", c.rows, total, started)
        lock = {"schema": "arkiv-sourcify-v4-source-lock-v1", "bundle": str(root),
                "files": m["lock"] + [{"path": p["path"], "bytes": p["bytes"], "sha256": p["sha256"]} for p in m["parts"]]}
        report = {
            "schema": "arkiv-sourcify-v4-conversion-v1",
            "command": args.command,
            "framing": "ARKPFL02", "rowCodec": ROW_CODEC,
            "source": {"format": FORMAT, "seedVersion": SEED_VERSION, "chainId": m["chainId"], "owner": hx(m["owner"]),
                       "schemaPrefix": m["prefix"], "parts": len(m["parts"]),
                       "seedSha256": m["lock"][1]["sha256"], "reportSha256": m["lock"][2]["sha256"],
                       "sha256sumsSha256": m["lock"][0]["sha256"],
                       "sourceCompleteRequired": m["sourceCompleteRequired"], "records": m["totals"]["records"]},
            "sourceLockSha256": hashlib.sha256(json.dumps(lock, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
            "chainId": chain_id, "creator": hx(owner), "rekeyed": c.rekey,
            "keyPolicy": "bundle-nonce-from-zero/salt-zero-v1",
            "rows": c.rows, "contracts": c.kinds[CONTRACT], "entitiesByKind": kinds,
            "references": {KINDS[i]: n for i, n in enumerate(c.references) if n} | {"total": sum(c.references)},
            "chunkListsJoined": c.chunk_lists, "chunkBytes": c.chunk_bytes,
            "chunkRowsNeverJoined": len(c.chunks) - len(c.chunks_joined),
            "contractsWithAbihashLeftToHydrate": c.abihash_unchecked,
            "bytesValues": c.bytes_values, "storageUnits": str(c.units),
            "maxAttributes": c.max_attributes, "maxRowBytesValues": c.max_row_bytes, "maxFrameBytes": c.max_frame,
            "expiry": "permanent", "permanentExpiry": str(PERMANENT), "permanentPrefillRequired": True,
            "flags": 0, "salt": "0", "rentWei": "0",
            "firstKey": hx(c.first_key), "lastKey": hx(c.last_key),
            "validationScope": "bundle-checksums/row-rules/format-schema/references/content-hashes-and-digests",
            "workers": args.workers,
            "seconds": round(time.monotonic() - started, 3),
            "peakRssKiB": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
            "peakWorkerRssKiB": resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,
        }
        if convert:
            report.update({"streamHash": "0x" + stream_hash.hexdigest(), "prefillSha256": sha.hexdigest(),
                           "canonicalBytes": written})
        sync_json(staged / "source-lock.json", lock)
        sync_json(staged / "conversion-report.json", report)
        sync_dir(staged)
        publish(staged, destination)
    return report


def parser():
    result = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    result.add_argument("command", choices=("inventory", "convert"),
                        help="inventory: every check, publishing only the report; convert: also prefill.bin")
    result.add_argument("--bundle", type=Path, required=True, help="v4 bundle directory (process --format udc)")
    result.add_argument("--out", type=Path, required=True, help="new directory; never overwritten")
    result.add_argument("--chain-id", type=int, default=None,
                        help="chain ID of the base genesis (required for convert); another than the bundle's re-keys")
    result.add_argument("--owner", default=None, help="creator and owner of every row (default: the bundle's); re-keys")
    result.add_argument("--workers", type=int, default=min(8, os.cpu_count() or 1), help="part-scanning processes")
    result.add_argument("--reserve-gib", type=int, default=80, help="free disk to keep")
    result.add_argument("--allow-eof-source", action="store_true",
                        help="accept a bundle processed without --require-source-complete (fixtures)")
    return result


if __name__ == "__main__":
    try:
        print(json.dumps(run(parser().parse_args()), sort_keys=True))
    except (Invalid, OSError, ValueError, KeyError, TypeError, RecursionError, zlib.error) as error:
        print(f"sourcify-v4-prefill: {error}", file=sys.stderr)
        sys.exit(1)
