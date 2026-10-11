//! Size report for term-key layouts: one workload on a fresh redb
//! `DurableEngine`, then the database file size, the objects and bytes the
//! head reaches, and equality and range proof sizes.
//!
//! Only the stable public API is used, so the same file builds against the
//! layout-1 revision (padded keys) and the layout-2 revision (unpadded,
//! prefix-free keys) for a like-for-like comparison.
//!
//! ```sh
//! cargo run --release -p arkiv-db-redb --example key_size_report -- \
//!     --dir /path/to/scratch --workload mixed --records 20000
//! ```
//!
//! Workloads (each record also has a 64-byte `payload` field):
//! - `short`: three `str` attributes of 3–12 bytes;
//! - `maxlen`: two 128-byte `str` attributes (the default indexed limit);
//! - `scalar`: six scalar attributes (u64 ×2, i64, bool, u256, addr);
//! - `mixed`: two short `str`, two u64, a bool and an addr.
//!
//! The last line is `RESULT key=value ...`.

use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use arkiv_db::durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication};
use arkiv_db::proof::{
    EqQuery, EqRequest, ProofLimits, RangeProofLimits, RangeQuery, RangeRequest, SnapshotRef,
};
use arkiv_db::{
    B256, Budget, Cell, CellName, Effect, NamespaceName, OwnerId, PriceSchedule, RecordKey, Value,
    WriteOp,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const OWNER: OwnerId = OwnerId::new([0xb5; 20]);

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: [&str; 12] = [
    "red",
    "green",
    "blue",
    "cyan",
    "amber",
    "violet",
    "ok",
    "pending",
    "done",
    "archived",
    "x",
    "teal-blue",
];

fn name(text: &str) -> CellName {
    CellName::new(text).expect("valid name")
}

fn long_text(rng: &mut Rng) -> String {
    (0..128)
        .map(|_| char::from(b'a' + rng.below(26) as u8))
        .collect()
}

fn cells(workload: &str, i: u64, rng: &mut Rng) -> Vec<(CellName, Cell)> {
    let word = |rng: &mut Rng| WORDS[rng.below(WORDS.len() as u64) as usize];
    let mut cells = match workload {
        "short" => vec![
            (name("color"), Cell::attribute(Value::str(word(rng)))),
            (name("status"), Cell::attribute(Value::str(word(rng)))),
            (
                name("type"),
                Cell::attribute(Value::str(&format!("t-{:02}", rng.below(40)))),
            ),
        ],
        "maxlen" => vec![
            (name("title"), Cell::attribute(Value::str(&long_text(rng)))),
            (name("tag"), Cell::attribute(Value::str(&long_text(rng)))),
        ],
        "scalar" => {
            let mut word = [0u8; 32];
            word[24..].copy_from_slice(&rng.next().to_be_bytes());
            let mut addr = [0u8; 20];
            addr[12..].copy_from_slice(&rng.below(500).to_be_bytes());
            vec![
                (name("created"), Cell::attribute(Value::U64(i))),
                (
                    name("owner"),
                    Cell::attribute(Value::U64(rng.below(10_000))),
                ),
                (
                    name("delta"),
                    Cell::attribute(Value::I64(rng.below(2_000) as i64 - 1_000)),
                ),
                (
                    name("flag"),
                    Cell::attribute(Value::Bool(rng.below(2) == 0)),
                ),
                (name("amount"), Cell::attribute(Value::U256(word))),
                (name("account"), Cell::attribute(Value::Addr(addr))),
            ]
        }
        "mixed" => {
            let mut addr = [0u8; 20];
            addr[12..].copy_from_slice(&rng.below(500).to_be_bytes());
            vec![
                (name("color"), Cell::attribute(Value::str(word(rng)))),
                (
                    name("type"),
                    Cell::attribute(Value::str(&format!("type-{:02}", rng.below(20)))),
                ),
                (name("created"), Cell::attribute(Value::U64(i))),
                (
                    name("owner"),
                    Cell::attribute(Value::U64(rng.below(10_000))),
                ),
                (
                    name("flag"),
                    Cell::attribute(Value::Bool(rng.below(2) == 0)),
                ),
                (name("account"), Cell::attribute(Value::Addr(addr))),
            ]
        }
        other => panic!("unknown workload {other}"),
    };
    let payload: Vec<u8> = (0..8).flat_map(|_| rng.next().to_le_bytes()).collect();
    cells.push((name("payload"), Cell::field(Value::bytes(&payload))));
    cells
}

fn commit(engine: &mut DurableEngine, ops: Vec<WriteOp>) -> Res<()> {
    let mut branch = engine
        .begin(engine.head(), None)
        .map_err(|e| format!("begin: {e:?}"))?;
    for op in ops {
        branch
            .try_execute(OWNER, op, Budget::unlimited())
            .map_err(|e| format!("storage: {e:?}"))?
            .map_err(|e| format!("rejected: {e:?}"))?;
    }
    let sealed = branch.try_seal().map_err(|e| format!("seal: {e:?}"))?;
    engine
        .prepare(sealed)
        .map_err(|e| format!("prepare: {e:?}"))?;
    match engine.commit().map_err(|e| format!("commit: {e:?}"))? {
        Publication::Committed(_) => Ok(()),
        other => Err(format!("not committed: {other:?}").into()),
    }
}

fn main() -> Res<()> {
    let mut dir = std::env::temp_dir().join("arkiv-key-size");
    let mut workload = String::from("mixed");
    let mut records: u64 = 20_000;
    let mut block: u64 = 500;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or("missing value")?;
        match flag.as_str() {
            "--dir" => dir = PathBuf::from(value),
            "--workload" => workload = value,
            "--records" => records = value.parse()?,
            "--block" => block = value.parse()?,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("arkiv.redb");
    let store = Arc::new(
        RedbStore::create(&path, RedbConfig::with_cache_size(256 << 20))
            .map_err(|e| format!("redb: {e:?}"))?,
    );
    let versioned: Arc<dyn VersionedStore> = store.clone();
    let genesis = EngineGenesis::new([0x42; 32], PriceSchedule::FIXTURE);
    let mut engine = match DurableEngine::initialize(versioned, genesis, Options::default())
        .map_err(|e| format!("initialize: {e:?}"))?
    {
        Initialization::Committed(engine) => engine,
        _ => return Err("initialization not committed".into()),
    };
    let mut branch = engine
        .begin(engine.head(), None)
        .map_err(|e| format!("begin: {e:?}"))?;
    let created = branch
        .try_execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("sizes")?,
            },
            Budget::unlimited(),
        )
        .map_err(|e| format!("storage: {e:?}"))?
        .map_err(|e| format!("rejected: {e:?}"))?;
    let Effect::NamespaceCreated(ns) = created.effect else {
        return Err("no namespace".into());
    };
    let sealed = branch.try_seal().map_err(|e| format!("seal: {e:?}"))?;
    engine
        .prepare(sealed)
        .map_err(|e| format!("prepare: {e:?}"))?;
    engine.commit().map_err(|e| format!("commit: {e:?}"))?;

    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut sample_values: Vec<(CellName, Cell)> = Vec::new();
    let started = Instant::now();
    let mut i = 0;
    while i < records {
        let n = block.min(records - i);
        let ops = (i..i + n)
            .map(|j| {
                let cells = cells(&workload, j, &mut rng);
                if j % 997 == 0 {
                    sample_values.push(cells[0].clone());
                }
                WriteOp::CreateRecord {
                    namespace: ns,
                    key: RecordKey::new(format!("rec/{j:012}").as_bytes()).expect("key"),
                    cells,
                }
            })
            .collect();
        commit(&mut engine, ops)?;
        i += n;
    }
    let write_secs = started.elapsed().as_secs_f64();

    let report = engine
        .reachability(&[1])
        .map_err(|e| format!("reachability: {e:?}"))?;
    let reach = report.levels[0].total();
    let ns_reach = report.levels[0]
        .per_space
        .get(&arkiv_db_storage::versioned::SpaceId::Namespace(
            ns.ordinal(),
        ))
        .copied()
        .unwrap_or_default();

    let snapshot = engine.head_snapshot();
    let snapshot_ref = SnapshotRef {
        genesis_hash: B256::repeat_byte(1),
        height: snapshot.id().sequence(),
        block_hash: B256::repeat_byte(2),
        state_root: snapshot.state_root(),
    };
    // Equality proofs for sampled attribute values (first attribute).
    let mut eq_bytes = Vec::new();
    for (attribute, cell) in sample_values.iter().take(20) {
        let request = EqRequest {
            snapshot: snapshot_ref.clone(),
            query: EqQuery {
                namespace: ns,
                attribute: attribute.clone(),
                value: cell.value.clone(),
            },
            limit: 1,
            continuation: None,
        };
        let proof = snapshot
            .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
            .map_err(|e| format!("prove_eq: {e:?}"))?;
        eq_bytes.push(proof.bytes.len());
    }
    // Range proofs over the first attribute, from each sampled value up to
    // (excluding) the next larger sample, capped by the proof limits.
    let mut range_bytes = Vec::new();
    let mut range_terms = Vec::new();
    let mut samples: Vec<&Value> = sample_values.iter().map(|(_, c)| &c.value).collect();
    samples.sort_by(|a, b| a.cmp_ordered(b).unwrap());
    samples.dedup();
    let attribute = sample_values[0].0.clone();
    for value in samples.iter().take(10) {
        let request = RangeRequest {
            snapshot: snapshot_ref.clone(),
            query: RangeQuery {
                namespace: ns,
                attribute: attribute.clone(),
                lower: Bound::Included((*value).clone()),
                upper: Bound::Included((*value).clone()),
            },
        };
        if let Ok(proof) =
            snapshot.prove_range(&request, RangeProofLimits::default(), Budget::unlimited())
        {
            range_bytes.push(proof.bytes.len());
            range_terms.push(proof.supplied_nodes);
        }
    }
    drop(snapshot);
    drop(engine);
    drop(store);
    let file_bytes = std::fs::metadata(&path)?.len();
    let mean = |v: &[usize]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<usize>() as f64 / v.len() as f64
        }
    };
    println!(
        "RESULT workload={workload} records={records} block={block} write_secs={write_secs:.3} \
         file_bytes={file_bytes} reach_objects={} reach_bytes={} ns_objects={} ns_bytes={} \
         eq_proofs={} eq_proof_bytes_mean={:.1} range_proofs={} range_proof_bytes_mean={:.1} \
         range_supplied_nodes_mean={:.1}",
        reach.objects,
        reach.bytes,
        ns_reach.objects,
        ns_reach.bytes,
        eq_bytes.len(),
        mean(&eq_bytes),
        range_bytes.len(),
        mean(&range_bytes),
        mean(&range_terms),
    );
    Ok(())
}
