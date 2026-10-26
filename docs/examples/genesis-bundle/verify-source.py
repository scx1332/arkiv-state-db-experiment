"""Audit this small checked-in example; not a protocol or untrusted-input validator."""

from collections import Counter, defaultdict
from hashlib import sha256
import json
from pathlib import Path
import re
import tomllib


ROOT = Path(__file__).resolve().parent


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_toml(path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


def pinned(reference):
    relative = reference["path"]
    require(
        not relative.startswith("/")
        and "\\" not in relative
        and all(part not in ("", ".", "..") for part in relative.split("/")),
        f"invalid relative path: {relative}",
    )
    path = ROOT
    for part in relative.split("/"):
        path = path / part
        require(not path.is_symlink(), f"symlink: {relative}")
    require(path.is_file(), f"not a regular file: {relative}")
    data = path.read_bytes()  # Intentionally only these tiny example files.
    require(len(data) == reference["bytes"], f"length mismatch: {relative}")
    require(sha256(data).hexdigest() == reference["sha256"], f"hash mismatch: {relative}")
    return path


def uint(text, bits=64):
    require(isinstance(text, str) and re.fullmatch(r"0|[1-9][0-9]*", text), "integer spelling")
    value = int(text)
    require(value < 2**bits, f"u{bits} overflow")
    return value


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


manifest = read_toml(ROOT / "chain.toml")
require(manifest["schema"] == "arkiv-genesis-bundle-v1", "manifest schema")
profile = read_toml(pinned(manifest["profile"]))
require(profile["schema"] == "arkiv-genesis-profile-v1", "profile schema")
allocation_file = read_toml(pinned(manifest["allocations"]))
require(allocation_file["schema"] == "arkiv-genesis-allocations-v1", "allocation schema")
allocations = allocation_file["allocation"]
require(len(allocations) <= 256, "allocation count")
balances = {}
for allocation in allocations:
    address = allocation["address"]
    require(re.fullmatch(r"0x[0-9a-f]{40}", address), "allocation address")
    require(address not in balances, "duplicate allocation")
    balance = uint(allocation["balance_wei"], 256)
    require(balance > 0, "zero allocation")
    balances[address] = balance
initial_supply = sum(balances.values())
require(initial_supply < 2**256, "initial supply overflow")
require(
    initial_supply == uint(manifest["allocations"]["expected_initial_supply_wei"], 256),
    "initial supply assertion",
)
require(manifest["prefill"]["sponsor"] in balances, "sponsor allocation missing")
require(manifest["chain"]["proposer"] in balances, "example proposer allocation missing")

rows = []
for dataset in manifest["prefill"]["dataset"]:
    require(dataset["format"] == "arkiv-entities-toml-v2", "example selects TOML")
    source = read_toml(pinned(dataset))
    require(source["schema"] == dataset["format"], "dataset schema")
    rows.extend(source["entity"])
alternative = [
    json.loads(line, object_pairs_hook=unique_object)
    for line in (ROOT / "datasets/catalog.jsonl").read_text(encoding="utf-8").splitlines()
]
require(rows == alternative, "TOML/JSONL records differ")
require(len(rows) <= uint(manifest["prefill"]["limits"]["max_entities"]), "row count")
expiry_counts = Counter()
nonces = defaultdict(list)
bytes_total = 0
VALUE_TAGS = {"bool", "i32", "u64", "u256", "dec", "str", "bytes", "bytes_text", "bytes_file", "bytes32", "addr", "key"}
for row in rows:
    require(re.fullmatch(r"0x[0-9a-f]{40}", row["creator"]), "creator address")
    nonces[row["creator"]].append(uint(row["create_nonce"]))
    uint(row["salt"], 128)
    expiry = uint(row["expires_at_height"])
    # Entity lifetimes are unbounded since protocol 3; only the rent is capped.
    require(0 < expiry, "expiry bounds")
    require(uint(profile["chain_params"]["max_rent_lifetime_seconds"]) > 0, "rent cap")
    require(expiry != 2**64 - 1, "permanent expiry")
    expiry_counts[expiry] += 1
    require(type(row["flags"]) is int and 0 <= row["flags"] <= 3, "creation flags")
    require(not any(name.startswith("$") for name in row["attributes"]), "system override")
    require(not {"content_type", "payload", "payload_text", "payload_base64"} & row.keys(), "payload model")
    require(len(row["attributes"]) <= 32, "attributes per entity")
    require(len(row["attributes"]) + 7 <= profile["engine_limits"]["max_cells"], "total cells")
    entity_bytes = 0
    for name, attribute in row["attributes"].items():
        tags = VALUE_TAGS & attribute.keys()
        require(len(tags) == 1 and set(attribute) == tags | {"indexed"}, f"attribute {name}")
        require(type(attribute["indexed"]) is bool, f"indexed flag {name}")
        if "bytes_file" in tags:
            entity_bytes += pinned(attribute["bytes_file"]).stat().st_size
        if tags & {"bytes", "bytes_text", "bytes_file"}:
            require(not attribute["indexed"], f"indexed bytes {name}")
    require(entity_bytes <= 128 * 1024, "entity bytes")
    bytes_total += entity_bytes
for creator, values in nonces.items():
    require(sorted(values) == list(range(len(values))), f"creation nonces: {creator}")
require(max(expiry_counts.values(), default=0) <= profile["chain_params"]["expiry_per_block"], "expiry reservation")

print(f"Verified example source pins; {len(rows)} equivalent TOML/JSONL entities, {bytes_total} bytes-attribute bytes.")
print(f"Gross initial supply: {initial_supply} wei. Peak expiry count: {max(expiry_counts.values(), default=0)}.")
print("No rent debit, canonical encoding, state root or launch certificate is calculated by this source audit.")
