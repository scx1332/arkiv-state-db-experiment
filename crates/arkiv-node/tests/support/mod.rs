//! Shared builders for the node tests: actors, names, keys, cells, request
//! ids, a small genesis, and read helpers over the memory engine's `try_`
//! API (this crate's `clippy.toml` bans the infallible forms everywhere,
//! tests included).

#![allow(dead_code)]

use std::ops::Bound;

use arkiv_db::{
    Budget, Cell, CellName, Entry, Limits, NamespaceDescriptor, NamespaceId, NamespaceName,
    OwnerId, PriceSchedule, Projection, Query, RecordKey, Row, Snapshot, TypeTag, Value,
};
use arkiv_node::{
    AdminAction, AdminInput, BlockInputs, ChainParams, Expiry, FORMAT_VERSION, Genesis,
    PROTOCOL_VERSION, RequestId, TxEnvelope, TxOp,
};

pub const CHAIN: u64 = 7;
pub const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
pub const BOB: OwnerId = OwnerId::new([0xb0; 20]);
pub const CAROL: OwnerId = OwnerId::new([0xca; 20]);
pub const ADMIN: OwnerId = OwnerId::new([0xad; 20]);

/// Small limits: 8 cells, 64-byte values, pages of 4 (so an expiry sweep of
/// five records in one namespace reads two pages).
pub const LIMITS: Limits = Limits {
    namespace_cap: 1 << 32,
    max_cells: 8,
    max_str_len: 64,
    max_bytes_len: 64,
    max_page: 4,
    max_groups: 8,
    max_literals: 8,
    ..Limits::DEFAULT
};

/// Parameters for the fixtures. Under `PriceSchedule::FIXTURE` and
/// `LIMITS` one expiry deletion costs at most `DirectoryLookup 1 +
/// PointRead 3 + RecordDelete 20 + 8 × CellRemove 4 + 3 × RootUpdate 2 +
/// 16 × (IndexRemove 6 + 2 × RootUpdate 2)` = 1 + 3 + 20 + 32 + 6 + 160 =
/// 222, so four expiring records reserve 888 and the block budget of
/// 100,888 is exactly `user_budget + 888`.
pub const PARAMS: ChainParams = ChainParams {
    block_budget: 100_888,
    user_budget: 100_000,
    expiry_per_block: 4,
    max_admin_per_block: 2,
    max_txs_per_block: 16,
    max_ops_per_tx: 8,
    max_ops_per_block: 64,
    max_tx_bytes: 4096,
    max_block_input_bytes: 8 + 2 * 309 + 4096,
    max_lifetime: 100,
    block_period_ms: 2_000,
    max_rent_lifetime_seconds: arkiv_node::gas::DEFAULT_RENT_CAP_SECONDS,
};

pub fn genesis_with(params: ChainParams, limits: Limits, pricing: PriceSchedule) -> Genesis {
    Genesis {
        format_version: FORMAT_VERSION,
        protocol_version: PROTOCOL_VERSION,
        chain_id: CHAIN,
        timestamp_ms: 1_000,
        limits,
        pricing,
        params,
        admins: vec![ADMIN],
    }
}

pub fn genesis() -> Genesis {
    genesis_with(PARAMS, LIMITS, PriceSchedule::FIXTURE)
}

pub fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_ordinal(ordinal).expect("nonzero")
}

pub fn ns_name(text: &str) -> NamespaceName {
    NamespaceName::new(text).expect("valid namespace name")
}

pub fn name(text: &str) -> CellName {
    CellName::new(text).expect("valid cell name")
}

pub fn key(bytes: &[u8]) -> RecordKey {
    RecordKey::new(bytes).expect("valid record key")
}

pub fn attr(value: Value) -> Cell {
    Cell::attribute(value)
}

pub fn field(value: Value) -> Cell {
    Cell::field(value)
}

/// Request id `n`: 24 zero bytes then `n` big-endian.
pub fn request(n: u64) -> RequestId {
    let mut id = [0u8; 32];
    id[24..].copy_from_slice(&n.to_be_bytes());
    id
}

pub fn tx(actor: OwnerId, n: u64, tx_budget: u64, ops: Vec<TxOp>) -> TxEnvelope {
    TxEnvelope {
        chain_id: CHAIN,
        actor,
        request_id: request(n),
        tx_budget,
        ops,
    }
}

pub fn set_pricing(n: u64, schedule: PriceSchedule) -> AdminInput {
    AdminInput {
        actor: ADMIN,
        request_id: request(n),
        action: AdminAction::SetPricing(schedule),
    }
}

pub fn txs(txs: Vec<TxEnvelope>) -> BlockInputs {
    BlockInputs {
        admin: Vec::new(),
        txs,
    }
}

pub fn create_namespace(text: &str) -> TxOp {
    TxOp::CreateNamespace {
        name: ns_name(text),
    }
}

pub fn create(
    namespace: u64,
    key_bytes: &[u8],
    cells: Vec<(&str, Cell)>,
    expiry: Option<Expiry>,
) -> TxOp {
    TxOp::CreateRecord {
        namespace: ns(namespace),
        key: key(key_bytes),
        cells: cells
            .into_iter()
            .map(|(text, cell)| (name(text), cell))
            .collect(),
        expiry,
    }
}

pub fn patch(
    namespace: u64,
    key_bytes: &[u8],
    set: Vec<(&str, Cell)>,
    remove: Vec<&str>,
    expiry: Option<Expiry>,
) -> TxOp {
    TxOp::PatchRecord {
        namespace: ns(namespace),
        key: key(key_bytes),
        set: set
            .into_iter()
            .map(|(text, cell)| (name(text), cell))
            .collect(),
        remove: remove.into_iter().map(name).collect(),
        expiry,
    }
}

pub fn delete(namespace: u64, key_bytes: &[u8]) -> TxOp {
    TxOp::DeleteRecord {
        namespace: ns(namespace),
        key: key(key_bytes),
    }
}

pub fn put(namespace: u64, key_bytes: &[u8], value: &[u8]) -> TxOp {
    TxOp::Put {
        namespace: ns(namespace),
        key: key_bytes.to_vec(),
        value: value.to_vec(),
    }
}

pub const fn until(expires_at: u64) -> Expiry {
    Expiry {
        expires_at,
        min_lifetime: 0,
    }
}

// Reads over the memory engine; its `try_` forms never fail.

pub fn record(snapshot: &Snapshot, namespace: u64, key_bytes: &[u8]) -> Option<Row> {
    snapshot
        .try_get_record(
            ns(namespace),
            &key(key_bytes),
            &Projection::ALL,
            Budget::unlimited(),
        )
        .expect("memory engine")
        .expect("valid read")
        .value
}

pub fn raw(snapshot: &Snapshot, namespace: u64, key_bytes: &[u8]) -> Option<Vec<u8>> {
    snapshot
        .try_get(ns(namespace), key_bytes, Budget::unlimited())
        .expect("memory engine")
        .expect("valid read")
        .value
}

pub fn scan(snapshot: &Snapshot, namespace: u64) -> Vec<Entry> {
    snapshot
        .try_scan_prefix(ns(namespace), b"", Budget::unlimited())
        .expect("memory engine")
        .expect("valid read")
        .value
}

pub fn descriptor(snapshot: &Snapshot, namespace: u64) -> Option<NamespaceDescriptor> {
    snapshot
        .try_descriptor(ns(namespace), Budget::unlimited())
        .expect("memory engine")
        .ok()
        .map(|read| read.value)
}

pub fn host(snapshot: &Snapshot, key: &[u8; 32]) -> Option<Vec<u8>> {
    snapshot.try_host_get(key).expect("memory engine")
}

/// Every host-state entry of a snapshot.
pub fn host_entries(snapshot: &Snapshot) -> Vec<([u8; 32], Vec<u8>)> {
    snapshot
        .try_host_range(&[0u8; 32], usize::MAX)
        .expect("memory engine")
}

/// Every row matching `query`, walked page by page with the instance's
/// largest page.
pub fn query_all(
    snapshot: &Snapshot,
    namespace: u64,
    query: &Query,
    projection: &Projection,
    page: usize,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut cursor = None;
    loop {
        let read = snapshot
            .try_query(
                ns(namespace),
                query,
                projection,
                page,
                cursor.as_ref(),
                Budget::unlimited(),
            )
            .expect("memory engine")
            .expect("valid query")
            .value;
        rows.extend(read.rows);
        match read.next {
            Some(next) => cursor = Some(next),
            None => return rows,
        }
    }
}

/// The smallest `U64` term of `name`, if any.
pub fn smallest_u64(snapshot: &Snapshot, namespace: u64, name_text: &str) -> Option<u64> {
    let terms = snapshot
        .try_terms(
            ns(namespace),
            &name(name_text),
            TypeTag::U64,
            Bound::Unbounded,
            1,
            Budget::unlimited(),
        )
        .expect("memory engine")
        .expect("valid read")
        .value;
    match terms.first() {
        Some(Value::U64(value)) => Some(*value),
        _ => None,
    }
}

/// The host-map key of the due count of `namespace` at `height`, built from
/// the specification: `'D' ‖ height BE ‖ namespace BE ‖ 15 zero bytes`.
pub fn due_key(height: u64, namespace: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[0] = b'D';
    key[1..9].copy_from_slice(&height.to_be_bytes());
    key[9..17].copy_from_slice(&namespace.to_be_bytes());
    key
}

/// The host-map key of the total due count at `height`: `'T' ‖ height BE ‖
/// 23 zero bytes`.
pub fn total_key(height: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[0] = b'T';
    key[1..9].copy_from_slice(&height.to_be_bytes());
    key
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
