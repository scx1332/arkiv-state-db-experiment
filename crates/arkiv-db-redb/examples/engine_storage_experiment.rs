//! Engine-storage measurements on real redb files, including 10,000 durable
//! publications. Run in release mode; stdout is a standalone Markdown report.
//!
//! `cargo run --release --offline -p arkiv-db-redb --example engine_storage_experiment`

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/engine_support/mod.rs"]
mod engine_support;

use arkiv_db::{
    Budget, Cell, OperationClass, Receipt, Snapshot, SnapshotId, Value, WriteOp,
    durable::DurableEngine,
    proof::{ProofLimits, VerifiedEq, verify_eq},
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{Head, SpaceId, Table, VersionedStore};
use common::{BenchDir, filesystem_of};
use engine_support::*;
use std::{path::Path, process::Command, sync::Arc, time::Instant};

fn redb_config() -> RedbConfig {
    RedbConfig::with_cache_size(8 << 20)
}
fn command(program: &str, args: &[&str]) -> String {
    String::from_utf8_lossy(&Command::new(program).args(args).output().unwrap().stdout)
        .trim()
        .to_owned()
}
fn process_memory(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|s| s.starts_with(field))
        .and_then(|s| s.split_whitespace().nth(1))
        .unwrap()
        .parse()
        .unwrap()
}
fn rss() -> u64 {
    process_memory("VmRSS:")
}
fn high_water_rss() -> u64 {
    process_memory("VmHWM:")
}
fn open(path: &Path, cache: usize) -> (Arc<RedbStore>, DurableEngine) {
    let store = Arc::new(RedbStore::open(path, redb_config()).unwrap());
    let engine = DurableEngine::open(store.clone(), genesis(), options(cache)).unwrap();
    (store, engine)
}
fn assert_resident(engine: &DurableEngine, cache: usize) {
    let stats = engine.stats();
    assert_eq!(stats.resident_manifests, 1);
    assert_eq!(stats.pending_payload_bytes, 0);
    assert!(stats.cache.retained_bytes <= cache);
    assert!(stats.cache.entries <= options(cache).cache_entries);
    assert!(engine.head_snapshot().staged_objects().is_empty());
}

fn history() {
    let dir = BenchDir::new("engine-history");
    let path = dir.file("history.redb");
    let mut store = Arc::new(RedbStore::create(&path, redb_config()).unwrap());
    let mut engine = initialize(store.clone(), 64 << 10);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    base(&mut branch);
    publish(&mut engine, branch);
    let mut peak = engine.stats().last_preparation;
    let mut checkpoint_rows = Vec::new();
    let mut layout_rows = Vec::new();
    let mut open_rows = Vec::new();
    let started = Instant::now();
    for milestone in [100, 1000, 10000] {
        while engine.head().sequence() < milestone {
            let n = engine.head().sequence() + 1;
            let mut branch = engine.begin(engine.head(), None).unwrap();
            put(
                &mut branch,
                ns(1),
                &(n % 64).to_be_bytes(),
                &n.to_be_bytes(),
            );
            branch
                .try_host_put(HOST_KEY, &n.to_be_bytes())
                .unwrap()
                .unwrap();
            publish(&mut engine, branch);
            assert_resident(&engine, 64 << 10);
            let now = engine.stats().last_preparation;
            peak.payload_bytes = peak.payload_bytes.max(now.payload_bytes);
            peak.batch_container_bytes = peak.batch_container_bytes.max(now.batch_container_bytes);
            peak.dedup_entry_bytes = peak.dedup_entry_bytes.max(now.dedup_entry_bytes);
        }
        let s = engine.stats();
        checkpoint_rows.push(format!(
            "| {milestone} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.3} |",
            s.cache.retained_bytes,
            s.cache.container_bytes,
            s.cache.entries,
            s.cache.hits,
            s.cache.misses,
            s.resident_manifests,
            s.last_preparation.unique_objects,
            s.last_preparation.payload_bytes,
            rss(),
            high_water_rss(),
            std::fs::metadata(&path).unwrap().len(),
            started.elapsed().as_secs_f64()
        ));
        // Diagnostic traversal is outside the checkpoint/open counters. The
        // following close/reopen discards any backend cache warmth it creates.
        let layout = store
            .layout(&[
                SpaceId::Catalog,
                SpaceId::Namespace(1),
                SpaceId::Namespace(2),
                SpaceId::Host,
            ])
            .unwrap();
        layout_rows.push(format!(
            "| {milestone} | {} | {} | {} | {} | {} | {} | {} |",
            layout.tables,
            layout.directory_height,
            layout.page_size,
            layout.allocated_pages,
            layout.allocated_pages * layout.page_size as u64,
            layout.spaces.values().map(|s| s.entries).sum::<u64>(),
            layout.spaces.values().map(|s| s.stored_bytes).sum::<u64>()
        ));
        for mode in ["clean", "unclean"] {
            if mode == "unclean" {
                store.probe().kill();
            }
            drop(engine);
            drop(store);
            store = Arc::new(RedbStore::open(&path, redb_config()).unwrap());
            let report = store.last_open();
            let before = store.stats();
            let app = Instant::now();
            engine = DurableEngine::open(store.clone(), genesis(), options(64 << 10)).unwrap();
            let app_us = app.elapsed().as_micros();
            let delta = store.stats().since(&before);
            assert_eq!(delta.object_reads(), 0);
            assert_eq!(delta.record_reads, 8); // includes the head's state-floor record
            assert_eq!(delta.head_reads, 1);
            assert!(!report.full_repair);
            assert_eq!(engine.head().sequence(), milestone);
            open_rows.push(format!(
                "| {milestone} | {mode} | {} | {} | {} | {app_us} | {} | {} | {} | {} |",
                delta.record_reads,
                delta.head_reads,
                delta.object_reads(),
                report.duration.as_micros(),
                report.backend.reads,
                report.backend.read_bytes,
                report.full_repair
            ));
        }
    }
    println!(
        "\n## Retained history and resident state\n\nEach commit changes one of 64 raw keys and one host key; 16 typed rows remain fixed. Both caches start cold after each checkpoint reopen. Candidate/branch owners are dropped each commit.\n"
    );
    println!(
        "| sequence | node cache retained bytes | cache container bytes | cache entries | cache hits | cache misses | resident manifests | last staged objects | last payload bytes | RSS KiB | process HWM KiB | file bytes | elapsed s |\n| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in checkpoint_rows {
        println!("{row}");
    }
    println!(
        "\nPeak prepared payload: {} bytes; batch-vector capacity: {} bytes; dedup entry payload: {} bytes. The dedup entry figure excludes bounded B-tree allocator/node overhead. Preparation retains one batch, never a batch per historical snapshot, and the store commits it by reference.",
        peak.payload_bytes, peak.batch_container_bytes, peak.dedup_entry_bytes
    );
    println!(
        "\nAt all three quiescent checkpoints, the only retained decoded trie nodes are the decoded-node cache's, bounded by its configured bytes (the retained-bytes column is the sum of their estimated heap ownership) and entries: the head contains stored roots, no caller branch/candidate survives, and the runner asserts the head stages zero objects after every publication. Direct weak-reference tests separately check dirty graph/value release and that evicted or cleared cache nodes are released. Active calls may hold further decoded nodes; their peak is not measured here.\n\nThe following redb layout diagnostics measure disk allocation, including retained history. They run after each checkpoint's counters/RSS and before the cold reopen. They do not attribute resident metadata heap bytes.\n"
    );
    println!(
        "| sequence | tables | directory height | page bytes | allocated pages | allocated disk bytes | historical object entries | object key/value bytes |\n| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in layout_rows {
        println!("{row}");
    }
    println!(
        "\n## Cold application open\n\nThe OS page cache is warm; application and redb caches are new. `unclean` disables the old handle's backend before drop, preventing clean-close writes; actual child-process SIGKILL cases are tested separately. Backend open figures include redb allocator/repair metadata and are separate from the application's point reads.\n"
    );
    println!(
        "| sequence | close | app records | app head reads | app trie objects | app μs | redb open μs | redb reads | redb read bytes | full repair |\n| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in open_rows {
        println!("{row}");
    }
    drop(engine);
    drop(store);
    historical_proofs(&path);
}

fn historical_proofs(path: &Path) {
    let (memory, _) = memory_snapshots(false);
    let expected = proof(&memory, ns(1));
    let mut work_rows = Vec::new();
    let mut prior_work = None;
    println!(
        "\n## Historical proofs under bounded caches\n\nSnapshot 1 reopens after 10,000 publications. Proof bytes equal the memory engine and verify independently. Cold and warm refer to the engine's decoded-node cache.\n"
    );
    println!(
        "| cache bytes | cold object reads | warm object reads | cold backend bytes | warm backend bytes | proof bytes | retained bytes | entries |\n| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for cache in [0, 64 << 10, 8 << 20] {
        let (store, engine) = open(path, cache);
        let before = store.stats();
        let snapshot = engine.snapshot(SnapshotId::from_sequence(1)).unwrap();
        let opened = store.stats().since(&before);
        assert_eq!(opened.object_reads(), 0);
        assert_eq!(opened.record_reads, 3);
        let request = proof_request(&snapshot, ns(1));
        let mut physical = Vec::new();
        for mode in ["cold", "warm"] {
            let before = store.stats();
            let cache_before = engine.stats().cache;
            let started = Instant::now();
            let generated = snapshot
                .prove_eq(
                    &request,
                    ProofLimits::default(),
                    Budget::unlimited().traced(),
                )
                .unwrap();
            let generate_us = started.elapsed().as_micros();
            let delta = store.stats().since(&before);
            let cache_after = engine.stats().cache;
            let started = Instant::now();
            let verified = verify_eq(
                &request,
                None,
                &generated.bytes,
                ProofLimits::default(),
                genesis().limits,
            )
            .unwrap();
            let verify_us = started.elapsed().as_micros();
            assert!(matches!(verified, VerifiedEq::Page(_)));
            assert_eq!(generated.bytes, expected);
            let path_visits: u64 = generated
                .receipt
                .ledger
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .filter(|entry| entry.class == OperationClass::ProofNode)
                .map(|entry| entry.count)
                .sum();
            let work = (
                path_visits,
                generated.supplied_nodes,
                generated.posting_count,
                generated.receipt.clone(),
            );
            if let Some(prior) = &prior_work {
                assert_eq!(&work, prior, "cache warmth must not change logical work");
            }
            prior_work = Some(work);
            let hits = cache_after.hits - cache_before.hits;
            let misses = cache_after.misses - cache_before.misses;
            assert_eq!(misses, delta.object_reads());
            work_rows.push(format!(
                "| {cache} | {mode} | {hits} | {misses} | {path_visits} | {} | {} | {} | {generate_us} | {verify_us} |",
                generated.supplied_nodes,
                generated.posting_count,
                generated.receipt.spent.units()
            ));
            physical.push(delta);
        }
        let cold = &physical[0];
        let warm = &physical[1];
        let s = engine.stats();
        assert_resident(&engine, cache);
        if cache > 0 {
            assert_eq!(warm.object_reads(), 0);
        }
        println!(
            "| {cache} | {} | {} | {} | {} | {} | {} | {} |",
            cold.object_reads(),
            warm.object_reads(),
            cold.backend.unwrap().read_bytes,
            warm.backend.unwrap().read_bytes,
            expected.len(),
            s.cache.retained_bytes,
            s.cache.entries
        );
    }
    println!(
        "\nLogical path visits are the traced `ProofNode` ledger count, including inline nodes visited while building witness paths. They exclude preliminary value lookups and posting iteration, whose aggregate trie visits are not exposed by the proof API. Witness nodes are emitted encodings; hits plus misses count stored-object loader calls, not all logical visits. Charges, path visits, witnesses and posting count are asserted equal across all six runs. Generation and independent verification are timed separately; single-call microsecond results are observations, not latency benchmarks.\n"
    );
    println!(
        "| cache bytes | pass | cache hits | cache misses | logical proof-path visits | witness nodes | posting IDs | proof charge | generate μs | verify μs |\n| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in work_rows {
        println!("{row}");
    }
    let (store, engine) = open(path, 64 << 10);
    let before = store.stats();
    let rss_before = rss();
    let hwm_before = high_water_rss();
    let handles: Vec<_> = (1..=1000)
        .map(|n| engine.snapshot(SnapshotId::from_sequence(n)).unwrap())
        .collect();
    assert_eq!(store.stats().since(&before).object_reads(), 0);
    let rss_created = rss();
    let inline_bytes = std::mem::size_of::<Snapshot>();
    let vector_capacity_bytes = handles.capacity() * inline_bytes;
    for snapshot in &handles {
        assert!(snapshot.staged_objects().is_empty());
        snapshot.try_namespace_roots(ns(1)).unwrap();
    }
    let rss_live = rss();
    let hwm_live = high_water_rss();
    let stats = engine.stats();
    drop(handles);
    println!(
        "\n## Historical handle ownership and memory\n\n1,000 distinct held historical handles perform zero trie-object reads during creation. Each `Snapshot` occupies {inline_bytes} inline bytes; the handle vector capacity occupies {vector_capacity_bytes} bytes. Each additionally owns a private fixed-size root/header/context allocation; its heap layout and allocator overhead are not exposed by the public API.\n\n| phase | process RSS KiB |\n| --- | ---: |\n| before handles | {rss_before} |\n| after creation, before reads | {rss_created} |\n| after reading through all handles | {rss_live} |\n| after dropping handles | {} |\n\nCreation changes process RSS by {:.1} bytes per handle on average. This includes allocator/backend effects and reuse, so it is not an isolated per-handle heap cost. Linux-reported process `VmHWM` changes {hwm_before} → {hwm_live} KiB; it is process-wide and is **not** peak live decode memory. Cache after reads: {} bytes/{} entries; engine resident manifests: {}. Handles retain roots/header/context state; allocator RSS need not fall on drop.",
        rss(),
        (rss_created as f64 - rss_before as f64) * 1024.0 / 1000.0,
        stats.cache.retained_bytes,
        stats.cache.entries,
        stats.resident_manifests
    );
}

fn target_probe(engine: &DurableEngine, store: &RedbStore) -> (Receipt, VerifiedEq) {
    let snapshot = engine.head_snapshot();
    let request = proof_request(&snapshot, ns(1));
    let before = store.stats();
    let generated = snapshot
        .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
        .unwrap();
    let result = verify_eq(
        &request,
        None,
        &generated.bytes,
        ProofLimits::default(),
        genesis().limits,
    )
    .unwrap();
    let delta = store.stats().since(&before);
    assert_eq!(delta.space(SpaceId::Namespace(2)).reads, 0);
    println!(
        "| {} | {} | {} | {} | {} | {} | {} |",
        engine.head().sequence(),
        delta.space(SpaceId::Namespace(1)).reads,
        delta.space(SpaceId::Namespace(2)).reads,
        delta.space(SpaceId::Catalog).reads,
        delta.backend.unwrap().read_bytes,
        generated.receipt.spent.units(),
        generated.bytes.len()
    );
    (generated.receipt, result)
}

fn live_growth() {
    let dir = BenchDir::new("engine-live-growth");
    let path = dir.file("growth.redb");
    let mut store = Arc::new(RedbStore::create(&path, redb_config()).unwrap());
    let mut engine = initialize(store.clone(), 0);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    namespace(&mut branch, "fixed");
    namespace(&mut branch, "growing");
    for n in 0..128 {
        row(&mut branch, ns(1), n);
    }
    publish(&mut engine, branch);
    let mut inserted = 0;
    let mut update_rows = Vec::new();
    let mut write_rows = Vec::new();
    let mut prior_probe = None;
    println!(
        "\n## Namespace isolation\n\nNamespace 1 remains at 128 rows; namespace 2 grows from 1,000 to 10,000 rows. Each probe runs after closing/reopening redb and with the node cache disabled.\n"
    );
    println!(
        "| snapshot | target object reads | foreign object reads | catalog reads | backend bytes | proof charge | proof bytes |\n| ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for count in [1000, 10000] {
        while inserted < count {
            let end = (inserted + 100).min(count);
            let mut branch = engine.begin(engine.head(), None).unwrap();
            for n in inserted..end {
                row(&mut branch, ns(2), n);
            }
            publish(&mut engine, branch);
            inserted = end;
        }
        drop(engine);
        drop(store);
        (store, engine) = open(&path, 0);
        let probe = target_probe(&engine, &store);
        if let Some(prior) = prior_probe {
            assert_eq!(
                probe, prior,
                "foreign growth changes neither rows nor charges"
            );
        }
        prior_probe = Some(probe);
        let before = store.stats();
        let mut branch = engine.begin(engine.head(), None).unwrap();
        execute(
            &mut branch,
            WriteOp::PatchRecord {
                namespace: ns(2),
                key: key(500),
                set: vec![(
                    name("payload"),
                    Cell::field(Value::bytes(&[count as u8; 64])),
                )],
                remove: vec![],
            },
        );
        let before_prepare = store.stats();
        let info = engine.prepare(branch.seal()).unwrap();
        let preparation = store.stats().since(&before_prepare);
        assert_eq!(preparation.object_reads(), 0);
        let s = engine.stats().last_preparation;
        engine.commit().unwrap();
        let delta = store.stats().since(&before);
        assert_eq!(delta.space(SpaceId::Namespace(1)).objects_written, 0);
        assert_eq!(delta.records_written, 2);
        assert_eq!(
            delta
                .spaces
                .values()
                .map(|s| s.objects_written)
                .sum::<u64>(),
            s.unique_objects as u64,
            "these updated object IDs must all be new for the byte accounting"
        );
        // Read exact written values only after taking the write-counter delta.
        let manifest_key = info.snapshot.sequence().to_be_bytes();
        let index_key = info.commit_id.as_slice();
        let manifest = store
            .read_record_bounded(Table::Manifests, &manifest_key, 1024)
            .unwrap()
            .unwrap();
        let index = store
            .read_record_bounded(Table::BlockIndex, index_key, 8)
            .unwrap()
            .unwrap();
        let record_bytes = manifest_key.len() + manifest.len() + index_key.len() + index.len();
        let object_bytes = delta.spaces.values().map(|s| s.bytes_written).sum::<u64>();
        let object_key_bytes = 32 * s.unique_objects;
        assert_eq!(
            s.payload_bytes as u64,
            object_bytes + (object_key_bytes + record_bytes) as u64
        );
        let backend = delta.backend.unwrap();
        write_rows.push(format!(
            "| {count} | {object_bytes} | {object_key_bytes} | {record_bytes} | {} | {} | {} | {} | {} | {:.2} |",
            Head::ENCODED_LEN,
            s.payload_bytes,
            backend.writes,
            backend.write_bytes,
            backend.syncs,
            backend.write_bytes as f64 / s.payload_bytes as f64
        ));
        update_rows.push(format!(
            "| {count} | {} | {} | {} | {} | {} | {} | {} | {} |",
            s.unique_objects,
            s.payload_bytes,
            s.preflight_visits,
            s.staging_visits,
            preparation.object_reads(),
            delta.space(SpaceId::Namespace(1)).objects_written,
            delta.space(SpaceId::Namespace(2)).bytes_written,
            delta.backend.unwrap().write_bytes
        ));
        let branch = engine.begin(engine.head(), None).unwrap();
        let before = store.stats();
        publish(&mut engine, branch);
        assert!(
            store
                .stats()
                .since(&before)
                .spaces
                .values()
                .all(|s| s.objects_written == 0)
        );
        assert_eq!(engine.stats().last_preparation.unique_objects, 0);
        assert_resident(&engine, 0);
    }
    println!(
        "\n## Incremental fixed-size updates as live state grows\n\nA single field update in namespace 2; namespace 1 is untouched. Empty successors at both sizes wrote zero trie objects. Physical redb write amplification is measured separately.\n"
    );
    println!(
        "| growing rows | staged objects | batch payload bytes | preflight visits | staging visits | staging object reads | foreign objects written | namespace object bytes written | backend bytes written |\n| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in update_rows {
        println!("{row}");
    }
    println!(
        "\nThe two records are the manifest and manifest-digest index. Their bytes include keys and values, measured by reading those exact bounded records after commit. All staged object IDs are asserted newly written, so encoded object bytes + 32-byte IDs + record bytes must equal the prepared batch payload. Amplification is backend write bytes / this logical payload, following `CommitBatch::logical_bytes`; the separate 40-byte head value is excluded from that denominator. The immutable metadata is not rewritten in a successor.\n"
    );
    println!(
        "| growing rows | all encoded object bytes | object ID bytes | record key/value bytes | head bytes | logical payload bytes | backend write calls | backend write bytes | syncs | amplification × |\n| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for row in write_rows {
        println!("{row}");
    }
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("run this measurement with --release; redb debug opens have additional scans");
        std::process::exit(2);
    }
    let base = common::base_dir();
    std::fs::create_dir_all(&base).unwrap();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap();
    let cpu = cpuinfo
        .lines()
        .find_map(|line| line.strip_prefix("model name\t: "))
        .unwrap_or("unknown");
    println!(
        "# Engine storage v1 measured report\n\n- Date: {}\n- Command: `cargo run --release --offline -p arkiv-db-redb --example engine_storage_experiment`\n- {}\n- CPU: {cpu}; logical CPUs available: {}\n- Kernel: {}\n- Filesystem: {}; path: {}\n- redb 4.3.0; quick repair on; redb page cache 8 MiB. Node-cache options are stated per experiment.\n- This measures engine snapshots, not node block/body/signing restart. No pruning or compaction.",
        command("date", &["-u", "+%Y-%m-%d %H:%M UTC"]),
        command("rustc", &["--version"]),
        std::thread::available_parallelism().unwrap().get(),
        command("uname", &["-r"]),
        filesystem_of(&base),
        base.display()
    );
    history();
    live_growth();
    println!(
        "\n## Interpretation and unavailable attribution\n\nAll structural/counter assertions passed. Application cold open performed eight record reads (including the state-floor lookup), one head read and zero trie-object reads at all history sizes. The engine kept one resident manifest and no pending payload after publication. Read handles retained stored roots; direct weak-reference release tests live in the engine/trie suites.\n\nIsolated peak live decode bytes/nodes, private per-handle heap overhead, and resident redb metadata bytes are **not measured**: the public engine/redb APIs expose no allocation ownership counters. A scoped global allocator would still combine decoder, traversal, result, cache and backend allocations; implementing one here would also require unsafe code forbidden by the workspace. An exact attribution needs dedicated allocation hooks or a separately configured profiler. The sampled `/proc/self/status` RSS and `VmHWM` fields are kernel-reported process values, not allocation counters or a scoped active-call allocation peak. The redb page-cache setting is a configured bound, not a measured resident allocation. Disk layout numbers likewise cannot be used as resident metadata bytes.\n\nRSS includes redb allocator/table metadata, Rust allocator retention and active decode/response buffers as well as the bounded cache. These observations do not establish a universal process-memory bound or cold-device latency. OS page-cache eviction was not performed. Other namespaces caused no namespace-local reads in the fixed target proof, while shared catalog/backend work is separately reported."
    );
}
