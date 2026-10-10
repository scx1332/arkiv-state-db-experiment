//! Durable-engine throughput on a real redb file: single-writer typed
//! creates and patches, then point reads from one and from several threads.
//! Run in release mode. Prints one Markdown table row per measurement.
//!
//! `ARKIV_BENCH_DIR=<dir on the target disk> PERF_CACHE_MB=8 cargo run --release --offline -p arkiv-db-redb --example durable_throughput`
//!
//! `PERF_CACHE_ENTRIES` defaults to 2,048 per cache MiB, the ratio of the
//! default options. `PERF_RECORDS` (default 50,000) records are created in commits of
//! `PERF_BATCH` (default 500) operations. Write rates count only operation
//! execution; the commit column adds preparation and the durable commit.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/engine_support/mod.rs"]
mod engine_support;

use arkiv_db::{
    Budget, Cell, Projection, Value, WriteOp,
    durable::{DurableEngine, Initialization, Options},
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use common::BenchDir;
use engine_support::*;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// SplitMix64: a fixed key sequence per seed.
struct Keys(u64);
impl Keys {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % bound
    }
}

fn create(branch: &mut arkiv_db::Branch, n: u64) {
    execute(
        branch,
        WriteOp::CreateRecord {
            namespace: ns(1),
            key: key(n),
            cells: vec![
                (name("kind"), Cell::attribute(Value::U64(n % 16))),
                (
                    name("owner"),
                    Cell::attribute(Value::str(&format!("owner-{}", n % 97))),
                ),
                (name("score"), Cell::attribute(Value::U64(n))),
                (name("payload"), Cell::field(Value::bytes(&[n as u8; 64]))),
            ],
        },
    );
}

fn patch(branch: &mut arkiv_db::Branch, n: u64, round: u64) {
    execute(
        branch,
        WriteOp::PatchRecord {
            namespace: ns(1),
            key: key(n),
            set: vec![(name("score"), Cell::attribute(Value::U64(n + round)))],
            remove: vec![],
        },
    );
}

fn rate(count: u64, elapsed: Duration) -> f64 {
    count as f64 / elapsed.as_secs_f64()
}

fn reads(engine: &DurableEngine, threads: u64, per_thread: u64, records: u64) -> f64 {
    let snapshot = engine.head_snapshot();
    let started = Instant::now();
    std::thread::scope(|scope| {
        for thread in 0..threads {
            let snapshot = snapshot.clone();
            scope.spawn(move || {
                let mut keys = Keys(1000 + thread);
                for _ in 0..per_thread {
                    let n = keys.next(records);
                    let row = snapshot
                        .try_get_record(ns(1), &key(n), &Projection::ALL, Budget::unlimited())
                        .unwrap()
                        .unwrap()
                        .value
                        .unwrap();
                    assert_eq!(row.key.unwrap(), key(n));
                }
            });
        }
    });
    rate(threads * per_thread, started.elapsed())
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("run this measurement with --release");
        std::process::exit(2);
    }
    let cache_mb = env("PERF_CACHE_MB", 8);
    let records = env("PERF_RECORDS", 50_000);
    let batch = env("PERF_BATCH", 500);
    let dir = BenchDir::new("durable-throughput");
    let path = dir.file("throughput.redb");
    let store = Arc::new(RedbStore::create(&path, RedbConfig::with_cache_size(64 << 20)).unwrap());
    // The default entry limit per byte (16,384 entries per 8 MiB) unless set.
    let entries = env("PERF_CACHE_ENTRIES", cache_mb * 2048);
    let options = Options {
        cache_bytes: (cache_mb as usize) << 20,
        cache_entries: entries as usize,
        ..Options::default()
    };
    let mut engine = match DurableEngine::initialize(store.clone(), genesis(), options).unwrap() {
        Initialization::Committed(engine) => engine,
        other => panic!("initialization: {other:?}"),
    };
    let mut branch = engine.begin(engine.head(), None).unwrap();
    namespace(&mut branch, "bench");
    publish(&mut engine, branch);

    let (mut execute_time, mut commit_time) = (Duration::ZERO, Duration::ZERO);
    let mut n = 0;
    while n < records {
        let mut branch = engine.begin(engine.head(), None).unwrap();
        let started = Instant::now();
        let end = (n + batch).min(records);
        for i in n..end {
            create(&mut branch, i);
        }
        execute_time += started.elapsed();
        let started = Instant::now();
        publish(&mut engine, branch);
        commit_time += started.elapsed();
        n = end;
    }
    let create_rate = rate(records, execute_time);
    let create_total = rate(records, execute_time + commit_time);

    let patches = records.min(20_000);
    let (mut execute_time, mut commit_time) = (Duration::ZERO, Duration::ZERO);
    let mut keys = Keys(7);
    let mut done = 0;
    let mut round = 1;
    while done < patches {
        let mut branch = engine.begin(engine.head(), None).unwrap();
        let started = Instant::now();
        let end = (done + batch).min(patches);
        for _ in done..end {
            patch(&mut branch, keys.next(records), round);
        }
        execute_time += started.elapsed();
        let started = Instant::now();
        publish(&mut engine, branch);
        commit_time += started.elapsed();
        done = end;
        round += 1;
    }
    let patch_rate = rate(patches, execute_time);
    let patch_total = rate(patches, execute_time + commit_time);

    let before = engine.stats().cache;
    let read_1 = reads(&engine, 1, 100_000, records);
    let read_4 = reads(&engine, 4, 100_000, records);
    let read_8 = reads(&engine, 8, 100_000, records);
    let after = engine.stats().cache;
    let read_hits = after.hits - before.hits;
    let read_misses = after.misses - before.misses;
    println!(
        "| {cache_mb} MiB / {entries} | {records} | {create_rate:.0} | {create_total:.0} | {patch_rate:.0} | {patch_total:.0} | {read_1:.0} | {read_4:.0} | {read_8:.0} | {:.1}% | {} |",
        100.0 * read_hits as f64 / (read_hits + read_misses).max(1) as f64,
        std::fs::metadata(&path).unwrap().len() >> 20,
    );
}
