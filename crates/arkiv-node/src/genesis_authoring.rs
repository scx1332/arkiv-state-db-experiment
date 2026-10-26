//! Strict, small-input authoring frontend. Human source bytes are provenance;
//! only the canonical launch record and prefill stream define chain identity.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Component, Path},
    sync::atomic::{AtomicU64, Ordering},
};

use alloy_primitives::U256;
use arkiv_db::{Cell, CellName, Limits, OwnerId, PriceSchedule, Value, decode_exact, encode};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustix::fs::{self, AtFlags, Mode, OFlags, RenameFlags};
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};

use crate::{
    ChainParams, Genesis,
    accounts::AccountConfig,
    genesis_bundle::{self, PrefillEntity, PrefillLimits},
    signed::protocol::{SignedGenesis, SignedLimits},
};

pub type AuthoringReport = serde_json::Value;
type Result<T> = std::result::Result<T, String>;
const DOCUMENT_CAP: usize = 1024 * 1024;
const INPUT_CAP: usize = 8 * DOCUMENT_CAP;
const ROW_CAP: usize = 256;
const LOCK_FILE: &str = "genesis.lock.toml";
const PRESET_ID: &str = "arkiv-entity-v3/glm-v2@4";
/// The rent rule of the profile: capped GLM rent per second, debited outside
/// gas ([`crate::gas::rent`]).
const RENT_RULE: &str = "glm-rent-v3";
/// The entity rules of the profile: user-defined columns.
const ENTITY_RULES: &str = "arkiv-entity-v3";
/// Entity dataset formats; v1 (payload and content type) is refused.
const TOML_DATASET: &str = "arkiv-entities-toml-v2";
const JSONL_DATASET: &str = "arkiv-entities-jsonl-v2";
const PRESET: &str =
    include_str!("../../../docs/examples/genesis-bundle/profiles/fixed-glm-v2.toml");
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    protocol: String,
    name: Option<String>,
    chain: Chain,
    profile: ProfileRef,
    allocations: Allocations,
    prefill: Prefill,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chain {
    id: String,
    run_id: String,
    start_time: String,
    block_period_ms: String,
    proposer: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRef {
    preset: Option<String>,
    path: Option<String>,
    bytes: Option<u64>,
    sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Allocations {
    path: Option<String>,
    bytes: Option<u64>,
    sha256: Option<String>,
    entry: Option<Vec<Allocation>>,
    expected_initial_supply_wei: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AllocationDocument {
    schema: String,
    allocation: Vec<Allocation>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Allocation {
    label: Option<String>,
    address: String,
    balance_wei: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prefill {
    builder: String,
    namespace: String,
    rent_policy: String,
    sponsor: String,
    /// Selects the permanent-prefill contract (launch version 3, and version 4
    /// once `genesis stream-build` uses this genesis as its base): rows may
    /// use `expires_at_height = "18446744073709551615"` (`u64::MAX`) to never
    /// expire, without expiry scheduling or time-based rent. Absent or false
    /// keeps the finite-only version 1 launch and its identity unchanged.
    #[serde(default)]
    allow_permanent: bool,
    limits: AuthorLimits,
    #[serde(default)]
    entity: Vec<Entity>,
    #[serde(default)]
    dataset: Vec<DatasetRef>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorLimits {
    max_entities: String,
    max_canonical_bytes: String,
    max_storage_units: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DatasetRef {
    id: String,
    format: String,
    path: String,
    bytes: Option<u64>,
    sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityDocument {
    schema: String,
    #[serde(default)]
    entity: Vec<Entity>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRef {
    path: String,
    bytes: Option<u64>,
    sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entity {
    label: Option<String>,
    creator: String,
    create_nonce: String,
    salt: String,
    expires_at_height: String,
    flags: u8,
    #[serde(default)]
    attributes: BTreeMap<String, Attribute>,
}
/// One user attribute: exactly one typed value and an explicit `indexed`.
/// `bytes` is Base64; `bytes_text` (exact UTF-8) and `bytes_file` (a pinned
/// source file) are other spellings of a `bytes` value. `bytes` is never
/// indexed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attribute {
    indexed: bool,
    bool: Option<bool>,
    i32: Option<String>,
    u64: Option<String>,
    u256: Option<String>,
    dec: Option<String>,
    str: Option<String>,
    bytes: Option<String>,
    bytes_text: Option<String>,
    bytes_file: Option<FileRef>,
    bytes32: Option<String>,
    addr: Option<String>,
    key: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema: String,
    entity_rules: String,
    account_rules: String,
    state_layout: u32,
    codec_version: u32,
    admins: Vec<String>,
    pricing: Pricing,
    engine_limits: EngineLimits,
    chain_params: Params,
    signed_limits: WireLimits,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Pricing {
    schedule_id: u32,
    weights: [u64; 31],
    gas_price_wei: String,
    rent_rule: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EngineLimits {
    namespace_cap: u64,
    max_cells: usize,
    max_cell_name_len: usize,
    max_str_len: usize,
    max_bytes_len: usize,
    max_indexed_len: usize,
    max_page: usize,
    max_groups: usize,
    max_literals: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Params {
    block_budget: String,
    user_budget: String,
    expiry_per_block: u32,
    max_admin_per_block: u32,
    max_txs_per_block: u32,
    max_ops_per_tx: u32,
    max_ops_per_block: u32,
    max_tx_bytes: u32,
    max_block_input_bytes: u32,
    max_lifetime: String,
    /// The rent cap in seconds, 1 to 100 years: the consensus
    /// `max_rent_lifetime_seconds`, applied in whole blocks (rounded up) at
    /// the chain's `block_period_ms`.
    max_rent_lifetime_seconds: String,
    /// The consensus `max_new_storage_units_per_block`, at least 1.
    max_new_storage_units_per_block: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WireLimits {
    max_tx_bytes: u32,
    max_input_bytes: u32,
    max_body_bytes: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourcePin {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Lock {
    schema: String,
    compiler: String,
    genesis_sha256: String,
    prefill_sha256: String,
    expanded_profile_sha256: String,
    expanded_profile: Profile,
    source: Vec<SourcePin>,
    resolved: serde_json::Value,
}
struct Compiled {
    root: File,
    genesis: SignedGenesis,
    stream: Vec<u8>,
    lock: Lock,
}

fn err(e: impl std::fmt::Display) -> String {
    let mut message = e.to_string();
    if message.len() > 512 {
        let mut end = 512;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    message
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn expect(value: &str, required: &str, field: &str) -> Result<()> {
    if value == required {
        Ok(())
    } else {
        Err(format!("unsupported {field}: {}", err(value)))
    }
}
fn identifier(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("invalid ASCII label or dataset ID".into());
    }
    Ok(())
}
fn decimal_integer(s: &str, signed: bool) -> Result<()> {
    let magnitude = if signed {
        s.strip_prefix('-').unwrap_or(s)
    } else {
        s
    };
    if magnitude.is_empty()
        || !magnitude.bytes().all(|b| b.is_ascii_digit())
        || (magnitude.len() > 1 && magnitude.starts_with('0'))
        || s == "-0"
    {
        return Err("expected canonical decimal integer string".into());
    }
    Ok(())
}
fn number<T: std::str::FromStr>(s: &str) -> Result<T> {
    decimal_integer(s, false)?;
    s.parse().map_err(|_| "integer out of range".into())
}
fn signed_number<T: std::str::FromStr>(s: &str) -> Result<T> {
    decimal_integer(s, true)?;
    s.parse().map_err(|_| "signed integer out of range".into())
}
fn uint256(s: &str) -> Result<U256> {
    decimal_integer(s, false)?;
    U256::from_str_radix(s, 10).map_err(|_| "U256 out of range".into())
}
fn fixed_hex<const N: usize>(s: &str) -> Result<[u8; N]> {
    let s = s.strip_prefix("0x").ok_or("expected lowercase 0x hex")?;
    if s.len() != N * 2
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("expected {N}-byte lowercase hex"));
    }
    let mut out = [0; N];
    for (byte, pair) in out.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
        *byte = (hex_digit(pair[0]) << 4) | hex_digit(pair[1]);
    }
    Ok(out)
}
fn hex_digit(b: u8) -> u8 {
    if b <= b'9' { b - b'0' } else { b - b'a' + 10 }
}
fn base64_bytes(s: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD.decode(s).map_err(err)?;
    if STANDARD.encode(&bytes) != s {
        return Err("noncanonical Base64".into());
    }
    Ok(bytes)
}
fn decimal_units(s: &str) -> Result<[u8; 32]> {
    let negative = s.starts_with('-');
    let magnitude = s.strip_prefix('-').unwrap_or(s);
    let (whole, fraction) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    decimal_integer(whole, false)?;
    if (magnitude.contains('.') && fraction.is_empty())
        || fraction.len() > 18
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("decimal requires at most 18 exact fractional digits".into());
    }
    let scale = U256::from(1_000_000_000_000_000_000u64);
    let mut units = uint256(whole)?
        .checked_mul(scale)
        .ok_or("decimal overflow")?;
    if !fraction.is_empty() {
        let digits = U256::from_str_radix(fraction, 10).map_err(err)?;
        units = units
            .checked_add(digits * U256::from(10u64.pow((18 - fraction.len()) as u32)))
            .ok_or("decimal overflow")?;
    }
    let boundary = U256::from(1) << 255;
    if units > boundary || (!negative && units == boundary) || (negative && units == U256::ZERO) {
        return Err("decimal outside signed 256-bit range or negative zero".into());
    }
    if negative {
        units = (!units).wrapping_add(U256::from(1));
    }
    Ok(units.to_be_bytes())
}
fn cell(a: Attribute, inputs: &mut Inputs) -> Result<Cell> {
    let mut values = Vec::new();
    if let Some(v) = a.bool {
        values.push(Value::Bool(v));
    }
    if let Some(v) = a.i32 {
        values.push(Value::I32(signed_number(&v)?));
    }
    if let Some(v) = a.u64 {
        values.push(Value::U64(number(&v)?));
    }
    if let Some(v) = a.u256 {
        values.push(Value::U256(uint256(&v)?.to_be_bytes()));
    }
    if let Some(v) = a.dec {
        values.push(Value::Dec(decimal_units(&v)?));
    }
    if let Some(v) = a.str {
        values.push(Value::str(&v));
    }
    if let Some(v) = a.bytes {
        values.push(Value::bytes(&base64_bytes(&v)?));
    }
    if let Some(v) = a.bytes_text {
        values.push(Value::bytes(v.as_bytes()));
    }
    if let Some(f) = a.bytes_file {
        values.push(Value::bytes(&inputs.read(
            &f.path,
            f.bytes,
            f.sha256.as_deref(),
        )?));
    }
    if let Some(v) = a.bytes32 {
        values.push(Value::Bytes32(fixed_hex(&v)?));
    }
    if let Some(v) = a.addr {
        values.push(Value::Addr(fixed_hex(&v)?));
    }
    if let Some(v) = a.key {
        values.push(Value::Key(fixed_hex(&v)?));
    }
    let [value] = <[Value; 1]>::try_from(values)
        .map_err(|_| "each attribute requires exactly one typed value")?;
    if a.indexed && matches!(value, Value::Bytes(_)) {
        return Err("bytes attributes are stored, never indexed: use indexed = false".into());
    }
    Ok(if a.indexed {
        Cell::attribute(value)
    } else {
        Cell::field(value)
    })
}
fn address(s: &str, aliases: &BTreeMap<String, [u8; 20]>) -> Result<[u8; 20]> {
    match s.strip_prefix('@') {
        Some(label) => aliases
            .get(label)
            .copied()
            .ok_or_else(|| format!("unknown allocation alias @{}", err(label))),
        None => fixed_hex(s),
    }
}

/// Exact UTC seconds without leap seconds, timezone offsets, or clock defaults.
fn timestamp(s: &str) -> Result<u64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16 | 19) || c.is_ascii_digit())
    {
        return Err("start_time must be YYYY-MM-DDTHH:MM:SSZ".into());
    }
    let parse = |a, b| s[a..b].parse::<u64>().map_err(err);
    let (year, month, day, hour, minute, second) = (
        parse(0, 4)?,
        parse(5, 7)?,
        parse(8, 10)?,
        parse(11, 13)?,
        parse(14, 16)?,
        parse(17, 19)?,
    );
    let leap = |y: u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let months = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > months[(month - 1) as usize]
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("invalid or pre-epoch UTC start_time".into());
    }
    let days = (1970..year)
        .map(|y| if leap(y) { 366 } else { 365 })
        .sum::<u64>()
        + months[..(month - 1) as usize].iter().sum::<u64>()
        + day
        - 1;
    Ok((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000)
}

fn no_multiline(text: &str) -> Result<()> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        match quote {
            Some(b'"') if bytes[i] == b'\\' => {
                i += 2;
                continue;
            }
            Some(q) if bytes[i] == q => quote = None,
            Some(_) => {}
            None if bytes[i] == b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            None if matches!(bytes[i], b'"' | b'\'') => {
                if bytes.get(i + 1) == Some(&bytes[i]) && bytes.get(i + 2) == Some(&bytes[i]) {
                    return Err(
                        "multiline TOML strings are unsupported; use a bytes_file reference".into(),
                    );
                }
                quote = Some(bytes[i]);
            }
            None => {}
        }
        i += 1;
    }
    Ok(())
}
fn utf8(bytes: &[u8]) -> Result<&str> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err("UTF-8 BOM is unsupported".into());
    }
    std::str::from_utf8(bytes).map_err(err)
}
fn parse_toml<T: DeserializeOwned>(bytes: &[u8], path: &str) -> Result<T> {
    if bytes.len() > DOCUMENT_CAP {
        return Err(format!("{path}: document exceeds 1 MiB"));
    }
    let text = utf8(bytes)?;
    no_multiline(text)?;
    toml::from_str(text).map_err(|e| format!("{path}: {}", err(e)))
}
// serde_json::Value normally silently replaces duplicate object keys. This
// recursive visitor rejects them before conversion into the typed row schema.
struct StrictJson(serde_json::Value);
impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrictJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("strict JSON without duplicate keys or floating-point values")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element::<StrictJson>()? {
                    out.push(v.0);
                }
                Ok(StrictJson(out.into()))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if out.contains_key(&k) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                    out.insert(k, a.next_value::<StrictJson>()?.0);
                }
                Ok(StrictJson(out.into()))
            }
        }
        d.deserialize_any(V)
    }
}
fn strict_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let value: StrictJson = serde_json::from_slice(bytes).map_err(err)?;
    serde_json::from_value(value.0).map_err(err)
}

fn relative(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<_> = path.split('/').collect();
    if path.len() > 512
        || !path.is_ascii()
        || path.contains('\\')
        || path.contains('\0')
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(format!("unsafe relative path: {}", err(path)));
    }
    Ok(parts)
}
fn open_directory(path: &Path) -> Result<File> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(err)?.join(path)
    };
    let mut current = File::open("/").map_err(err)?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                current = File::from(
                    fs::openat(
                        &current,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(err)?,
                );
            }
            _ => return Err("directory path contains traversal".into()),
        }
    }
    Ok(current)
}
fn open_relative(root: &File, path: &str, flags: OFlags) -> Result<File> {
    let parts = relative(path)?;
    let mut dir = root.try_clone().map_err(err)?;
    for part in &parts[..parts.len() - 1] {
        dir = File::from(
            fs::openat(
                &dir,
                *part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| format!("{path}: {}", err(e)))?,
        );
    }
    Ok(File::from(
        fs::openat(
            &dir,
            parts[parts.len() - 1],
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|e| format!("{path}: {}", err(e)))?,
    ))
}
fn read_relative(root: &File, path: &str, cap: usize) -> Result<Vec<u8>> {
    let file = open_relative(root, path, OFlags::RDONLY)?;
    let meta = file.metadata().map_err(err)?;
    if !meta.is_file() || meta.len() > cap as u64 {
        return Err(format!(
            "{path}: requires regular file within {cap}-byte cap"
        ));
    }
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    if bytes.len() > cap {
        return Err(format!("{path}: file exceeds byte cap"));
    }
    Ok(bytes)
}
struct Inputs {
    root: File,
    pins: Vec<SourcePin>,
    paths: BTreeSet<String>,
    total: usize,
}
impl Inputs {
    fn read(&mut self, path: &str, length: Option<u64>, hash: Option<&str>) -> Result<Vec<u8>> {
        relative(path)?;
        if self.pins.len() >= 512 {
            return Err("at most 512 source files are supported".into());
        }
        if path.eq_ignore_ascii_case(LOCK_FILE) || !self.paths.insert(path.to_lowercase()) {
            return Err(format!("duplicate or case-colliding source path: {path}"));
        }
        let cap = DOCUMENT_CAP.min(INPUT_CAP - self.total);
        let bytes = read_relative(&self.root, path, cap)?;
        let pin = SourcePin {
            path: path.into(),
            bytes: bytes.len() as u64,
            sha256: sha256(&bytes),
        };
        if length.is_some_and(|n| n != pin.bytes) || hash.is_some_and(|h| h != pin.sha256) {
            return Err(format!("{path}: supplied source pin mismatch"));
        }
        self.total += bytes.len();
        self.pins.push(pin);
        Ok(bytes)
    }
}

fn profile_native(profile: &Profile, chain: &Chain) -> Result<Genesis> {
    expect(
        &profile.schema,
        "arkiv-genesis-profile-v1",
        "profile schema",
    )?;
    expect(&profile.entity_rules, ENTITY_RULES, "entity rules")?;
    expect(&profile.account_rules, "glm-v2", "account rules")?;
    expect(&profile.pricing.rent_rule, RENT_RULE, "rent rule")?;
    if profile.state_layout != arkiv_db::LAYOUT_VERSION
        || profile.codec_version != arkiv_db::CODEC_VERSION
        || !profile.admins.is_empty()
        || number::<u64>(&profile.pricing.gas_price_wei)? != crate::accounts::GAS_PRICE_WEI
    {
        return Err("unsupported profile constants".into());
    }
    let pricing = PriceSchedule::new(profile.pricing.schedule_id, profile.pricing.weights);
    if pricing != crate::gas::schedule() {
        return Err("profile must specify the immutable GLM-v2 pricing schedule".into());
    }
    let e = &profile.engine_limits;
    let p = &profile.chain_params;
    Ok(Genesis {
        format_version: crate::block::FORMAT_VERSION,
        protocol_version: crate::block::PROTOCOL_VERSION,
        chain_id: number(&chain.id)?,
        timestamp_ms: timestamp(&chain.start_time)?,
        limits: Limits {
            namespace_cap: e.namespace_cap,
            max_cells: e.max_cells,
            max_cell_name_len: e.max_cell_name_len,
            max_str_len: e.max_str_len,
            max_bytes_len: e.max_bytes_len,
            max_indexed_len: e.max_indexed_len,
            max_page: e.max_page,
            max_groups: e.max_groups,
            max_literals: e.max_literals,
        },
        pricing,
        params: ChainParams {
            block_budget: number(&p.block_budget)?,
            user_budget: number(&p.user_budget)?,
            expiry_per_block: p.expiry_per_block,
            max_admin_per_block: p.max_admin_per_block,
            max_txs_per_block: p.max_txs_per_block,
            max_ops_per_tx: p.max_ops_per_tx,
            max_ops_per_block: p.max_ops_per_block,
            max_tx_bytes: p.max_tx_bytes,
            max_block_input_bytes: p.max_block_input_bytes,
            max_lifetime: number(&p.max_lifetime)?,
            // Rent is priced at the chain's block period.
            block_period_ms: number(&chain.block_period_ms)?,
            max_rent_lifetime_seconds: rent_cap(p)?,
            max_new_storage_units_per_block: match number(&p.max_new_storage_units_per_block)? {
                0 => return Err("max_new_storage_units_per_block must be at least 1".into()),
                units => units,
            },
        },
        admins: Vec::new(),
    })
}
/// The profile's rent cap in seconds, 1 to 100 years.
fn rent_cap(p: &Params) -> Result<u64> {
    let seconds = number::<u64>(&p.max_rent_lifetime_seconds)?;
    if seconds == 0 || seconds > crate::gas::MAX_RENT_CAP_SECONDS {
        return Err(format!(
            "max_rent_lifetime_seconds must be 1 to {} (100 years)",
            crate::gas::MAX_RENT_CAP_SECONDS
        ));
    }
    Ok(seconds)
}
fn normalize_entity(
    e: Entity,
    aliases: &BTreeMap<String, [u8; 20]>,
    inputs: &mut Inputs,
) -> Result<PrefillEntity> {
    if let Some(label) = &e.label {
        identifier(label)?;
    }
    if e.attributes.len() > crate::arkiv::MAX_ATTRIBUTES {
        return Err("entity exceeds the 32-attribute cap".into());
    }
    let mut attributes = Vec::with_capacity(e.attributes.len());
    for (name, a) in e.attributes {
        if name.starts_with('$') {
            return Err("system attributes cannot be authored".into());
        }
        let cell = cell(a, inputs).map_err(|e| format!("attribute {}: {e}", err(&name)))?;
        attributes.push((
            CellName::new(&name).map_err(|e| format!("invalid attribute {}: {e:?}", err(&name)))?,
            cell,
        ));
    }
    if crate::arkiv::entity_bytes(&attributes) > crate::arkiv::MAX_ENTITY_BYTES {
        return Err("entity bytes attributes exceed 128 KiB together".into());
    }
    Ok(PrefillEntity {
        creator: address(&e.creator, aliases)?,
        create_nonce: number(&e.create_nonce)?,
        salt: number(&e.salt)?,
        expires_at_height: number(&e.expires_at_height)?,
        flags: e.flags,
        attributes,
    })
}
fn compile(path: &Path) -> Result<Compiled> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("chain path must have a UTF-8 filename")?;
    let root = open_directory(parent)?;
    let mut inputs = Inputs {
        root,
        pins: Vec::new(),
        paths: BTreeSet::new(),
        total: 0,
    };
    let source = inputs.read(filename, None, None)?;
    let manifest: Manifest = parse_toml(&source, filename)?;
    expect(
        &manifest.schema,
        "arkiv-genesis-bundle-v1",
        "manifest schema",
    )?;
    expect(&manifest.protocol, "arkiv-launch-prefill-v1", "protocol")?;
    if let Some(name) = &manifest.name
        && name.len() > 256
    {
        return Err("chain name exceeds 256 bytes".into());
    }
    let p = manifest.profile;
    let profile: Profile = match (p.preset, p.path, p.bytes, p.sha256) {
        (Some(preset), None, None, None) => {
            expect(&preset, PRESET_ID, "preset")?;
            parse_toml(PRESET.as_bytes(), PRESET_ID)?
        }
        (None, Some(path), bytes, hash) => {
            parse_toml(&inputs.read(&path, bytes, hash.as_deref())?, &path)?
        }
        _ => return Err("profile requires exactly a versioned preset or file reference".into()),
    };
    let a = manifest.allocations;
    let allocations = match (a.entry, a.path, a.bytes, a.sha256) {
        (Some(entries), None, None, None) => entries,
        (None, Some(path), bytes, hash) => {
            let doc: AllocationDocument =
                parse_toml(&inputs.read(&path, bytes, hash.as_deref())?, &path)?;
            expect(
                &doc.schema,
                "arkiv-genesis-allocations-v1",
                "allocations schema",
            )?;
            doc.allocation
        }
        _ => {
            return Err(
                "allocations require exactly an inline entry list or file reference".into(),
            );
        }
    };
    if allocations.len() > 256 {
        return Err("at most 256 allocations are supported".into());
    }
    let mut aliases = BTreeMap::new();
    let mut entries = Vec::new();
    for entry in allocations {
        let address = fixed_hex(&entry.address)?;
        if let Some(label) = entry.label {
            identifier(&label)?;
            if aliases.insert(label, address).is_some() {
                return Err("duplicate allocation label".into());
            }
        }
        entries.push((OwnerId::new(address), uint256(&entry.balance_wei)?));
    }
    let accounts =
        AccountConfig::new(entries).map_err(|e| format!("invalid allocations: {e:?}"))?;
    if accounts.initial_supply().map_err(|e| format!("{e:?}"))?
        != uint256(&a.expected_initial_supply_wei)?
    {
        return Err("initial allocation supply mismatch".into());
    }
    let native = profile_native(&profile, &manifest.chain)?;
    // Validated by `build_genesis` once the prefill is attached.
    let base = SignedGenesis {
        genesis: native,
        run_id: fixed_hex(&manifest.chain.run_id)?,
        proposer: address(&manifest.chain.proposer, &aliases)?,
        block_period_ms: number(&manifest.chain.block_period_ms)?,
        limits: SignedLimits {
            max_tx_bytes: profile.signed_limits.max_tx_bytes,
            max_input_bytes: profile.signed_limits.max_input_bytes,
            max_body_bytes: profile.signed_limits.max_body_bytes,
        },
        accounts,
        prefill: None,
    };
    let prefill = manifest.prefill;
    expect(&prefill.builder, "arkiv-prefill-v1", "builder")?;
    expect(&prefill.namespace, "entities", "namespace")?;
    expect(
        &prefill.rent_policy,
        "sponsored-rent-v1",
        "prefill rent policy",
    )?;
    let limits = PrefillLimits {
        max_entities: number(&prefill.limits.max_entities)?,
        max_canonical_bytes: number(&prefill.limits.max_canonical_bytes)?,
        max_storage_units: number(&prefill.limits.max_storage_units)?,
    };
    if limits.max_entities > ROW_CAP as u64
        || limits.max_canonical_bytes > DOCUMENT_CAP as u64
        || limits.max_storage_units > INPUT_CAP as u64
    {
        return Err("prefill limits exceed supported 256 rows / 1 MiB canonical bytes / 8 MiB storage units".into());
    }
    let sponsor = address(&prefill.sponsor, &aliases)?;
    let mut rows = prefill.entity;
    if rows.len() > ROW_CAP {
        return Err("more than 256 prefill rows".into());
    }
    let mut ids = BTreeSet::new();
    for source in prefill.dataset {
        identifier(&source.id)?;
        if !ids.insert(source.id) {
            return Err("duplicate dataset ID".into());
        }
        let bytes = inputs.read(&source.path, source.bytes, source.sha256.as_deref())?;
        match source.format.as_str() {
            TOML_DATASET => {
                let doc: EntityDocument = parse_toml(&bytes, &source.path)?;
                expect(&doc.schema, TOML_DATASET, "dataset schema")?;
                rows.extend(doc.entity);
            }
            JSONL_DATASET => {
                let text = utf8(&bytes)?;
                for (i, line) in text.lines().enumerate() {
                    if line.is_empty() {
                        return Err(format!("{}:{}: empty JSONL line", source.path, i + 1));
                    }
                    rows.push(
                        strict_json(line.as_bytes())
                            .map_err(|e| format!("{}:{}: {e}", source.path, i + 1))?,
                    );
                    if rows.len() > ROW_CAP {
                        return Err("more than 256 prefill rows".into());
                    }
                }
            }
            _ => return Err("unsupported dataset format".into()),
        }
        if rows.len() > ROW_CAP {
            return Err("more than 256 prefill rows".into());
        }
    }
    let mut entities = Vec::new();
    let mut canonical_bytes = 20usize;
    for (i, row) in rows.into_iter().enumerate() {
        let entity = normalize_entity(row, &aliases, &mut inputs)
            .map_err(|e| format!("entity {}: {e}", i + 1))?;
        canonical_bytes = canonical_bytes
            .checked_add(4 + encode(&entity).len())
            .ok_or("canonical byte overflow")?;
        if canonical_bytes as u64 > limits.max_canonical_bytes {
            return Err("normalized entity stream exceeds configured canonical byte cap".into());
        }
        entities.push(entity);
    }
    let permanent = prefill.allow_permanent;
    let stream = genesis_bundle::encode_stream_with(&entities, permanent)?;
    let canonical = genesis_bundle::decode_stream_with(&stream, permanent)?;
    let genesis = genesis_bundle::build_genesis_with(base, limits, sponsor, &canonical, permanent)?;
    inputs.pins.sort_by(|a, b| a.path.cmp(&b.path));
    let profile_bytes = toml::to_string(&profile).map_err(err)?;
    let lock = Lock {
        schema: "arkiv-genesis-lock-v1".into(),
        compiler: "arkiv-small-genesis-builder-v1".into(),
        genesis_sha256: sha256(&encode(&genesis)),
        prefill_sha256: sha256(&stream),
        expanded_profile_sha256: sha256(profile_bytes.as_bytes()),
        expanded_profile: profile,
        source: inputs.pins,
        resolved: genesis_bundle::report(&genesis, &stream),
    };
    Ok(Compiled {
        root: inputs.root,
        genesis,
        stream,
        lock,
    })
}

fn temporary_name(kind: &str) -> String {
    format!(
        ".arkiv-{kind}-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    )
}
fn write_new(root: &File, path: &str, bytes: &[u8]) -> Result<()> {
    let mut file = open_relative(root, path, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}
fn old_lock(root: &File) -> Result<Lock> {
    parse_toml(&read_relative(root, LOCK_FILE, DOCUMENT_CAP)?, LOCK_FILE)
}
fn write_lock(root: &File, lock: &Lock, refresh: bool) -> Result<()> {
    let name = temporary_name("lock");
    let bytes = toml::to_string_pretty(lock).map_err(err)?;
    write_new(root, &name, bytes.as_bytes())?;
    let result = if refresh {
        fs::renameat(root, &name, root, LOCK_FILE)
    } else {
        fs::renameat_with(root, &name, root, LOCK_FILE, RenameFlags::NOREPLACE)
    }
    .map_err(err);
    if result.is_err() {
        let _ = fs::unlinkat(root, &name, AtFlags::empty());
    }
    result?;
    root.sync_all().map_err(err)
}

/// Resolve, validate, replay, and pin all source bytes. Existing locks require
/// explicit refresh even when their contents would be identical.
pub fn lock(chain_path: &Path, refresh: bool) -> Result<AuthoringReport> {
    let compiled = compile(chain_path)?;
    let previous = if refresh {
        match fs::statat(&compiled.root, LOCK_FILE, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Some(old_lock(&compiled.root)?),
            Err(rustix::io::Errno::NOENT) => None,
            Err(e) => return Err(err(e)),
        }
    } else {
        None
    };
    write_lock(&compiled.root, &compiled.lock, refresh)?;
    let mut report = genesis_bundle::report(&compiled.genesis, &compiled.stream);
    report["sources"] = serde_json::to_value(&compiled.lock.source).map_err(err)?;
    report["lockChanged"] = serde_json::json!(previous.as_ref() != Some(&compiled.lock));
    let mut changes = serde_json::Map::new();
    if let Some(previous) = previous
        && let Some(fields) = compiled.lock.resolved.as_object()
    {
        for (field, now) in fields {
            let before = &previous.resolved[field];
            if before != now {
                changes.insert(
                    field.clone(),
                    serde_json::json!({"before":before,"after":now}),
                );
            }
        }
        // Optional fields (such as the permanent-prefill markers) can also
        // disappear; report them as changed to null.
        if let Some(old_fields) = previous.resolved.as_object() {
            for (field, before) in old_fields {
                if !fields.contains_key(field) {
                    changes.insert(
                        field.clone(),
                        serde_json::json!({"before":before,"after":null}),
                    );
                }
            }
        }
    }
    report["resolvedChanges"] = changes.into();
    Ok(report)
}

/// Replay a complete locked source tree and atomically publish a private bundle.
/// The unlocked mode still validates supplied source pins and generates a lock
/// inside the bundle; it never edits the source lock.
pub fn build(chain_path: &Path, out_dir: &Path, locked: bool) -> Result<AuthoringReport> {
    let compiled = compile(chain_path)?;
    if locked && old_lock(&compiled.root)? != compiled.lock {
        return Err(
            "stale or incomplete genesis.lock.toml; explicitly run genesis lock --refresh".into(),
        );
    }
    let parent = out_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let destination = out_dir
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("bundle destination needs a UTF-8 directory name")?;
    relative(destination)?;
    let root = open_directory(parent)?;
    let stage = temporary_name("bundle");
    fs::mkdirat(&root, &stage, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(err)?;
    let dir = open_relative(&root, &stage, OFlags::RDONLY | OFlags::DIRECTORY)?;
    let report = genesis_bundle::report(&compiled.genesis, &compiled.stream);
    let mut published = false;
    let result = (|| {
        write_new(&dir, "genesis.bin", &encode(&compiled.genesis))?;
        write_new(&dir, "prefill.bin", &compiled.stream)?;
        write_new(
            &dir,
            "report.json",
            &serde_json::to_vec_pretty(&report).map_err(err)?,
        )?;
        write_new(
            &dir,
            LOCK_FILE,
            toml::to_string_pretty(&compiled.lock)
                .map_err(err)?
                .as_bytes(),
        )?;
        dir.sync_all().map_err(err)?;
        fs::renameat_with(&root, &stage, &root, destination, RenameFlags::NOREPLACE)
            .map_err(err)?;
        published = true;
        root.sync_all().map_err(err)?;
        Ok(report)
    })();
    if result.is_err() && !published {
        for name in ["genesis.bin", "prefill.bin", "report.json", LOCK_FILE] {
            let _ = fs::unlinkat(&dir, name, AtFlags::empty());
        }
        let _ = fs::unlinkat(&root, &stage, AtFlags::REMOVEDIR);
    }
    result
}

/// Recompute the canonical stream, genesis state, rent, identities and report.
/// Source pins remain reviewable provenance; verification needs no source tree.
pub fn verify(bundle_dir: &Path) -> Result<AuthoringReport> {
    let root = open_directory(bundle_dir)?;
    let genesis_bytes = read_relative(&root, "genesis.bin", 65536)?;
    let stream = read_relative(&root, "prefill.bin", DOCUMENT_CAP)?;
    let genesis: SignedGenesis = decode_exact(&genesis_bytes).map_err(err)?;
    if encode(&genesis) != genesis_bytes {
        return Err("noncanonical genesis bytes".into());
    }
    genesis_bundle::verify_genesis(&genesis, &stream)?;
    let lock = old_lock(&root)?;
    expect(&lock.schema, "arkiv-genesis-lock-v1", "lock schema")?;
    expect(
        &lock.compiler,
        "arkiv-small-genesis-builder-v1",
        "lock compiler",
    )?;
    if lock.genesis_sha256 != sha256(&genesis_bytes)
        || lock.prefill_sha256 != sha256(&stream)
        || lock.expanded_profile_sha256
            != sha256(
                toml::to_string(&lock.expanded_profile)
                    .map_err(err)?
                    .as_bytes(),
            )
    {
        return Err("bundle lock commitment mismatch".into());
    }
    if lock.source.is_empty()
        || lock.source.len() > 512
        || lock.source.windows(2).any(|w| w[0].path >= w[1].path)
    {
        return Err("invalid source inventory count or order".into());
    }
    let mut paths = BTreeSet::new();
    let mut total = 0u64;
    for pin in &lock.source {
        relative(&pin.path)?;
        if !paths.insert(pin.path.to_lowercase())
            || pin.bytes > DOCUMENT_CAP as u64
            || pin.sha256.len() != 64
            || !pin
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid bundle source inventory".into());
        }
        total = total
            .checked_add(pin.bytes)
            .ok_or("source inventory byte overflow")?;
    }
    if total > INPUT_CAP as u64 {
        return Err("source inventory exceeds aggregate byte cap".into());
    }
    let profile = &lock.expanded_profile;
    let placeholder = Chain {
        id: genesis.genesis.chain_id.to_string(),
        run_id: String::new(),
        start_time: "1970-01-01T00:00:00Z".into(),
        // The rent clock repeats the signed block period.
        block_period_ms: genesis.block_period_ms.to_string(),
        proposer: String::new(),
    };
    let mut native = profile_native(profile, &placeholder)?;
    native.timestamp_ms = genesis.genesis.timestamp_ms;
    let wire = &profile.signed_limits;
    if native != genesis.genesis
        || genesis.limits
            != (SignedLimits {
                max_tx_bytes: wire.max_tx_bytes,
                max_input_bytes: wire.max_input_bytes,
                max_body_bytes: wire.max_body_bytes,
            })
    {
        return Err("expanded lock profile differs from canonical genesis".into());
    }
    let report = genesis_bundle::report(&genesis, &stream);
    if lock.resolved != report {
        return Err("resolved lock recipe differs from rebuilt genesis".into());
    }
    let stored: serde_json::Value =
        strict_json(&read_relative(&root, "report.json", DOCUMENT_CAP)?)?;
    if stored != report {
        return Err("bundle report differs from independently rebuilt genesis".into());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs as stdfs, path::PathBuf};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(temporary_name("authoring-test"));
            stdfs::create_dir(&root).unwrap();
            let fixture = Self(root);
            let examples =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/genesis-bundle");
            for path in [
                "chain.toml",
                "chain.inline.toml",
                "allocations.toml",
                "profiles/fixed-glm-v2.toml",
                "datasets/catalog.toml",
                "datasets/catalog.jsonl",
                "payloads/welcome.txt",
                "payloads/status.txt",
            ] {
                let dest = fixture.0.join(path);
                stdfs::create_dir_all(dest.parent().unwrap()).unwrap();
                stdfs::copy(examples.join(path), dest).unwrap();
            }
            fixture
        }
        fn chain(&self) -> PathBuf {
            self.0.join("chain.toml")
        }
        fn inline(&self) -> PathBuf {
            self.0.join("chain.inline.toml")
        }
        fn bundle(&self) -> PathBuf {
            self.0.join("built")
        }
        fn change(&self, path: &Path, from: &str, to: &str) {
            let text = stdfs::read_to_string(path).unwrap();
            assert!(text.contains(from), "missing fixture substitution {from}");
            stdfs::write(path, text.replace(from, to)).unwrap();
        }
        fn jsonl_manifest(&self) {
            let mut source: toml::Value =
                toml::from_str(&stdfs::read_to_string(self.chain()).unwrap()).unwrap();
            let dataset = source["prefill"]["dataset"].as_array_mut().unwrap()[0]
                .as_table_mut()
                .unwrap();
            dataset.insert("format".into(), JSONL_DATASET.into());
            dataset.insert("path".into(), "datasets/catalog.jsonl".into());
            dataset.remove("bytes");
            dataset.remove("sha256");
            stdfs::write(self.chain(), toml::to_string(&source).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = stdfs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn compiled_preset_expansion_is_frozen() {
        assert_eq!(
            sha256(PRESET.as_bytes()),
            "1cd8e56e98b9a1345f35887087a18f54b9981db7d34522ebf0b464d3f50b02ec"
        );
    }

    #[test]
    fn lock_build_verify_and_existing_destination_are_checked() {
        let fixture = Fixture::new();
        assert!(build(&fixture.chain(), &fixture.bundle(), true).is_err());
        let locked = lock(&fixture.chain(), false).unwrap();
        assert_eq!(locked["rowCount"], "2");
        assert!(lock(&fixture.chain(), false).is_err());
        let built = build(&fixture.chain(), &fixture.bundle(), true).unwrap();
        assert_eq!(built, verify(&fixture.bundle()).unwrap());
        let genesis = stdfs::read(fixture.bundle().join("genesis.bin")).unwrap();
        assert!(build(&fixture.chain(), &fixture.bundle(), true).is_err());
        assert_eq!(
            genesis,
            stdfs::read(fixture.bundle().join("genesis.bin")).unwrap()
        );
        assert!(!stdfs::read_dir(&fixture.0).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".arkiv-bundle-")
        }));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            stdfs::metadata(fixture.bundle())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn refresh_pins_source_presentation_without_changing_semantics() {
        let fixture = Fixture::new();
        let original = lock(&fixture.inline(), false).unwrap();
        let mut source = stdfs::read_to_string(fixture.inline()).unwrap();
        source.push_str("\n# reviewed source comment\n");
        stdfs::write(fixture.inline(), source).unwrap();
        assert!(
            build(&fixture.inline(), &fixture.bundle(), true)
                .unwrap_err()
                .contains("stale")
        );
        let refreshed = lock(&fixture.inline(), true).unwrap();
        assert_eq!(original["genesisHash"], refreshed["genesisHash"]);
        assert_ne!(original["sources"], refreshed["sources"]);
        assert_eq!(refreshed["resolvedChanges"], serde_json::json!({}));
        build(&fixture.inline(), &fixture.bundle(), true).unwrap();
    }

    #[test]
    fn permanent_rows_require_the_explicit_allow_permanent_switch() {
        let fixture = Fixture::new();
        let finite = lock(&fixture.inline(), false).unwrap();
        assert!(finite.get("permanentPrefill").is_none());
        // An explicit `false` is the default: only the source pin changes.
        fixture.change(
            &fixture.inline(),
            "sponsor = \"@catalog\"",
            "sponsor = \"@catalog\"\nallow_permanent = false",
        );
        let same = lock(&fixture.inline(), true).unwrap();
        assert_eq!(same["resolvedChanges"], serde_json::json!({}));
        assert_eq!(same["genesisHash"], finite["genesisHash"]);
        fixture.change(
            &fixture.inline(),
            "expires_at_height = \"86400\"",
            "expires_at_height = \"18446744073709551615\"",
        );
        let error = lock(&fixture.inline(), true).unwrap_err();
        assert!(error.contains("finite positive expiry"), "{error}");
        fixture.change(
            &fixture.inline(),
            "allow_permanent = false",
            "allow_permanent = true",
        );
        let permanent = lock(&fixture.inline(), true).unwrap();
        assert_eq!(permanent["permanentPrefill"], true);
        assert_eq!(permanent["prefillVersion"], 3);
        assert_eq!(permanent["entities"][0]["rentWei"], "0");
        assert_ne!(permanent["genesisHash"], finite["genesisHash"]);
        let built = build(&fixture.inline(), &fixture.bundle(), true).unwrap();
        assert_eq!(built, verify(&fixture.bundle()).unwrap());
        let genesis: SignedGenesis =
            decode_exact(&stdfs::read(fixture.bundle().join("genesis.bin")).unwrap()).unwrap();
        assert_eq!(genesis.prefill.unwrap().version, 3);
        // Turning the switch off again reports the vanished markers.
        fixture.change(
            &fixture.inline(),
            "allow_permanent = true",
            "allow_permanent = false",
        );
        fixture.change(
            &fixture.inline(),
            "expires_at_height = \"18446744073709551615\"",
            "expires_at_height = \"86400\"",
        );
        let back = lock(&fixture.inline(), true).unwrap();
        assert_eq!(back["genesisHash"], finite["genesisHash"]);
        assert_eq!(
            back["resolvedChanges"]["permanentPrefill"],
            serde_json::json!({"before":true,"after":null})
        );
    }

    #[test]
    fn refresh_reports_economic_changes() {
        let fixture = Fixture::new();
        lock(&fixture.inline(), false).unwrap();
        fixture.change(
            &fixture.inline(),
            "expires_at_height = \"86400\"",
            "expires_at_height = \"86401\"",
        );
        let refreshed = lock(&fixture.inline(), true).unwrap();
        assert!(refreshed["resolvedChanges"]["burnedWei"].is_object());
        assert!(refreshed["resolvedChanges"]["genesisHash"].is_object());
    }

    #[test]
    fn toml_jsonl_source_order_and_decimal_spellings_have_one_identity() {
        let fixture = Fixture::new();
        let original = compile(&fixture.chain()).unwrap();
        fixture.jsonl_manifest();
        let jsonl = fixture.0.join("datasets/catalog.jsonl");
        let text = stdfs::read_to_string(&jsonl)
            .unwrap()
            .replace("12.50", "12.500000000000000000");
        let reversed = text.lines().rev().collect::<Vec<_>>().join("\n") + "\n";
        stdfs::write(jsonl, reversed).unwrap();
        let alternative = compile(&fixture.chain()).unwrap();
        assert_eq!(original.stream, alternative.stream);
        assert_eq!(encode(&original.genesis), encode(&alternative.genesis));
        assert_ne!(original.lock.source, alternative.lock.source);
    }

    #[test]
    fn the_rent_cap_is_a_year_by_default_and_set_in_seconds() {
        let fixture = Fixture::new();
        let clock = |c: &Compiled| crate::gas::RentClock::of(&c.genesis.genesis.params).unwrap();
        // The preset's year, kept in seconds, at this example's 5-second blocks.
        let compiled = compile(&fixture.inline()).unwrap();
        let params = compiled.genesis.genesis.params;
        assert_eq!(compiled.genesis.block_period_ms, 5_000);
        assert_eq!(params.block_period_ms, 5_000);
        assert_eq!(params.max_rent_lifetime_seconds, 31_536_000);
        assert_eq!(clock(&compiled).cap_blocks(), 6_307_200);
        // The same preset at 2-second blocks: the same year, 15,768,000 blocks.
        fixture.change(
            &fixture.inline(),
            "block_period_ms = \"5000\"",
            "block_period_ms = \"2000\"",
        );
        let two_second = compile(&fixture.inline()).unwrap();
        assert_eq!(
            two_second.genesis.genesis.params.max_rent_lifetime_seconds,
            crate::gas::DEFAULT_RENT_CAP_SECONDS
        );
        assert_eq!(clock(&two_second).cap_blocks(), 15_768_000);
        // A custom profile sets its own cap, applied in whole blocks rounded up.
        let custom = PRESET.replace(
            "max_rent_lifetime_seconds = \"31536000\"",
            "max_rent_lifetime_seconds = \"86401\"",
        );
        stdfs::write(fixture.0.join("profiles/custom.toml"), custom).unwrap();
        fixture.change(
            &fixture.inline(),
            "profile = { preset = \"arkiv-entity-v3/glm-v2@4\" }",
            "profile = { path = \"profiles/custom.toml\" }",
        );
        let custom = compile(&fixture.inline()).unwrap();
        assert_eq!(
            custom.genesis.genesis.params.max_rent_lifetime_seconds,
            86_401
        );
        assert_eq!(clock(&custom).cap_blocks(), 43_201);
        assert_ne!(encode(&custom.genesis), encode(&two_second.genesis));
        // The cap is a consensus value: a bundle rebuilds and verifies it.
        build(&fixture.inline(), &fixture.bundle(), false).unwrap();
        verify(&fixture.bundle()).unwrap();
        // Zero, over 100 years, missing, or a retired rent rule are refused.
        for (from, to) in [
            (
                "max_rent_lifetime_seconds = \"86401\"",
                "max_rent_lifetime_seconds = \"0\"",
            ),
            (
                "max_rent_lifetime_seconds = \"86401\"",
                "max_rent_lifetime_seconds = \"3153600001\"",
            ),
            ("max_rent_lifetime_seconds = \"86401\"", "# no rent cap"),
            ("rent_rule = \"glm-rent-v3\"", "rent_rule = \"glm-rent-v2\""),
            ("rent_rule = \"glm-rent-v3\"", "rent_rule = \"glm-rent-v1\""),
        ] {
            fixture.change(&fixture.0.join("profiles/custom.toml"), from, to);
            assert!(compile(&fixture.inline()).is_err(), "accepted {to:?}");
            fixture.change(&fixture.0.join("profiles/custom.toml"), to, from);
        }
        // A hundred years is the longest cap.
        fixture.change(
            &fixture.0.join("profiles/custom.toml"),
            "max_rent_lifetime_seconds = \"86401\"",
            "max_rent_lifetime_seconds = \"3153600000\"",
        );
        let longest = compile(&fixture.inline()).unwrap();
        assert_eq!(clock(&longest).cap_blocks(), 1_576_800_000);
    }

    #[test]
    fn inline_bytes_encodings_normalize_to_exact_bytes() {
        let fixture = Fixture::new();
        let original = compile(&fixture.inline()).unwrap();
        fixture.change(
            &fixture.inline(),
            "bytes_text = \"Welcome to Arkiv.\"",
            &format!("bytes = {:?}", STANDARD.encode(b"Welcome to Arkiv.")),
        );
        assert_eq!(original.stream, compile(&fixture.inline()).unwrap().stream);
        fixture.change(
            &fixture.inline(),
            &format!("bytes = {:?}", STANDARD.encode(b"Welcome to Arkiv.")),
            "bytes = \"V2VsY29tZSB0byBBcmtpdi4\"",
        );
        assert!(compile(&fixture.inline()).is_err());
    }

    #[test]
    fn attributes_carry_their_indexed_flag_into_the_stream() {
        let fixture = Fixture::new();
        let rows =
            genesis_bundle::decode_stream(&compile(&fixture.inline()).unwrap().stream).unwrap();
        assert_eq!(
            rows[0].attributes,
            vec![
                (
                    CellName::new("body").unwrap(),
                    Cell::field(Value::bytes(b"Welcome to Arkiv."))
                ),
                (
                    CellName::new("title").unwrap(),
                    Cell::attribute(Value::str("Welcome"))
                ),
            ]
        );
        // The flag is part of the entity, and so of the chain's identity.
        let indexed = compile(&fixture.inline()).unwrap();
        fixture.change(
            &fixture.inline(),
            "{ str = \"Welcome\", indexed = true }",
            "{ str = \"Welcome\", indexed = false }",
        );
        assert_ne!(
            encode(&indexed.genesis),
            encode(&compile(&fixture.inline()).unwrap().genesis)
        );
        // `bytes` is never indexed, and `indexed` is required.
        for (from, to) in [
            (
                "{ bytes_text = \"Welcome to Arkiv.\", indexed = false }",
                "{ bytes_text = \"Welcome to Arkiv.\", indexed = true }",
            ),
            (
                "{ str = \"Welcome\", indexed = false }",
                "{ str = \"Welcome\" }",
            ),
        ] {
            fixture.change(&fixture.inline(), from, to);
            assert!(compile(&fixture.inline()).is_err(), "accepted {to}");
            fixture.change(&fixture.inline(), to, from);
        }
    }

    #[test]
    fn unknown_duplicate_multiline_and_wrong_types_are_rejected() {
        for (from, to) in [
            (
                "name = \"Small catalog\"",
                "name = \"Small catalog\"\nunknown = 1",
            ),
            ("id = \"9002\"", "id = \"9002\"\nid = \"9002\""),
            ("salt = \"10\"", "salt = 10"),
            ("flags = 0", "flags = 0.0"),
            (
                "bytes_text = \"Welcome to Arkiv.\"",
                "bytes_text = \"\"\"Welcome to Arkiv.\"\"\"",
            ),
            (
                "attributes.title = { str = \"Welcome\", indexed = true }",
                "attributes.title = { str = \"Welcome\", u64 = \"1\", indexed = true }",
            ),
            (
                "attributes.title = { str = \"Welcome\", indexed = true }",
                "attributes.title = { str = \"Welcome\", indexed = true }\nattributes.\"$owner\" = { addr = \"0x2222222222222222222222222222222222222222\", indexed = true }",
            ),
            (
                "bytes_text = \"Welcome to Arkiv.\"",
                "bytes_text = \"Welcome to Arkiv.\", bytes = \"\"",
            ),
            // The removed payload model is not a spelling of anything.
            ("flags = 0", "flags = 0\ncontent_type = \"text/plain\""),
            (
                "flags = 0",
                "flags = 0\npayload_text = \"Welcome to Arkiv.\"",
            ),
        ] {
            let fixture = Fixture::new();
            fixture.change(&fixture.inline(), from, to);
            assert!(compile(&fixture.inline()).is_err(), "accepted {to}");
        }
        no_multiline("# \"\"\" permitted in comments\nvalue = '\"\"\"'\n").unwrap();
        assert!(parse_toml::<Manifest>(b"\xef\xbb\xbfschema = 'x'", "bom").is_err());
    }

    #[test]
    fn json_duplicate_keys_are_rejected_at_every_level() {
        for json in [
            r#"{"creator":"a","creator":"b"}"#,
            r#"{"attributes":{"name":{"str":"a"},"name":{"str":"b"}}}"#,
            r#"{"attributes":{"name":{"str":"a","str":"b"}}}"#,
            r#"{"flags":0.0}"#,
        ] {
            assert!(strict_json::<serde_json::Value>(json.as_bytes()).is_err());
        }
        let fixture = Fixture::new();
        fixture.jsonl_manifest();
        let jsonl = fixture.0.join("datasets/catalog.jsonl");
        fixture.change(&jsonl, "\"flags\":0", "\"flags\":0,\"flags\":0");
        assert!(
            compile(&fixture.chain())
                .unwrap_err_string()
                .contains("duplicate JSON")
        );
    }

    #[test]
    fn integer_decimal_time_and_hex_boundaries_are_exact() {
        for s in ["", "+1", "01", "-1", "1.0", " 1", "1_000"] {
            assert!(number::<u64>(s).is_err(), "{s}");
        }
        assert!(number::<u64>("18446744073709551616").is_err());
        assert_eq!(number::<u64>("18446744073709551615").unwrap(), u64::MAX);
        assert_eq!(
            signed_number::<i64>("-9223372036854775808").unwrap(),
            i64::MIN
        );
        assert!(signed_number::<i64>("-0").is_err());
        assert_eq!(
            decimal_units("12.50").unwrap(),
            decimal_units("12.500000000000000000").unwrap()
        );
        assert_eq!(decimal_units("-0.000000000000000001").unwrap(), [255; 32]);
        for s in ["NaN", "1e3", "0.0000000000000000001", "-0.00", "1."] {
            assert!(decimal_units(s).is_err(), "{s}");
        }
        assert_eq!(timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            timestamp("2027-01-01T00:00:00Z").unwrap(),
            1_798_761_600_000
        );
        assert!(timestamp("2000-02-29T12:00:00Z").is_ok());
        for s in [
            "1969-12-31T23:59:59Z",
            "2026-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2027-01-01T00:00:60Z",
            "2027-01-01T00:00:00+00:00",
        ] {
            assert!(timestamp(s).is_err(), "{s}");
        }
        assert!(fixed_hex::<1>("0xAF").is_err());
        assert_eq!(fixed_hex::<1>("0xaf").unwrap(), [175]);
    }

    #[test]
    fn supplied_pins_are_assertions_even_during_refresh() {
        let fixture = Fixture::new();
        lock(&fixture.chain(), false).unwrap();
        stdfs::write(fixture.0.join("payloads/welcome.txt"), "changed").unwrap();
        assert!(
            lock(&fixture.chain(), true)
                .unwrap_err()
                .contains("pin mismatch")
        );
        assert!(!fixture.bundle().exists());
    }

    #[test]
    fn paths_symlinks_case_collisions_and_caps_fail_closed() {
        for path in [
            "/absolute",
            "../outside",
            "a/../b",
            "a/./b",
            "a//b",
            "a\\b",
            "./a",
            "",
        ] {
            assert!(relative(path).is_err(), "{path}");
        }
        let fixture = Fixture::new();
        let root = open_directory(&fixture.0).unwrap();
        std::os::unix::fs::symlink("welcome.txt", fixture.0.join("payloads/link.txt")).unwrap();
        assert!(read_relative(&root, "payloads/link.txt", DOCUMENT_CAP).is_err());
        std::os::unix::fs::symlink("payloads", fixture.0.join("linked-directory")).unwrap();
        assert!(read_relative(&root, "linked-directory/welcome.txt", DOCUMENT_CAP).is_err());
        let mut inputs = Inputs {
            root,
            pins: Vec::new(),
            paths: BTreeSet::new(),
            total: 0,
        };
        inputs.read("payloads/welcome.txt", None, None).unwrap();
        assert!(
            inputs
                .read("payloads/Welcome.txt", None, None)
                .unwrap_err()
                .contains("case-colliding")
        );
        stdfs::write(fixture.0.join("large"), vec![0; DOCUMENT_CAP + 1]).unwrap();
        assert!(inputs.read("large", None, None).is_err());
        inputs.total = INPUT_CAP - 1;
        assert!(inputs.read("payloads/status.txt", None, None).is_err());
    }

    #[test]
    fn normalized_stream_cap_is_enforced_before_publication() {
        let fixture = Fixture::new();
        fixture.change(
            &fixture.inline(),
            "max_canonical_bytes = \"1048576\"",
            "max_canonical_bytes = \"20\"",
        );
        assert!(
            build(&fixture.inline(), &fixture.bundle(), false)
                .unwrap_err()
                .contains("canonical byte cap")
        );
        assert!(!fixture.bundle().exists());
    }

    #[test]
    fn verify_rebuilds_and_rejects_artifact_tampering() {
        let fixture = Fixture::new();
        build(&fixture.inline(), &fixture.bundle(), false).unwrap();
        let report = fixture.bundle().join("report.json");
        let original = stdfs::read(&report).unwrap();
        stdfs::write(&report, b"{}").unwrap();
        assert!(
            verify(&fixture.bundle())
                .unwrap_err()
                .contains("report differs")
        );
        stdfs::write(&report, original).unwrap();
        let lock_path = fixture.bundle().join(LOCK_FILE);
        let mut lock: Lock = parse_toml(&stdfs::read(&lock_path).unwrap(), LOCK_FILE).unwrap();
        lock.expanded_profile.chain_params.max_lifetime = "6307199".into();
        lock.expanded_profile_sha256 =
            sha256(toml::to_string(&lock.expanded_profile).unwrap().as_bytes());
        stdfs::write(&lock_path, toml::to_string(&lock).unwrap()).unwrap();
        assert!(
            verify(&fixture.bundle())
                .unwrap_err()
                .contains("profile differs")
        );
    }

    // Avoid requiring a Debug implementation on the compiled engine result.
    trait ErrorString {
        fn unwrap_err_string(self) -> String;
    }
    impl<T> ErrorString for Result<T> {
        fn unwrap_err_string(self) -> String {
            match self {
                Ok(_) => panic!("expected error"),
                Err(e) => e,
            }
        }
    }
}
