//! Reader/producer contention benchmark for the durable engine on redb.
//!
//! One producer thread writes typed records in blocks of `--block-ops`
//! operations (3 creates : 1 patch) through `begin` / `try_seal` /
//! `prepare` / `commit`, while `--readers` threads do point reads
//! (`try_get_record`, all cells) of random committed keys on the latest
//! published head snapshot. A preload of `--preload` records is committed
//! first and is not measured.
//!
//! The last line is machine-readable (`scripts/bench-compare.sh` parses it):
//!
//! ```text
//! RESULT readers=4 block_ops=100 ops=50000 producer_ops_per_s=... reader_reads_per_s=... ...
//! ```
//!
//! Only the stable public `DurableEngine` / `Snapshot` / `Branch` API and
//! `durable::Options::default()` are used, so the example builds against
//! older and newer engine revisions alike.
//!
//! ```sh
//! cargo run --release -p arkiv-db-redb --example bench_readers -- \
//!     --dir /path/to/scratch --readers 4 --block-ops 100 --ops 50000
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use arkiv_db::durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication};
use arkiv_db::{
    Budget, Cell, CellName, Effect, NamespaceId, NamespaceName, OwnerId, PriceSchedule, Projection,
    RecordKey, Snapshot, Value, WriteOp,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const OWNER: OwnerId = OwnerId::new([0xb5; 20]);

struct Args {
    dir: PathBuf,
    readers: usize,
    block_ops: usize,
    ops: usize,
    preload: usize,
    seed: u64,
    redb_cache_mib: usize,
}

fn parse_args() -> Res<Args> {
    let mut args = Args {
        dir: std::env::temp_dir().join("arkiv-bench-readers"),
        readers: 0,
        block_ops: 100,
        ops: 50_000,
        preload: 20_000,
        seed: 7,
        redb_cache_mib: 256,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--dir" => args.dir = PathBuf::from(value()?),
            "--readers" => args.readers = value()?.parse()?,
            "--block-ops" => args.block_ops = value()?.parse()?,
            "--ops" => args.ops = value()?.parse()?,
            "--preload" => args.preload = value()?.parse()?,
            "--seed" => args.seed = value()?.parse()?,
            "--redb-cache-mib" => args.redb_cache_mib = value()?.parse()?,
            "-h" | "--help" => {
                println!(
                    "usage: bench_readers [--dir D] [--readers N] [--block-ops N] [--ops N] \
                     [--preload N] [--seed N] [--redb-cache-mib N]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if args.block_ops == 0 {
        return Err("--block-ops must be > 0".into());
    }
    Ok(args)
}

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

fn key(i: u64) -> RecordKey {
    RecordKey::new(format!("rec/{i:012}").as_bytes()).expect("valid key")
}

struct Names {
    kind: CellName,
    owner: CellName,
    created: CellName,
    payload: CellName,
}

fn create_op(ns: NamespaceId, names: &Names, i: u64, rng: &mut Rng) -> WriteOp {
    let mut payload = vec![0u8; 256];
    for chunk in payload.chunks_mut(8) {
        let word = rng.next().to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    WriteOp::CreateRecord {
        namespace: ns,
        key: key(i),
        cells: vec![
            (
                names.kind.clone(),
                Cell::attribute(Value::str(&format!("type-{:02}", rng.below(20)))),
            ),
            (
                names.owner.clone(),
                Cell::attribute(Value::U64(rng.below(10_000))),
            ),
            (names.created.clone(), Cell::attribute(Value::U64(i))),
            (names.payload.clone(), Cell::field(Value::bytes(&payload))),
        ],
    }
}

fn patch_op(ns: NamespaceId, names: &Names, i: u64, rng: &mut Rng) -> WriteOp {
    WriteOp::PatchRecord {
        namespace: ns,
        key: key(i),
        set: vec![(
            names.owner.clone(),
            Cell::attribute(Value::U64(rng.below(10_000))),
        )],
        remove: vec![],
    }
}

/// Executes `ops` in one block and commits it; returns the new head.
fn commit_block(engine: &mut DurableEngine, ops: Vec<WriteOp>) -> Res<Snapshot> {
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
        Publication::Committed(_) => Ok(engine.head_snapshot()),
        other => Err(format!("commit not committed: {other:?}").into()),
    }
}

fn percentile(sorted: &[u32], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx] as f64
}

fn main() -> Res<()> {
    let args = parse_args()?;
    let _ = std::fs::remove_dir_all(&args.dir);
    std::fs::create_dir_all(&args.dir)?;
    let path = args.dir.join("arkiv.redb");
    let store = Arc::new(
        RedbStore::create(
            &path,
            RedbConfig::with_cache_size(args.redb_cache_mib << 20),
        )
        .map_err(|e| format!("redb create: {e:?}"))?,
    );
    let versioned: Arc<dyn VersionedStore> = store.clone();
    let genesis = EngineGenesis::new([0x42; 32], PriceSchedule::FIXTURE);
    let mut engine = match DurableEngine::initialize(versioned, genesis, Options::default())
        .map_err(|e| format!("initialize: {e:?}"))?
    {
        Initialization::Committed(engine) => engine,
        _ => return Err("initialization not committed".into()),
    };

    // Namespace.
    let mut branch = engine
        .begin(engine.head(), None)
        .map_err(|e| format!("begin: {e:?}"))?;
    let created = branch
        .try_execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("bench")?,
            },
            Budget::unlimited(),
        )
        .map_err(|e| format!("storage: {e:?}"))?
        .map_err(|e| format!("rejected: {e:?}"))?;
    let Effect::NamespaceCreated(ns) = created.effect else {
        return Err("namespace not created".into());
    };
    let sealed = branch.try_seal().map_err(|e| format!("seal: {e:?}"))?;
    engine
        .prepare(sealed)
        .map_err(|e| format!("prepare: {e:?}"))?;
    engine.commit().map_err(|e| format!("commit: {e:?}"))?;

    let names = Names {
        kind: CellName::new("type")?,
        owner: CellName::new("owner")?,
        created: CellName::new("created")?,
        payload: CellName::new("payload")?,
    };
    let mut rng = Rng::new(args.seed);

    // Preload (unmeasured), 1000 creates per block.
    let preload_start = Instant::now();
    let mut next_key: u64 = 0;
    while (next_key as usize) < args.preload {
        let n = (args.preload - next_key as usize).min(1000);
        let ops = (0..n)
            .map(|j| create_op(ns, &names, next_key + j as u64, &mut rng))
            .collect();
        commit_block(&mut engine, ops)?;
        next_key += n as u64;
    }
    let preload_secs = preload_start.elapsed().as_secs_f64();

    // Readers.
    // The head and the number of keys it holds, swapped together.
    let head = Arc::new(RwLock::new((engine.head_snapshot(), next_key)));
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for r in 0..args.readers {
        let head = head.clone();
        let stop = stop.clone();
        let seed = args.seed ^ ((r as u64 + 1) << 32);
        handles.push(std::thread::spawn(move || -> Res<(u64, u64, Vec<u32>)> {
            let mut rng = Rng::new(seed);
            let mut reads = 0u64;
            let mut found = 0u64;
            let mut samples = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let (snapshot, limit) = head.read().map_err(|_| "poisoned")?.clone();
                // A batch of reads per snapshot fetch, like a request handler.
                for _ in 0..64 {
                    let k = key(rng.below(limit));
                    let t = Instant::now();
                    let row = snapshot
                        .try_get_record(ns, &k, &Projection::ALL, Budget::unlimited())
                        .map_err(|e| format!("read storage: {e:?}"))?
                        .map_err(|e| format!("read rejected: {e:?}"))?;
                    let micros = t.elapsed().as_micros().min(u32::MAX as u128) as u32;
                    if reads.is_multiple_of(16) {
                        samples.push(micros.max(1));
                    }
                    reads += 1;
                    if row.value.is_some() {
                        found += 1;
                    }
                }
            }
            Ok((reads, found, samples))
        }));
    }

    // Producer (measured).
    let mut commit_ms: Vec<u32> = Vec::new();
    let start = Instant::now();
    let mut done = 0usize;
    let mut created_in_run = 0u64;
    while done < args.ops {
        let n = (args.ops - done).min(args.block_ops);
        let mut ops = Vec::with_capacity(n);
        let mut new_keys = 0u64;
        for j in 0..n {
            if j % 4 == 3 {
                let existing = rng.below(next_key);
                ops.push(patch_op(ns, &names, existing, &mut rng));
            } else {
                ops.push(create_op(ns, &names, next_key + new_keys, &mut rng));
                new_keys += 1;
            }
        }
        let t = Instant::now();
        let snapshot = commit_block(&mut engine, ops)?;
        commit_ms.push((t.elapsed().as_micros().min(u32::MAX as u128)) as u32);
        next_key += new_keys;
        created_in_run += new_keys;
        *head.write().map_err(|_| "poisoned")? = (snapshot, next_key);
        done += n;
    }
    let wall = start.elapsed();
    // Let readers observe the final head briefly so 0-op runs still sample.
    if args.ops == 0 {
        std::thread::sleep(Duration::from_secs(2));
    }
    stop.store(true, Ordering::Relaxed);
    let reader_wall = start.elapsed();
    let mut reads = 0u64;
    let mut found = 0u64;
    let mut samples = Vec::new();
    for h in handles {
        let (r, f, s) = h.join().map_err(|_| "reader panicked")??;
        reads += r;
        found += f;
        samples.extend(s);
    }
    samples.sort_unstable();
    commit_ms.sort_unstable();
    let secs = wall.as_secs_f64().max(1e-9);
    let disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "preload {} records in {preload_secs:.1}s; {} creates + {} patches in {} blocks",
        args.preload,
        created_in_run,
        args.ops as u64 - created_in_run,
        commit_ms.len()
    );
    println!(
        "RESULT readers={} block_ops={} ops={} secs={:.3} producer_ops_per_s={:.1} \
         block_p50_ms={:.3} block_p99_ms={:.3} reader_reads={} reader_reads_per_s={:.1} \
         read_found={} read_p50_us={:.1} read_p99_us={:.1} disk_bytes={}",
        args.readers,
        args.block_ops,
        args.ops,
        secs,
        args.ops as f64 / secs,
        percentile(&commit_ms, 0.5) / 1000.0,
        percentile(&commit_ms, 0.99) / 1000.0,
        reads,
        reads as f64 / reader_wall.as_secs_f64().max(1e-9),
        found,
        percentile(&samples, 0.5),
        percentile(&samples, 0.99),
        disk
    );
    drop(engine);
    drop(store);
    let _ = std::fs::remove_dir_all(&args.dir);
    Ok(())
}
