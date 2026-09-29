//! The redb storage experiment: measures every gate of
//! `docs/redb-experiment.md` and prints a Markdown report.
//!
//! ```sh
//! cargo run --release -p arkiv-db-redb --example redb_experiment
//! cargo run --release -p arkiv-db-redb --example redb_experiment -- --quick
//! ```
//!
//! Measurement files go to `ARKIV_BENCH_DIR`, or `<workspace>/target/bench`
//! (a real disk; `std::env::temp_dir()` may be a RAM file system where
//! `fsync` does nothing). They are deleted afterwards. Times are wall-clock
//! with the operating system's page cache warm (dropping it needs root);
//! backend byte counts do not depend on it. Debug builds of redb walk the
//! whole file on open (a `debug_assertions` check), so open measurements are
//! only meaningful in release builds; the report states the profile.

#[path = "../tests/common/mod.rs"]
mod common;

use std::{
    io,
    process::Command,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp, SplitMix},
};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, ObjectReader, Reconciliation, SpaceId, VersionedStore, reconcile,
};
use common::{
    BenchDir, amplification, assert_applied, base_dir, block, commit, crash, crash_store, creation,
    disk_full::{self, Space},
    filesystem_of, isolation, lost_sync, mb, recovery, timed,
};

struct Scale {
    recovery_sizes: Vec<u64>,
    isolation: isolation::Params,
    amplification_objects: u64,
    amplification_blocks: Vec<u64>,
    history_blocks: u64,
    growing_all_points: bool,
}

fn scale(quick: bool) -> Scale {
    if quick {
        Scale {
            recovery_sizes: vec![4 << 20, 16 << 20, 64 << 20],
            isolation: isolation::Params {
                a_objects: 500,
                object_len: 256,
                b_factors: vec![1, 10, 100],
                namespace_counts: vec![1_000],
                history_blocks: 200,
                objects_per_block: 2_000,
                small_cache: 256 << 10,
            },
            amplification_objects: 4_096,
            amplification_blocks: vec![1, 16, 256, 4_096],
            history_blocks: 100,
            growing_all_points: false,
        }
    } else {
        Scale {
            recovery_sizes: vec![16 << 20, 64 << 20, 256 << 20, 1 << 30],
            isolation: isolation::Params {
                a_objects: 1_000,
                object_len: 256,
                b_factors: vec![1, 10, 100],
                namespace_counts: vec![1_000, 10_000],
                history_blocks: 1_000,
                objects_per_block: 4_000,
                small_cache: 256 << 10,
            },
            amplification_objects: 16_384,
            amplification_blocks: vec![1, 16, 256, 4_096, 16_384],
            history_blocks: 400,
            growing_all_points: true,
        }
    }
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn machine() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown CPU".to_owned());
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("MemTotal"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|kib| kib.parse::<u64>().ok())
        })
        .map_or("unknown".to_owned(), |kib| format!("{} GiB", kib >> 20));
    let threads = std::thread::available_parallelism().map_or(0, |count| count.get());
    format!(
        "{cpu}, {threads} hardware threads, {memory} RAM, kernel {}",
        command_output("uname", &["-r"])
    )
}

fn ms(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1e3)
}

fn main() {
    let quick = std::env::args().any(|arg| arg == "--quick");
    let scale = scale(quick);
    let base = base_dir();
    std::fs::create_dir_all(&base).expect("bench directory");
    let base = std::fs::canonicalize(&base).unwrap_or(base);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    println!("# redb experiment report\n");
    println!(
        "- date: {}",
        command_output("date", &["-u", "+%Y-%m-%d %H:%M UTC"])
    );
    println!(
        "- command: `cargo run --release -p arkiv-db-redb --example redb_experiment{}`",
        if quick { " -- --quick" } else { "" }
    );
    println!(
        "- profile: {profile}; {}",
        command_output("rustc", &["--version"])
    );
    println!(
        "- redb 4.3.0, quick repair on unless stated, redb cache {} KiB by default",
        arkiv_db_redb::DEFAULT_CACHE_SIZE >> 10
    );
    println!("- machine: {}", machine());
    println!(
        "- measurement directory: {} ({})",
        base.display(),
        filesystem_of(&base)
    );
    if profile == "debug" {
        println!("\n**Warning: debug build. Open I/O and timings are not meaningful.**");
    }

    atomicity(&scale);
    durability();
    recovery_gate(&scale);
    history(&scale);
    reconciliation();
    latching();
    disk_full();
    isolation_gate(&scale);
    amplification_gate(&scale);
}

fn atomicity(scale: &Scale) {
    println!("\n## Gate 1: atomicity at every crash point\n");
    println!(
        "| run | crash points | calls/commit | syncs/commit | set_len/commit | images | parent | candidate | full repairs | outcomes C/I/N |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |");
    let row = |name: &str, tally: &crash::Tally| {
        println!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{}/{} |",
            tally.crash_points,
            tally.calls_per_commit,
            tally.syncs_per_commit,
            tally.set_lens_per_commit,
            tally.images,
            tally.parent,
            tally.candidate,
            tally.full_repairs,
            tally.committed_outcomes,
            tally.indeterminate_outcomes,
            tally.not_committed_outcomes,
        );
    };
    let (tally, time) = timed(|| crash::run(true, crash::HEAVY_SEEDS));
    row(
        "ten tables after a restart, quick repair, 2 + 128 torn modes",
        &tally,
    );
    let (tally_off, _) = timed(|| crash::run(false, crash::SEEDS));
    row(
        "ten tables after a restart, no quick repair, 2 + 16 torn modes",
        &tally_off,
    );
    let steady = crash::run_steady(true, crash::HEAVY_SEEDS);
    row(
        "ten tables in steady state, quick repair, 2 + 128 torn modes",
        &steady,
    );
    let select = |k: u64, trace: &[DiskOp]| {
        let calls = trace.len() as u64;
        k < 16
            || k + 32 >= calls
            || k.is_multiple_of(97)
            || (k.saturating_sub(1)..=k + 1)
                .any(|i| i < calls && trace[i as usize] != DiskOp::Write)
    };
    let points = if scale.growing_all_points {
        "every crash point"
    } else {
        "sampled crash points"
    };
    let select = (!scale.growing_all_points).then_some(&select as crash::Select<'_>);
    row(
        &format!("spilling, growing commit after a restart ({points}), 2 + 1 torn modes"),
        &crash::run_growing(select, 1),
    );
    row(
        &format!("spilling, growing commit in steady state ({points}), 2 + 1 torn modes"),
        &crash::run_growing_steady(select, 1),
    );
    println!(
        "\nC/I/N: Committed / Indeterminate / NotCommitted outcomes of the cut-off commit. Every image also passed redb's integrity check, before and after one more block committed on top. First run: {} s.",
        time.as_secs()
    );
}

fn durability() {
    println!("\n## Immediate durability\n");
    let disk = CrashDisk::new();
    let store = crash_store(&disk, RedbConfig::default());
    let mut parent = None;
    let mut survived = 0;
    for height in 0..50 {
        let batch = block(parent, height, 1, &[(SpaceId::Namespace(1), 10, 200)]);
        commit(&store, batch.clone());
        parent = Some(batch.new_head);
        let reopened = crash_store(
            &CrashDisk::from_image(disk.crash_image(CrashMode::DurableOnly)),
            RedbConfig::default(),
        );
        if reopened.head().unwrap() == parent {
            survived += 1;
        }
    }
    let syncs = store.stats().backend.unwrap().syncs;
    println!(
        "- 50 commits on a CrashDisk: {survived}/50 survive a power cut taken right after `Committed` (synced data only)."
    );
    println!(
        "- backend syncs during store creation and 50 commits: {syncs} (two per commit with quick repair)."
    );

    let created = creation::creation_images();
    let in_place = creation::reopen_in_place(&created.images);
    let dir = BenchDir::new("experiment-creation");
    let discarded = creation::create_over_leftovers(dir.path(), &created.images);
    println!(
        "- store creation cut off at each of its {} mutating calls: {} distinct crash images. Reopened in place: {} initialized, {} opened, {} refused as `Incompatible`. Left as `create`'s temporary file: {discarded} of {} discarded and a fresh store created.",
        created.calls,
        created.images.len(),
        in_place.initialized,
        in_place.opened,
        in_place.refused,
        created.images.len(),
    );
}

fn recovery_gate(scale: &Scale) {
    println!("\n## Gate 3: recovery and per-commit cost against database size\n");
    println!(
        "Blocks of 256 objects of 1 KiB over 8 namespaces until the size is reached; then 16 small commits (16 objects of 256 B plus records) measured; then the handle is killed (no clean close) and the file reopened with a 1 MiB cache.\n"
    );
    println!(
        "| file MB | blocks | quick repair | full repair at open | open ms | open backend reads | open read MB | open read / file | app reads at open | app backend reads (KB) | commit write KB | commit reads | syncs/commit | commit p50 ms |"
    );
    println!(
        "| ---: | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |"
    );
    let dir = BenchDir::new("experiment-recovery");
    for size in &scale.recovery_sizes {
        for quick_repair in [true, false] {
            let row = recovery::measure(&dir, *size, quick_repair, 1024, 256, 16);
            println!(
                "| {:.1} | {} | {} | {} | {} | {} | {:.3} | {:.2} | {} | {} ({:.1}) | {:.1} | {} | {} | {} |",
                mb(row.file_bytes),
                row.blocks,
                if quick_repair { "on" } else { "off" },
                row.open.full_repair,
                ms(row.open.duration),
                row.open.backend.reads,
                mb(row.open.backend.read_bytes),
                row.open.backend.read_bytes as f64 / row.file_bytes as f64,
                row.app_reads_at_open,
                row.app_backend_at_open.reads,
                row.app_backend_at_open.read_bytes as f64 / 1e3,
                row.commit_io.write_bytes as f64 / 1e3,
                row.commit_io.reads,
                row.commit_io.syncs,
                ms(row.commit_p50),
            );
            let _ = std::fs::remove_file(dir.file(&format!("recovery-{size}-{quick_repair}.redb")));
        }
    }
}

fn history(scale: &Scale) {
    println!("\n## Historical objects after reopen and during commits\n");
    let dir = BenchDir::new("experiment-history");
    let store = Arc::new(
        RedbStore::create(
            dir.file("history.redb"),
            RedbConfig::with_cache_size(256 << 10),
        )
        .unwrap(),
    );
    let published: Arc<RwLock<Vec<CommitBatch>>> = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let verified = Arc::new(AtomicU64::new(0));
    let first = block(None, 0, 1, &[(SpaceId::Namespace(1), 50, 200)]);
    commit(store.as_ref(), first.clone());
    published.write().unwrap().push(first);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for reader in 0..4 {
            let (store, published, done, verified) = (
                Arc::clone(&store),
                Arc::clone(&published),
                Arc::clone(&done),
                Arc::clone(&verified),
            );
            scope.spawn(move || {
                let mut rng = SplitMix::new(reader);
                while !done.load(Ordering::Acquire) {
                    let batch = {
                        let list = published.read().unwrap();
                        list[rng.below(list.len() as u64) as usize].clone()
                    };
                    assert_applied(store.as_ref(), &batch);
                    verified.fetch_add(batch.objects.len() as u64, Ordering::Relaxed);
                }
            });
        }
        let mut parent = Some(published.read().unwrap()[0].new_head);
        for height in 1..scale.history_blocks {
            let batch = block(
                parent,
                height,
                1,
                &[
                    (SpaceId::Namespace(1), 50, 200),
                    (SpaceId::Namespace(2 + height % 7), 10, 500),
                ],
            );
            commit(store.as_ref(), batch.clone());
            parent = Some(batch.new_head);
            published.write().unwrap().push(batch);
        }
        done.store(true, Ordering::Release);
    });
    let elapsed = started.elapsed();
    let blocks = published.read().unwrap().clone();
    drop(store);
    let reopened =
        RedbStore::open(dir.file("history.redb"), RedbConfig::with_cache_size(0)).unwrap();
    for batch in &blocks {
        assert_applied(&reopened, batch);
    }
    println!(
        "- {} blocks committed in {:.2} s while 4 readers verified {} historical objects ({:.0} objects/s); every committed object matched.",
        blocks.len(),
        elapsed.as_secs_f64(),
        verified.load(Ordering::Relaxed),
        verified.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64()
    );
    println!(
        "- after a reopen with a zero-byte redb cache, all {} objects of all blocks read back identical.",
        blocks
            .iter()
            .map(|batch| batch.objects.len())
            .sum::<usize>()
    );
}

fn reconciliation() {
    println!("\n## Lost-acknowledgement reconciliation\n");
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 30, 200)]);
    let candidate = block(
        Some(genesis.new_head),
        1,
        1,
        &[
            (SpaceId::Namespace(1), 40, 200),
            (SpaceId::Namespace(2), 20, 700),
        ],
    );
    let image = {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, RedbConfig::default());
        commit(&store, genesis);
        disk.crash(store, CrashMode::DurableOnly)
    };
    let trace = {
        let disk = CrashDisk::from_image(image.clone());
        let store = crash_store(&disk, RedbConfig::default());
        let start = disk.mutating_trace().len();
        commit(&store, candidate.clone());
        disk.mutating_trace()[start..].to_vec()
    };
    let (mut absent, mut committed, mut once) = (0, 0, 0);
    for (k, op) in trace.iter().enumerate() {
        let skip = trace[..k].iter().filter(|earlier| *earlier == op).count() as u64;
        let disk = CrashDisk::from_image(image.clone());
        let store = crash_store(&disk, RedbConfig::default());
        disk.fail(*op, skip, io::ErrorKind::Other);
        let outcome = store.commit(candidate.clone());
        assert!(!matches!(outcome, CommitOutcome::Committed));
        match reconcile(&store, &candidate).unwrap() {
            Reconciliation::Committed => committed += 1,
            Reconciliation::NotCommitted => {
                absent += 1;
                commit(&store, candidate.clone());
                if matches!(
                    store.commit(candidate.clone()),
                    CommitOutcome::NotCommitted(_)
                ) {
                    once += 1;
                }
            }
            Reconciliation::Diverged { head } => panic!("diverged at {head:?}"),
        }
    }
    println!(
        "- I/O error injected at each of the {} mutating calls of a commit: {absent} reconciled absent (retried: committed once, second retry refused in {once}), {committed} reconciled committed (the final sync failed after the commit record was written).",
        trace.len()
    );
    let (_, candidate) = lost_sync::chain();
    let two_phase = lost_sync::lost_final_sync(true);
    let one_phase = lost_sync::lost_final_sync(false);
    println!(
        "- final sync failing and losing its writes (reads still return them), reconciled as committed, then a power cut: with quick repair (two-phase commit) the reopen finds {}; without it, {}.",
        match &two_phase {
            Ok(head) if *head == Some(candidate.new_head) => "the block".to_owned(),
            other => format!("{other:?}"),
        },
        match &one_phase {
            Err(error) => format!("the reopen fails: {error}"),
            Ok(head) => format!("head {head:?}"),
        }
    );
}

fn latching() {
    println!("\n## Failed-handle latching and recover()\n");
    let disk = CrashDisk::new();
    let store = crash_store(&disk, RedbConfig::with_cache_size(0));
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 10, 200)]);
    commit(&store, genesis.clone());
    disk.fail(DiskOp::Read, 0, io::ErrorKind::Other);
    let error = store.head().unwrap_err();
    let refused = store
        .read_object(SpaceId::Namespace(1), &genesis.objects[0].1)
        .unwrap_err()
        == error;
    let (head, recover_time) = timed(|| store.recover());
    println!(
        "- after one injected read error every call returned the latched error: {refused}; `recover()` reopened in {} ms and returned the head: {}.",
        ms(recover_time),
        head.unwrap() == Some(genesis.new_head)
    );
}

fn disk_full() {
    println!("\n## Disk full\n");
    for space in [Space::Length, Space::Allocation] {
        let result = disk_full::sweep(space);
        println!(
            "- {} quota, {} quotas between {} and {} bytes: {} `NotCommitted(Full)`, {} `Indeterminate(Full)`, {} committed; every failure latched `Full`, reconciled absent while full, failed again on retry, left the parent after a power cut, and committed exactly once after the quota was lifted.",
            match space {
                Space::Length => "length",
                Space::Allocation => "allocation",
            },
            result.quotas,
            result.base,
            result.needed,
            result.not_committed,
            result.indeterminate,
            result.committed,
        );
    }
}

fn isolation_gate(scale: &Scale) {
    println!("\n## Gate 2: namespace isolation\n");
    let params = &scale.isolation;
    println!(
        "A: {} objects of {} B, frozen. Each phase: reopen with a 0-byte cache and read all of A (uncached), then reopen with a {} KiB cache and read A twice (cold, warm).\n",
        params.a_objects,
        params.object_len,
        params.small_cache >> 10
    );
    println!(
        "| phase | B objects | namespaces | blocks | file MB | tables | directory height | A height | A app reads (hits, bytes) | other spaces read | uncached page reads | bound | cold page reads | warm page reads |"
    );
    println!(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |"
    );
    let dir = BenchDir::new("experiment-isolation");
    for phase in isolation::run(&dir, params) {
        let a = phase
            .app
            .iter()
            .find(|(space, _)| *space == isolation::A)
            .map(|(_, stats)| *stats)
            .unwrap_or_default();
        let others = phase
            .app
            .iter()
            .filter(|(space, _)| *space != isolation::A)
            .count();
        println!(
            "| {} | {} | {} | {} | {:.1} | {} | {} | {} | {} ({}, {}) | {} | {} | {} | {} | {} |",
            phase.name,
            phase.b_objects,
            phase.namespaces,
            phase.blocks,
            mb(phase.file_bytes),
            phase.tables,
            phase.directory_height,
            phase.a_height,
            a.reads,
            a.hits,
            a.read_bytes,
            others,
            phase.uncached.reads,
            phase.uncached_bound(),
            phase.cold.reads,
            phase.warm.reads,
        );
    }
}

fn amplification_gate(scale: &Scale) {
    println!("\n## Write amplification: few large vs many small blocks\n");
    println!(
        "{} objects of 256 B over 4 namespaces; every block also carries block, manifest, index and inclusion records and the head.\n",
        scale.amplification_objects
    );
    println!(
        "| objects/block | quick repair | blocks | logical MB | written MB | written/logical | writes | syncs | file MB | total s | p50 ms | p99 ms |"
    );
    println!(
        "| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    let dir = BenchDir::new("experiment-amplification");
    let mut runs: Vec<(u64, bool)> = scale
        .amplification_blocks
        .iter()
        .map(|size| (*size, true))
        .collect();
    runs.push((0, true));
    runs.push((16, false));
    runs.push((*scale.amplification_blocks.last().unwrap(), false));
    for (objects_per_block, quick_repair) in runs {
        let series = amplification::measure(
            &dir,
            if objects_per_block == 0 {
                0
            } else {
                scale.amplification_objects
            },
            256,
            objects_per_block,
            1_024,
            quick_repair,
        );
        println!(
            "| {} | {} | {} | {:.3} | {:.2} | {:.1} | {} | {} | {:.2} | {:.2} | {} | {} |",
            if objects_per_block == 0 {
                "0 (records only)".to_owned()
            } else {
                objects_per_block.to_string()
            },
            if quick_repair { "on" } else { "off" },
            series.blocks,
            mb(series.logical_bytes),
            mb(series.backend.write_bytes),
            series.amplification(),
            series.backend.writes,
            series.backend.syncs,
            mb(series.file_bytes),
            series.elapsed.as_secs_f64(),
            ms(series.p50),
            ms(series.p99),
        );
    }
}
