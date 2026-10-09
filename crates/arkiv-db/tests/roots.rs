//! State roots of the engine (node step N3b) against an independent model.
//!
//! What this file establishes:
//! - **Oracle roots.** Over a seeded multi-block history on four namespaces
//!   (raw puts and deletes including the empty key, typed creates, patches
//!   that change values, types and kinds, deletes, transfers, host-state
//!   puts and removes, pricing changes, rolled-back transactions and empty
//!   blocks), every map root the engine reports (each namespace's raw,
//!   key-binding, row and term maps, the catalog and the host map) equals
//!   the root of an independent Merkle Patricia construction over leaves
//!   built by hand in this file from a `BTreeMap` model of the state, and
//!   the state root equals `keccak256(len ‖ "arkiv/state/v1" ‖ header)` over
//!   a `StateHeader` encoded by hand. The construction is the yellow-paper
//!   reference of `arkiv-db-trie`'s tests (any key length), cross-checked
//!   here against `alloy_trie::HashBuilder` for every map whose keys have at
//!   most 32 bytes. Nothing on the expected side calls the engine's codec,
//!   layouts or digest.
//! - **History independence.** Identical logical content reached by
//!   different histories (other operation orders, other block boundaries,
//!   rolled-back transactions, transfers there and back through different
//!   owners, raw keys deleted and written again, patches undone) has the
//!   same state root and the same map roots.
//! - **Retention.** Every published snapshot's state root is unchanged
//!   after all later blocks, and still equals the model's root of its block.
//! - **Sensitivity.** The root changes when any logical field changes, each
//!   alone: a descriptor's owner, the pricing schedule, the record
//!   allocator (a create rolled forward and deleted again), a host entry and
//!   one cell value. Each changed root equals the model's.
//! - The sealed candidate's root equals the root of the snapshot it
//!   publishes, and `state_header().root()` is `state_root()`.
//! - An open branch's `try_state_root` equals the model's root after every
//!   transaction (asking does not change later roots), and the seal root.
//!
//! What it does not establish: roots of states read from storage (see
//! `storage_faults.rs`), proofs (node step N3d), or anything about header
//! v2 (a later node step).

#[path = "../../arkiv-db-trie/tests/support/mod.rs"]
mod trie_support;

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{B256, keccak256};
use arkiv_db::{
    Branch, Budget, Cell, CellName, Effect, Engine, NamespaceId, NamespaceName, NamespaceRoots,
    OwnerId, PriceSchedule, RecordKey, Revision, SealedCandidate, Snapshot, SnapshotId, Value,
    WriteOp,
};
use trie_support::{Rng, hash_builder_root, reference};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);
const CAROL: OwnerId = OwnerId::new([0xc0; 20]);
const OWNERS: [OwnerId; 3] = [ALICE, BOB, CAROL];

/// Every schedule in this file prices every class the same; all budgets are
/// unlimited, so prices only matter as state.
fn schedule(id: u32, weight: u64) -> PriceSchedule {
    PriceSchedule::uniform(id, weight)
}

// ---------------------------------------------------------------------------
// The model: logical state only, as plain maps
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum V {
    Bool(bool),
    I64(i64),
    U64(u64),
    Str(String),
    Bytes(Vec<u8>),
}

impl V {
    /// The type id: Bool 1, I64 2, U64 3, Str 4, Bytes 5.
    fn tag(&self) -> u8 {
        match self {
            V::Bool(_) => 1,
            V::Str(_) => 2,
            V::Bytes(_) => 3,
            V::U64(_) => 13,
            V::I64(_) => 17,
        }
    }

    fn engine(&self) -> Value {
        match self {
            V::Bool(flag) => Value::Bool(*flag),
            V::I64(value) => Value::I64(*value),
            V::U64(value) => Value::U64(*value),
            V::Str(text) => Value::str(text),
            V::Bytes(bytes) => Value::bytes(bytes),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct C {
    attribute: bool,
    value: V,
}

impl C {
    fn engine(&self) -> Cell {
        if self.attribute {
            Cell::attribute(self.value.engine())
        } else {
            Cell::field(self.value.engine())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NsModel {
    id: u64,
    name: String,
    owner: OwnerId,
    revision: u64,
    next_record: u64,
    raw: BTreeMap<Vec<u8>, Vec<u8>>,
    /// id → (key, cells by name).
    rows: BTreeMap<u64, (Vec<u8>, BTreeMap<String, C>)>,
}

impl NsModel {
    fn bound(&self, key: &[u8]) -> Option<u64> {
        self.rows
            .iter()
            .find(|(_, (bound, _))| bound == key)
            .map(|(id, _)| *id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    namespaces: BTreeMap<u64, NsModel>,
    next_namespace: u64,
    /// (schedule id, uniform weight).
    pricing: (u32, u64),
    host: BTreeMap<[u8; 32], Vec<u8>>,
}

impl Model {
    fn new(pricing: (u32, u64)) -> Self {
        Self {
            namespaces: BTreeMap::new(),
            next_namespace: 1,
            pricing,
            host: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Leaves built by hand from the layout of state format 1
// ---------------------------------------------------------------------------

fn be32(value: usize) -> [u8; 4] {
    u32::try_from(value).unwrap().to_be_bytes()
}

/// `u32 len ‖ bytes`.
fn prefixed(bytes: &[u8]) -> Vec<u8> {
    let mut out = be32(bytes.len()).to_vec();
    out.extend_from_slice(bytes);
    out
}

/// `bytes ‖ 0x00 pad to width ‖ len`.
fn padded(bytes: &[u8], width: usize) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out.resize(width, 0);
    out.push(u8::try_from(bytes.len()).unwrap());
    out
}

/// A cell value in the codec: tag, then text and bytes length-prefixed,
/// every other type its term bytes.
fn value_bytes(value: &V) -> Vec<u8> {
    let mut out = vec![value.tag()];
    match value {
        V::Str(text) => out.extend(prefixed(text.as_bytes())),
        V::Bytes(bytes) => out.extend(prefixed(bytes)),
        fixed => out.extend(term_bytes(fixed)),
    }
    out
}

/// The order-preserving term bytes: Bool 00/01, I64 with the sign bit
/// flipped, U64 big-endian, Str its UTF-8.
fn term_bytes(value: &V) -> Vec<u8> {
    match value {
        V::Bool(flag) => vec![u8::from(*flag)],
        V::I64(number) => ((*number as u64) ^ (1 << 63)).to_be_bytes().to_vec(),
        V::U64(number) => number.to_be_bytes().to_vec(),
        V::Str(text) => text.as_bytes().to_vec(),
        V::Bytes(_) => unreachable!("bytes are never indexed"),
    }
}

/// `name ‖ pad to 32 ‖ name_len ‖ tag ‖ presence ‖ value ‖ pad to 128 ‖
/// value_len`: 164 bytes.
fn term_key(name: &str, tag: u8, value: Option<&[u8]>) -> Vec<u8> {
    let mut key = padded(
        name.as_bytes(),
        32,
    );
    key.push(tag);
    match value {
        None => {
            key.push(0);
            key.extend(padded(
                &[],
                128,
            ));
        }
        Some(bytes) => {
            key.push(1);
            key.extend(padded(
                bytes,
                128,
            ));
        }
    }
    assert_eq!(key.len(), 164);
    key
}

/// The root of a map, from the reference construction; for keys of at most
/// 32 bytes also checked against `HashBuilder`.
fn root(entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> B256 {
    let expected = reference::root(entries);
    if entries.keys().next().is_some_and(|key| key.len() <= 32) {
        assert_eq!(
            hash_builder_root(entries),
            expected,
            "reference vs HashBuilder"
        );
    }
    expected
}

/// `root ‖ len u64`.
fn handle(root: B256, len: usize) -> Vec<u8> {
    let mut out = root.to_vec();
    out.extend(u64::try_from(len).unwrap().to_be_bytes());
    out
}

/// The roots of one namespace's maps, from its model.
fn namespace_roots(ns: &NsModel) -> NamespaceRoots {
    let raw: BTreeMap<Vec<u8>, Vec<u8>> = ns
        .raw
        .iter()
        .map(|(key, value)| (padded(key, 20), prefixed(value)))
        .collect();
    let keys: BTreeMap<Vec<u8>, Vec<u8>> = ns
        .rows
        .iter()
        .map(|(id, (key, _))| {
            let mut encoded = vec![u8::try_from(key.len()).unwrap()];
            encoded.extend(key);
            encoded.resize(33, 0);
            (encoded, id.to_be_bytes().to_vec())
        })
        .collect();
    let rows: BTreeMap<Vec<u8>, Vec<u8>> = ns
        .rows
        .iter()
        .map(|(id, (key, cells))| {
            let mut value = vec![u8::try_from(key.len()).unwrap()];
            value.extend(key);
            value.extend(be32(cells.len()));
            for (name, cell) in cells {
                value.extend(prefixed(name.as_bytes()));
                value.push(if cell.attribute { 1 } else { 2 });
                value.extend(value_bytes(&cell.value));
            }
            (id.to_be_bytes().to_vec(), value)
        })
        .collect();
    let mut postings: BTreeMap<Vec<u8>, BTreeSet<u64>> = BTreeMap::new();
    for (id, (_, cells)) in &ns.rows {
        for (name, cell) in cells.iter().filter(|(_, cell)| cell.attribute) {
            let tag = cell.value.tag();
            let value = term_bytes(&cell.value);
            for key in [term_key(name, tag, Some(&value)), term_key(name, tag, None)] {
                postings.entry(key).or_default().insert(*id);
            }
        }
    }
    let terms: BTreeMap<Vec<u8>, Vec<u8>> = postings
        .iter()
        .map(|(key, ids)| {
            let members: BTreeMap<Vec<u8>, Vec<u8>> = ids
                .iter()
                .map(|id| (id.to_be_bytes().to_vec(), vec![1]))
                .collect();
            (key.clone(), handle(root(&members), members.len()))
        })
        .collect();
    let pair = |entries: &BTreeMap<Vec<u8>, Vec<u8>>| (root(entries), entries.len() as u64);
    NamespaceRoots {
        next_record: ns.next_record,
        records: pair(&raw),
        keys: pair(&keys),
        rows: pair(&rows),
        terms: pair(&terms),
    }
}

/// The catalog leaf: `id ‖ name ‖ owner ‖ revision ‖ next_record ‖ four
/// map handles`.
fn catalog_leaf(ns: &NsModel, roots: &NamespaceRoots) -> Vec<u8> {
    let mut leaf = ns.id.to_be_bytes().to_vec();
    leaf.extend(prefixed(ns.name.as_bytes()));
    leaf.extend(ns.owner.as_bytes());
    leaf.extend(ns.revision.to_be_bytes());
    leaf.extend(ns.next_record.to_be_bytes());
    for (root, len) in [roots.records, roots.keys, roots.rows, roots.terms] {
        leaf.extend(handle(root, usize::try_from(len).unwrap()));
    }
    leaf
}

/// Everything the model predicts about one state.
#[derive(Debug, PartialEq, Eq)]
struct Roots {
    namespaces: BTreeMap<u64, NamespaceRoots>,
    catalog: (B256, u64),
    host: (B256, u64),
    header: Vec<u8>,
    state: B256,
}

fn model_roots(model: &Model) -> Roots {
    let mut namespaces = BTreeMap::new();
    let mut catalog = BTreeMap::new();
    for (id, ns) in &model.namespaces {
        let roots = namespace_roots(ns);
        catalog.insert(id.to_be_bytes().to_vec(), catalog_leaf(ns, &roots));
        namespaces.insert(*id, roots);
    }
    let host: BTreeMap<Vec<u8>, Vec<u8>> = model
        .host
        .iter()
        .map(|(key, value)| (key.to_vec(), prefixed(value)))
        .collect();
    let catalog = (root(&catalog), catalog.len() as u64);
    let host = (root(&host), host.len() as u64);
    // StateHeader: format 1 ‖ catalog root ‖ catalog len ‖ next namespace
    // ‖ pricing (id ‖ 31 ‖ 31 weights) ‖ host root ‖ host len.
    let mut header = 1u32
        .to_be_bytes()
        .to_vec();
    header.extend(handle(catalog.0, catalog.1 as usize));
    header.extend(model.next_namespace.to_be_bytes());
    header.extend(model.pricing.0.to_be_bytes());
    header.extend(31u32.to_be_bytes());
    for _ in 0..31 {
        header.extend(model.pricing.1.to_be_bytes());
    }
    header.extend(handle(host.0, host.1 as usize));
    let domain = b"arkiv/state/v1";
    let mut preimage = vec![u8::try_from(domain.len()).unwrap()];
    preimage.extend(domain);
    preimage.extend(&header);
    Roots {
        namespaces,
        catalog,
        host,
        header,
        state: keccak256(preimage),
    }
}

/// What the engine reports about one state, in the same shape.
fn engine_roots(snapshot: &Snapshot, namespaces: impl Iterator<Item = u64>) -> Roots {
    let header = snapshot.state_header();
    assert_eq!(header.root(), snapshot.state_root());
    Roots {
        namespaces: namespaces
            .map(|id| {
                let roots = snapshot
                    .try_namespace_roots(ns(id))
                    .expect("in memory")
                    .expect("the namespace exists");
                (id, roots)
            })
            .collect(),
        catalog: (header.catalog_root, header.catalog_len),
        host: (header.host_root, header.host_len),
        header: arkiv_db::encode(&header),
        state: snapshot.state_root(),
    }
}

fn check(snapshot: &Snapshot, model: &Model, context: &str) {
    let want = model_roots(model);
    let got = engine_roots(snapshot, model.namespaces.keys().copied());
    for (id, roots) in &want.namespaces {
        assert_eq!(got.namespaces[id], *roots, "{context}: ns {id} map roots");
    }
    assert_eq!(got.catalog, want.catalog, "{context}: catalog root");
    assert_eq!(got.host, want.host, "{context}: host root");
    assert_eq!(got.header, want.header, "{context}: state header bytes");
    assert_eq!(got.state, want.state, "{context}: state root");
}

// ---------------------------------------------------------------------------
// Driving the engine and the model together
// ---------------------------------------------------------------------------

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_ordinal(ordinal).unwrap()
}

fn cell_name(text: &str) -> CellName {
    CellName::new(text).unwrap()
}

fn record_key(bytes: &[u8]) -> RecordKey {
    RecordKey::new(bytes).unwrap()
}

/// One logical change, applied to the engine and to the model alike.
#[derive(Clone, Debug)]
enum Op {
    CreateNamespace(&'static str),
    Transfer(u64, OwnerId),
    Put(u64, Vec<u8>, Vec<u8>),
    Delete(u64, Vec<u8>),
    Create(u64, Vec<u8>, Vec<(&'static str, C)>),
    Patch(u64, Vec<u8>, Vec<(&'static str, C)>, Vec<&'static str>),
    DeleteRecord(u64, Vec<u8>),
    HostPut([u8; 32], Vec<u8>),
    HostRemove([u8; 32]),
    Pricing(u32, u64),
}

/// Applies `op` to the branch and expects it to apply.
fn run(branch: &mut Branch, model: &Model, op: &Op) {
    let owner = |id: u64| model.namespaces[&id].owner;
    let cells = |cells: &[(&'static str, C)]| -> Vec<(CellName, Cell)> {
        cells
            .iter()
            .map(|(name, cell)| (cell_name(name), cell.engine()))
            .collect()
    };
    let unlimited = Budget::unlimited();
    let executed = match op {
        Op::CreateNamespace(name) => branch.execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: NamespaceName::new(name).unwrap(),
            },
            unlimited,
        ),
        Op::Transfer(id, to) => branch.execute(
            owner(*id),
            WriteOp::TransferNamespace {
                namespace: ns(*id),
                expected_revision: Revision(model.namespaces[id].revision),
                new_owner: *to,
            },
            unlimited,
        ),
        Op::Put(id, key, value) => branch.execute(
            owner(*id),
            WriteOp::Put {
                namespace: ns(*id),
                key: key.clone(),
                value: value.clone(),
            },
            unlimited,
        ),
        Op::Delete(id, key) => branch.execute(
            owner(*id),
            WriteOp::Delete {
                namespace: ns(*id),
                key: key.clone(),
            },
            unlimited,
        ),
        Op::Create(id, key, set) => branch.execute(
            owner(*id),
            WriteOp::CreateRecord {
                namespace: ns(*id),
                key: record_key(key),
                cells: cells(set),
            },
            unlimited,
        ),
        Op::Patch(id, key, set, remove) => branch.execute(
            owner(*id),
            WriteOp::PatchRecord {
                namespace: ns(*id),
                key: record_key(key),
                set: cells(set),
                remove: remove.iter().map(|name| cell_name(name)).collect(),
            },
            unlimited,
        ),
        Op::DeleteRecord(id, key) => branch.execute(
            owner(*id),
            WriteOp::DeleteRecord {
                namespace: ns(*id),
                key: record_key(key),
            },
            unlimited,
        ),
        Op::HostPut(key, value) => {
            branch
                .try_host_put(*key, value)
                .expect("in memory")
                .expect("host values fit");
            return;
        }
        Op::HostRemove(key) => {
            branch.try_host_remove(key).expect("in memory");
            return;
        }
        Op::Pricing(id, weight) => {
            branch
                .set_pricing(schedule(*id, *weight))
                .expect("schedule ids increase");
            return;
        }
    };
    let executed = executed.unwrap_or_else(|rejected| panic!("{op:?} rejected: {rejected}"));
    if let Op::CreateNamespace(_) = op {
        assert_eq!(
            executed.effect,
            Effect::NamespaceCreated(ns(model.next_namespace))
        );
    }
}

/// Applies `op` to the model.
fn apply(model: &mut Model, op: &Op) {
    match op {
        Op::CreateNamespace(name) => {
            let id = model.next_namespace;
            model.next_namespace += 1;
            model.namespaces.insert(
                id,
                NsModel {
                    id,
                    name: (*name).to_owned(),
                    owner: ALICE,
                    revision: 0,
                    next_record: 1,
                    raw: BTreeMap::new(),
                    rows: BTreeMap::new(),
                },
            );
        }
        Op::Transfer(id, to) => {
            let ns = model.namespaces.get_mut(id).unwrap();
            ns.owner = *to;
            ns.revision += 1;
        }
        Op::Put(id, key, value) => {
            model
                .namespaces
                .get_mut(id)
                .unwrap()
                .raw
                .insert(key.clone(), value.clone());
        }
        Op::Delete(id, key) => {
            model.namespaces.get_mut(id).unwrap().raw.remove(key);
        }
        Op::Create(id, key, set) => {
            let ns = model.namespaces.get_mut(id).unwrap();
            let cells = set
                .iter()
                .map(|(name, cell)| ((*name).to_owned(), cell.clone()))
                .collect();
            ns.rows.insert(ns.next_record, (key.clone(), cells));
            ns.next_record += 1;
        }
        Op::Patch(id, key, set, remove) => {
            let ns = model.namespaces.get_mut(id).unwrap();
            let record = ns.bound(key).unwrap();
            let cells = &mut ns.rows.get_mut(&record).unwrap().1;
            for (name, cell) in set {
                cells.insert((*name).to_owned(), cell.clone());
            }
            for name in remove {
                cells.remove(*name);
            }
        }
        Op::DeleteRecord(id, key) => {
            let ns = model.namespaces.get_mut(id).unwrap();
            let record = ns.bound(key).unwrap();
            ns.rows.remove(&record);
        }
        Op::HostPut(key, value) => {
            model.host.insert(*key, value.clone());
        }
        Op::HostRemove(key) => {
            model.host.remove(key);
        }
        Op::Pricing(id, weight) => model.pricing = (*id, *weight),
    }
}

/// A block of transactions; each transaction applies in full or, when
/// marked, is rolled back after running.
struct Block {
    transactions: Vec<(Vec<Op>, bool)>,
}

/// Runs `block` on a branch from the head, checks the sealed candidate's
/// root, publishes it and checks the published snapshot.
fn produce(engine: &mut Engine, model: &mut Model, block: &Block, context: &str) -> SnapshotId {
    let mut branch = engine.begin(engine.head(), None).unwrap();
    for (ops, rolled_back) in &block.transactions {
        let checkpoint = branch.checkpoint();
        let mut shadow = model.clone();
        for op in ops {
            run(&mut branch, &shadow, op);
            apply(&mut shadow, op);
        }
        if *rolled_back {
            branch.rollback_to(checkpoint).unwrap();
        } else {
            *model = shadow;
        }
        // The open branch's root between transactions, before further writes.
        assert_eq!(
            branch.try_state_root().unwrap(),
            model_roots(model).state,
            "{context}: open branch root"
        );
    }
    let open_root = branch.try_state_root().unwrap();
    let sealed: SealedCandidate = branch.seal();
    let root = sealed.state_root();
    assert_eq!(
        open_root, root,
        "{context}: open branch root is the seal root"
    );
    assert_eq!(sealed.state_header().root(), root);
    let id = engine.publish(&sealed).unwrap();
    let snapshot = engine.snapshot(id).unwrap();
    assert_eq!(snapshot.state_root(), root, "{context}: published root");
    check(&snapshot, model, context);
    id
}

fn attr(value: V) -> C {
    C {
        attribute: true,
        value,
    }
}

fn field(value: V) -> C {
    C {
        attribute: false,
        value,
    }
}

fn text(value: &str) -> V {
    V::Str(value.to_owned())
}

// ---------------------------------------------------------------------------
// A seeded history
// ---------------------------------------------------------------------------

const RAW_KEYS: [&[u8]; 8] = [
    b"",
    b"\x00",
    b"\x00\x00",
    b"a",
    b"a\x00",
    b"ab",
    b"\xff",
    &[7; 20],
];
const RECORD_KEYS: [&[u8]; 6] = [b"r1", b"r2", b"r3", b"\x00", &[0xff; 32], &[0; 32]];
const NAMES: [&str; 4] = ["kind", "n", "$exp", "note"];

fn random_value(rng: &mut Rng) -> V {
    match rng.below(6) {
        0 => V::Bool(rng.below(2) == 0),
        1 => V::I64([i64::MIN, -1, 0, 1, i64::MAX][rng.below(5)]),
        2 => V::U64(rng.below(4) as u64),
        3 => text(["", "a", "ab", &"z".repeat(64)][rng.below(4)]),
        4 => text(["red", "rose"][rng.below(2)]),
        _ => V::Bytes(vec![1; rng.below(3)]),
    }
}

fn random_cell(rng: &mut Rng) -> C {
    let value = random_value(rng);
    if matches!(value, V::Bytes(_)) || rng.below(4) == 0 {
        field(value)
    } else {
        attr(value)
    }
}

/// One random operation valid in `model`.
fn random_op(rng: &mut Rng, model: &Model, pricing_id: &mut u32) -> Op {
    let ids: Vec<u64> = model.namespaces.keys().copied().collect();
    let id = ids[rng.below(ids.len())];
    let ns = &model.namespaces[&id];
    let key = |rng: &mut Rng| RECORD_KEYS[rng.below(RECORD_KEYS.len())].to_vec();
    match rng.below(12) {
        0 | 1 => Op::Put(
            id,
            RAW_KEYS[rng.below(RAW_KEYS.len())].to_vec(),
            vec![b'v'; rng.below(3)],
        ),
        2 => Op::Delete(id, RAW_KEYS[rng.below(RAW_KEYS.len())].to_vec()),
        3..=5 => {
            let key = key(rng);
            match ns.bound(&key) {
                None => {
                    let mut names: Vec<&str> = NAMES.to_vec();
                    rng.shuffle(&mut names);
                    let count = rng.below(names.len() + 1);
                    let cells = names[..count]
                        .iter()
                        .map(|name| (*name, random_cell(rng)))
                        .collect();
                    Op::Create(id, key, cells)
                }
                Some(_) => Op::DeleteRecord(id, key),
            }
        }
        6..=8 => {
            let key = key(rng);
            match ns.bound(&key) {
                None => Op::Put(id, key[..key.len().min(20)].to_vec(), b"p".to_vec()),
                Some(_) => {
                    let set_name = NAMES[rng.below(NAMES.len())];
                    let removed = NAMES[rng.below(NAMES.len())];
                    let remove = if removed == set_name {
                        Vec::new()
                    } else {
                        vec![removed]
                    };
                    Op::Patch(id, key, vec![(set_name, random_cell(rng))], remove)
                }
            }
        }
        9 => Op::Transfer(id, OWNERS[rng.below(OWNERS.len())]),
        10 => {
            let mut host = [0u8; 32];
            host[31] = rng.below(4) as u8;
            if rng.below(2) == 0 {
                Op::HostPut(host, vec![0x5a; rng.below(3)])
            } else {
                Op::HostRemove(host)
            }
        }
        _ => {
            *pricing_id += 1;
            Op::Pricing(*pricing_id, 1 + rng.below(3) as u64)
        }
    }
}

#[test]
fn every_map_root_and_the_state_root_match_the_independent_model() {
    let mut engine = Engine::new(schedule(1, 1));
    let mut model = Model::new((1, 1));
    let genesis = engine.head_snapshot();
    check(&genesis, &model, "genesis");
    let mut published: Vec<(SnapshotId, Model)> = vec![(genesis.id(), model.clone())];

    // Block 1: four namespaces; the fourth stays empty forever.
    let setup = Block {
        transactions: vec![(
            vec![
                Op::CreateNamespace("alpha"),
                Op::CreateNamespace("beta"),
                Op::CreateNamespace("gamma"),
                Op::CreateNamespace("empty"),
            ],
            false,
        )],
    };
    let id = produce(&mut engine, &mut model, &setup, "block 1");
    published.push((id, model.clone()));

    let mut rng = Rng(0x5eed_4007);
    let mut pricing_id = 1;
    // How many operations of each kind applied, and how many rolled back.
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for height in 2..=40u32 {
        let mut block = Block {
            transactions: Vec::new(),
        };
        // Every fifth block is empty.
        if height % 5 != 0 {
            let mut shadow = model.clone();
            for _ in 0..1 + rng.below(4) {
                let rolled_back = rng.below(5) == 0;
                let mut ops = Vec::new();
                let mut scratch = shadow.clone();
                for _ in 0..1 + rng.below(4) {
                    let op = random_op(&mut rng, &scratch_without_empty(&scratch), &mut pricing_id);
                    apply(&mut scratch, &op);
                    ops.push(op);
                }
                for op in &ops {
                    let kind = if rolled_back {
                        "rolled back"
                    } else {
                        op_kind(op)
                    };
                    *kinds.entry(kind).or_default() += 1;
                }
                if !rolled_back {
                    shadow = scratch;
                }
                block.transactions.push((ops, rolled_back));
            }
        }
        let context = format!("block {height}");
        let before = model.clone();
        let id = produce(&mut engine, &mut model, &block, &context);
        if block.transactions.is_empty() {
            assert_eq!(model, before);
            let previous = engine.snapshot(published.last().unwrap().0).unwrap();
            assert_eq!(
                engine.snapshot(id).unwrap().state_root(),
                previous.state_root(),
                "{context}: an empty block keeps the root"
            );
        }
        published.push((id, model.clone()));
    }

    // Retention: every snapshot, pinned afresh, still has its block's root.
    for (id, model) in &published {
        let snapshot = engine.snapshot(*id).unwrap();
        check(&snapshot, model, &format!("{id} after all blocks"));
    }
    // The history exercised what it should.
    for kind in [
        "put",
        "delete",
        "create",
        "patch",
        "delete record",
        "transfer",
        "host put",
        "host remove",
        "pricing",
        "rolled back",
    ] {
        assert!(
            kinds.get(kind).is_some_and(|count| *count > 0),
            "{kind}: {kinds:?}"
        );
    }
    let last = &published.last().unwrap().1;
    assert!(last.namespaces[&4].raw.is_empty() && last.namespaces[&4].rows.is_empty());
    assert!(last.pricing.0 > 1, "pricing changed");
    assert!(
        last.namespaces.values().any(|ns| ns.revision > 0),
        "a transfer"
    );
    assert!(
        last.namespaces
            .values()
            .any(|ns| ns.next_record > ns.rows.len() as u64 + 1),
        "a record was deleted"
    );
}

fn op_kind(op: &Op) -> &'static str {
    match op {
        Op::CreateNamespace(_) => "create namespace",
        Op::Transfer(..) => "transfer",
        Op::Put(..) => "put",
        Op::Delete(..) => "delete",
        Op::Create(..) => "create",
        Op::Patch(..) => "patch",
        Op::DeleteRecord(..) => "delete record",
        Op::HostPut(..) => "host put",
        Op::HostRemove(..) => "host remove",
        Op::Pricing(..) => "pricing",
    }
}

/// The model without the namespace that must stay empty, for op choice.
fn scratch_without_empty(model: &Model) -> Model {
    let mut view = model.clone();
    view.namespaces.remove(&4);
    view
}

// ---------------------------------------------------------------------------
// History independence
// ---------------------------------------------------------------------------

fn cells_one() -> Vec<(&'static str, C)> {
    vec![
        ("kind", attr(text("red"))),
        ("n", attr(V::I64(-3))),
        ("note", field(V::Bytes(vec![1, 2]))),
    ]
}

fn cells_two() -> Vec<(&'static str, C)> {
    vec![("kind", attr(text("rose"))), ("$exp", attr(V::U64(9)))]
}

/// Runs blocks and returns the head snapshot.
fn build(blocks: &[Block]) -> Snapshot {
    let mut engine = Engine::new(schedule(1, 1));
    let mut model = Model::new((1, 1));
    for (index, block) in blocks.iter().enumerate() {
        produce(
            &mut engine,
            &mut model,
            block,
            &format!("block {}", index + 1),
        );
    }
    engine.head_snapshot()
}

fn one(ops: Vec<Op>) -> Block {
    Block {
        transactions: vec![(ops, false)],
    }
}

#[test]
fn identical_content_reached_by_different_histories_has_one_root() {
    let host = [3u8; 32];
    // History A: everything in one block, in a natural order.
    let a = build(&[one(vec![
        Op::CreateNamespace("alpha"),
        Op::CreateNamespace("beta"),
        Op::Put(1, b"k1".to_vec(), b"one".to_vec()),
        Op::Put(1, b"k2".to_vec(), b"two".to_vec()),
        Op::Create(1, b"r1".to_vec(), cells_one()),
        Op::Create(1, b"r2".to_vec(), cells_two()),
        Op::Transfer(2, BOB),
        Op::Transfer(2, ALICE),
        Op::HostPut(host, b"due".to_vec()),
    ])]);
    // History B: other blocks and orders, a rolled-back transaction, a
    // transfer round trip through another owner, a raw key deleted and
    // written again, a patch undone, and the same record ids.
    let b = build(&[
        one(vec![
            Op::CreateNamespace("alpha"),
            Op::CreateNamespace("beta"),
        ]),
        Block {
            transactions: vec![
                (
                    vec![
                        Op::Create(1, b"r1".to_vec(), cells_one()),
                        Op::Put(1, b"junk".to_vec(), b"x".to_vec()),
                        Op::HostPut([9; 32], b"x".to_vec()),
                    ],
                    true,
                ),
                (
                    vec![
                        Op::Put(1, b"k2".to_vec(), b"two".to_vec()),
                        Op::Transfer(2, CAROL),
                        Op::Create(1, b"r1".to_vec(), cells_one()),
                    ],
                    false,
                ),
            ],
        },
        one(vec![
            Op::Put(1, b"k1".to_vec(), b"old".to_vec()),
            Op::Delete(1, b"k1".to_vec()),
            Op::Put(1, b"k1".to_vec(), b"one".to_vec()),
            Op::Create(1, b"r2".to_vec(), cells_two()),
            Op::Patch(
                1,
                b"r2".to_vec(),
                vec![("kind", attr(text("ruby")))],
                vec![],
            ),
            Op::Patch(
                1,
                b"r2".to_vec(),
                vec![("kind", attr(text("rose")))],
                vec![],
            ),
            Op::Transfer(2, ALICE),
            Op::HostPut(host, b"due".to_vec()),
        ]),
    ]);
    assert_eq!(a.state_header(), b.state_header());
    assert_eq!(a.state_root(), b.state_root());
    for id in [1, 2] {
        assert_eq!(
            a.try_namespace_roots(ns(id)).unwrap(),
            b.try_namespace_roots(ns(id)).unwrap(),
            "ns {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// Sensitivity
// ---------------------------------------------------------------------------

#[test]
fn the_root_changes_with_every_logical_field_and_matches_the_model() {
    let base_ops = vec![
        Op::CreateNamespace("alpha"),
        Op::Put(1, b"k".to_vec(), b"v".to_vec()),
        Op::Create(1, b"r1".to_vec(), cells_one()),
        Op::HostPut([1; 32], b"h".to_vec()),
    ];
    let mut engine = Engine::new(schedule(1, 1));
    let mut model = Model::new((1, 1));
    produce(&mut engine, &mut model, &one(base_ops), "base");
    let base = engine.head_snapshot();
    let base_model = model.clone();

    let variants: Vec<(&str, Vec<Op>)> = vec![
        ("owner", vec![Op::Transfer(1, BOB)]),
        ("pricing", vec![Op::Pricing(2, 1)]),
        (
            "record allocator",
            vec![
                Op::Create(1, b"r2".to_vec(), cells_two()),
                Op::DeleteRecord(1, b"r2".to_vec()),
            ],
        ),
        ("host entry", vec![Op::HostPut([1; 32], b"H".to_vec())]),
        (
            "cell value",
            vec![Op::Patch(
                1,
                b"r1".to_vec(),
                vec![("n", attr(V::I64(-4)))],
                vec![],
            )],
        ),
        ("raw value", vec![Op::Put(1, b"k".to_vec(), b"w".to_vec())]),
    ];
    let mut roots = BTreeSet::from([base.state_root()]);
    for (what, ops) in variants {
        let mut branch = engine.begin(base.id(), None).unwrap();
        let mut changed = base_model.clone();
        for op in &ops {
            run(&mut branch, &changed, op);
            apply(&mut changed, op);
        }
        let sealed = branch.seal();
        assert_ne!(sealed.state_root(), base.state_root(), "{what}");
        assert_eq!(sealed.state_root(), model_roots(&changed).state, "{what}");
        assert!(roots.insert(sealed.state_root()), "{what}: a distinct root");
        // The allocator variant leaves the rows as they were: only the
        // catalog leaf's allocator differs.
        if what == "record allocator" {
            assert_eq!(changed.namespaces[&1].rows, base_model.namespaces[&1].rows);
            let roots = model_roots(&changed).namespaces[&1];
            let before = model_roots(&base_model).namespaces[&1];
            assert_eq!(
                (roots.keys, roots.rows, roots.terms),
                (before.keys, before.rows, before.terms)
            );
            assert_ne!(roots.next_record, before.next_record);
        }
    }
    // The base snapshot kept its root through all the discarded branches.
    assert_eq!(engine.head_snapshot().state_root(), base.state_root());
    check(&base, &base_model, "base after the variants");
}
