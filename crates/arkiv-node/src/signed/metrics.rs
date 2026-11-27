//! Optional, private Prometheus exporter. Scrapes never walk redb or lock the node.
//!
//! By default nothing walks the database: file sizes are read with `stat` at
//! each scrape and storage counters are kept by the store. The table walk
//! (`RedbStore::data_stats`) is opt-in (`table_stats`), because it holds one
//! redb read transaction for the whole walk, and redb cannot reuse any page
//! freed while that transaction is open: on a large state the file grows by
//! every commit's copy-on-write pages for the length of the walk.

use super::{
    Error, ErrorCode, Result,
    runtime::{Node, Shared},
};
use arkiv_db::{MapKind, durable::CacheStats};
use arkiv_db_redb::{DURATION_BUCKETS_NANOSECONDS, DataStats, OperationTimingSnapshot, RedbStore};
use arkiv_db_storage::versioned::{SpaceId, SpaceStats, StoreStats, VersionedStore};
use axum::{Router, extract::State, routing::get};
use std::{
    collections::BTreeMap,
    fmt::{Display, Write},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct Database {
    pub name: &'static str,
    pub path: PathBuf,
    pub store: Arc<RedbStore>,
}

/// The latest opt-in table walk of one database.
#[derive(Default)]
struct Sample {
    data: Option<DataStats>,
    allocated: Option<u64>,
    timestamp: Option<f64>,
    duration: f64,
    success: bool,
    errors: u64,
}

#[derive(Default)]
struct NodeSample {
    head: Option<(u64, f64)>,
    timestamp: Option<f64>,
    pool: Option<(usize, usize)>,
    paused: Option<bool>,
    observed: Option<u64>,
    retention: Option<RetentionSample>,
    /// The engine's decoded-node cache counters.
    cache: Option<CacheStats>,
}

/// What bounds deletion on a full node: read from the engine's atomics.
#[derive(Clone, Copy, Default)]
struct RetentionSample {
    floor: u64,
    durable_floor: u64,
    horizon: u64,
    lowest_pin: Option<u64>,
    pinned_handles: usize,
    /// The record floor and the heights below it whose records are still
    /// stored.
    record_floor: u64,
    record_backlog: u64,
}

#[derive(Default)]
pub struct Activity {
    calls: AtomicU64,
    errors: AtomicU64,
    busy: AtomicU64,
    produced: AtomicU64,
    nanos: AtomicU64,
}

impl Activity {
    pub fn record(&self, elapsed: Duration, result: &Result<bool>) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(
            elapsed.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        match result {
            Ok(true) => {
                self.produced.fetch_add(1, Ordering::Relaxed);
            }
            Err(Error {
                code: ErrorCode::ServerBusy,
                ..
            }) => {
                self.busy.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub struct Metrics {
    databases: Vec<Database>,
    /// Whether the periodic table walk runs (`--metrics-table-stats-secs`).
    table_stats: bool,
    samples: RwLock<Vec<Sample>>,
    node: RwLock<NodeSample>,
    started: Instant,
    role: &'static str,
    pub activity: Activity,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// One metric family: its declaration and its samples in the order added.
struct Family {
    kind: &'static str,
    help: &'static str,
    samples: String,
}

/// The exported families by name, without the `rogue_one_` prefix. Collected
/// per family because Prometheus text requires every family's samples to be
/// contiguous: one database's samples are not, and histogram buckets, count
/// and sum must not be interleaved with the error counter.
#[derive(Default)]
struct Families(BTreeMap<&'static str, Family>);

impl Families {
    /// The samples of family `name`, declared on first use.
    fn family(
        &mut self,
        kind: &'static str,
        name: &'static str,
        help: &'static str,
    ) -> &mut String {
        &mut self
            .0
            .entry(name)
            .or_insert(Family {
                kind,
                help,
                samples: String::new(),
            })
            .samples
    }

    fn sample(
        &mut self,
        kind: &'static str,
        name: &'static str,
        help: &'static str,
        labels: &str,
        value: impl Display,
    ) {
        let samples = self.family(kind, name, help);
        let _ = writeln!(samples, "rogue_one_{name}{labels} {value}");
    }

    fn gauge(&mut self, name: &'static str, help: &'static str, labels: &str, value: impl Display) {
        self.sample("gauge", name, help, labels, value);
    }

    fn counter(
        &mut self,
        name: &'static str,
        help: &'static str,
        labels: &str,
        value: impl Display,
    ) {
        self.sample("counter", name, help, labels, value);
    }

    /// The duration histogram and error counter of each operation of one
    /// database layer.
    fn timings(&mut self, database: &str, layer: &str, operations: &[OperationTimingSnapshot]) {
        for operation in operations {
            let labels = format!(
                "database=\"{database}\",layer=\"{layer}\",operation=\"{}\"",
                operation.operation
            );
            let histogram = self.family("histogram", OPERATION_DURATION, OPERATION_DURATION_HELP);
            for (upper, count) in DURATION_BUCKETS_NANOSECONDS
                .iter()
                .zip(operation.cumulative_buckets)
            {
                let _ = writeln!(
                    histogram,
                    "rogue_one_{OPERATION_DURATION}_bucket{{{labels},le=\"{}\"}} {count}",
                    *upper as f64 / 1e9
                );
            }
            let _ = writeln!(
                histogram,
                "rogue_one_{OPERATION_DURATION}_bucket{{{labels},le=\"+Inf\"}} {}\n\
                 rogue_one_{OPERATION_DURATION}_count{{{labels}}} {}\n\
                 rogue_one_{OPERATION_DURATION}_sum{{{labels}}} {}",
                operation.count,
                operation.count,
                operation.total_nanoseconds as f64 / 1e9,
            );
            self.counter(
                OPERATION_ERRORS,
                OPERATION_ERRORS_HELP,
                &format!("{{{labels}}}"),
                operation.errors,
            );
        }
    }

    /// The Prometheus text exposition, families in name order.
    fn render(self) -> String {
        let mut out = String::with_capacity(20000);
        for (name, family) in self.0 {
            let _ = write!(
                out,
                "# HELP rogue_one_{name} {}\n# TYPE rogue_one_{name} {}\n{}",
                family.help, family.kind, family.samples
            );
        }
        out
    }
}

const OPERATION_DURATION: &str = "db_operation_duration_seconds";
const OPERATION_DURATION_HELP: &str =
    "Wall time of storage contract calls or backend I/O, including failures.";
const OPERATION_ERRORS: &str = "db_operation_errors_total";
const OPERATION_ERRORS_HELP: &str = "Failed operation calls including indeterminate commits.";

/// One node sample, or `None` when the node lock is busy (never waits).
fn sample_node(rt: &Shared) -> Option<NodeSample> {
    let node = rt.node.try_lock().ok()?;
    let head = node.head();
    let mut sample = NodeSample {
        head: Some((head.height, head.timestamp_ms as f64 / 1000.)),
        timestamp: Some(now()),
        ..NodeSample::default()
    };
    match &*node {
        Node::Full(h) => {
            sample.pool = Some((h.pool.len(), h.pool.bytes()));
            sample.paused = Some(h.paused);
            let status = h.engine.retention_status();
            sample.retention = Some(RetentionSample {
                floor: status.floor,
                durable_floor: status.durable_floor,
                horizon: h.engine.gc_horizon(),
                lowest_pin: status.lowest_pinned,
                pinned_handles: status.pinned_handles,
                record_floor: h.engine.record_floor().floor,
                record_backlog: h.engine.record_floor().backlog(),
            });
            sample.cache = Some(h.engine.cache_stats());
        }
        Node::Light { light, .. } => {
            sample.observed = Some(light.observed);
        }
    }
    Some(sample)
}

impl Metrics {
    pub async fn start(
        rt: Shared,
        databases: Vec<Database>,
        role: &'static str,
        listen: &str,
        table_stats: Option<Duration>,
    ) -> Result<Arc<Self>> {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|_| ErrorCode::MetricsBindFailed)?;
        let metrics = Arc::new(Self {
            samples: RwLock::new((0..databases.len()).map(|_| Sample::default()).collect()),
            databases,
            table_stats: table_stats.is_some(),
            node: RwLock::new(NodeSample::default()),
            started: Instant::now(),
            role,
            activity: Activity::default(),
        });
        let sampled = metrics.clone();
        tokio::spawn(async move {
            loop {
                // Cheap metadata only; never wait behind production or proofs.
                // A busy producer holds the lock most of the time, so retry
                // briefly instead of skipping the whole second: one try a
                // second left the sample frozen at its first value.
                let Some(sample) = sample_node(&rt) else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                };
                *sampled.node.write().unwrap_or_else(|e| e.into_inner()) = sample;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        if let Some(interval) = table_stats {
            let sampled = metrics.clone();
            tokio::spawn(async move {
                loop {
                    let worker = sampled.clone();
                    // A single read-only scan at a time, with interval AFTER completion.
                    // No node mutex and no redb writer transaction are acquired,
                    // but freed pages cannot be reused until the scan ends.
                    if tokio::task::spawn_blocking(move || worker.refresh())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        }
        let router = Router::new()
            .route("/metrics", get(scrape))
            .with_state(metrics.clone());
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                eprintln!("metrics listener: {error}");
            }
        });
        Ok(metrics)
    }

    /// One table walk of every database (opt-in, see the module docs).
    fn refresh(&self) {
        for (i, db) in self.databases.iter().enumerate() {
            let started = Instant::now();
            let data = db.store.data_stats();
            let file = FileSize::of(&db.path);
            let mut samples = self.samples.write().unwrap_or_else(|e| e.into_inner());
            let sample = &mut samples[i];
            sample.duration = started.elapsed().as_secs_f64();
            sample.success = data.is_ok() && file.is_some();
            if let (Ok(data), Some(file)) = (data, file) {
                sample.data = Some(data);
                sample.allocated = file.allocated;
                sample.timestamp = Some(now());
            } else {
                sample.errors += 1;
            }
        }
    }

    pub fn render(&self) -> String {
        let mut families = Families::default();
        families.gauge(
            "node_info",
            "Node role; metric labels never contain keys or paths.",
            &format!("{{role=\"{}\"}}", self.role),
            1,
        );
        families.gauge(
            "uptime_seconds",
            "Exporter uptime in seconds.",
            "",
            self.started.elapsed().as_secs_f64(),
        );
        self.node
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .export(&mut families);
        self.activity.export(&mut families);
        // Declared even when no database has an operation to report.
        families.family("histogram", OPERATION_DURATION, OPERATION_DURATION_HELP);
        families.family("counter", OPERATION_ERRORS, OPERATION_ERRORS_HELP);
        let samples = self.samples.read().unwrap_or_else(|e| e.into_inner());
        for (db, sample) in self.databases.iter().zip(samples.iter()) {
            families.timings(db.name, "store", &db.store.operation_timings());
            families.timings(db.name, "backend", &db.store.probe().operation_timings());
            let labels = format!("{{database=\"{}\"}}", db.name);
            if let Some(file) = FileSize::of(&db.path) {
                file.export(&mut families, &labels);
            }
            if let Ok(Some(state)) = db.store.gc_state() {
                export_gc(
                    &mut families,
                    &labels,
                    &state,
                    db.store.gc_report(),
                    db.store.gc_seconds(),
                );
            }
            if self.table_stats {
                sample.export(&mut families, &labels);
            }
            let io = db.store.probe().stats();
            let commits = db.store.stats();
            export_store(&mut families, db.name, &commits);
            for (name, help, value) in [
                (
                    "db_backend_read_bytes_total",
                    "Successful backend read bytes, below redb cache; includes stats scans.",
                    io.read_bytes,
                ),
                (
                    "db_backend_write_bytes_total",
                    "Successful backend write bytes.",
                    io.write_bytes,
                ),
                (
                    "db_commits_total",
                    "Successful storage commits (with --state-durable-every, durable or not).",
                    commits.committed,
                ),
                (
                    "db_commit_not_committed_total",
                    "Rejected storage commits.",
                    commits.not_committed,
                ),
                (
                    "db_commit_indeterminate_total",
                    "Storage commits with uncertain durability.",
                    commits.indeterminate,
                ),
            ] {
                families.counter(name, help, &labels, value);
            }
        }
        families.render()
    }
}

impl NodeSample {
    fn export(&self, families: &mut Families) {
        if let Some((height, timestamp)) = self.head {
            families.gauge(
                "head_height",
                "Last sampled locally verified head height.",
                "",
                height,
            );
            families.gauge(
                "head_timestamp_seconds",
                "Last sampled head Unix timestamp.",
                "",
                timestamp,
            );
            families.gauge(
                "head_timestamp_lag_seconds",
                "Wall clock minus sampled head timestamp; includes normal block interval and \
                 sample age.",
                "",
                now() - timestamp,
            );
        }
        if let Some(timestamp) = self.timestamp {
            families.gauge(
                "node_sample_timestamp_seconds",
                "Last successful node sample Unix timestamp.",
                "",
                timestamp,
            );
        }
        if let Some((count, bytes)) = self.pool {
            families.gauge(
                "pending_transactions",
                "Transactions in local pool.",
                "",
                count,
            );
            families.gauge(
                "pending_transaction_bytes",
                "Encoded transaction bytes in local pool.",
                "",
                bytes,
            );
        }
        if let Some(paused) = self.paused {
            families.gauge(
                "production_paused",
                "Whether full host is paused (producer only meaningful).",
                "",
                u8::from(paused),
            );
        }
        if let Some(observed) = self.observed {
            families.gauge(
                "peer_observed_height",
                "Authenticated peer head last observed by light node.",
                "",
                observed,
            );
        }
        if let Some(r) = self.retention {
            families.gauge(
                "state_floor",
                "Lowest height whose state is served.",
                "",
                r.floor,
            );
            families.gauge(
                "state_durable_floor",
                "State floor recorded with the durable head.",
                "",
                r.durable_floor,
            );
            families.gauge(
                "state_gc_horizon_height",
                "Lowest height a reader may still open: min(durable floor, lowest pin). \
                 Objects that died before it are deleted.",
                "",
                r.horizon,
            );
            if let Some(pin) = r.lowest_pin {
                families.gauge(
                    "state_lowest_pin_height",
                    "Lowest snapshot height an open handle reads; a stuck one holds deletion.",
                    "",
                    pin,
                );
            }
            families.gauge(
                "state_pinned_handles",
                "Open snapshot handles.",
                "",
                r.pinned_handles,
            );
            families.gauge(
                "state_record_floor",
                "Lowest height (above 0) whose per-height records (feed, block, inclusions) \
                 are kept.",
                "",
                r.record_floor,
            );
            families.gauge(
                "state_record_backlog_heights",
                "Expired heights whose records are not deleted yet.",
                "",
                r.record_backlog,
            );
        }
        if let Some(cache) = &self.cache {
            export_cache(families, cache);
        }
    }
}

/// The Prometheus label of a map kind.
fn map_label(map: MapKind) -> &'static str {
    match map {
        MapKind::Catalog => "catalog",
        MapKind::Host => "host",
        MapKind::Records => "records",
        MapKind::Keys => "keys",
        MapKind::Rows => "rows",
        MapKind::Terms => "terms",
        MapKind::Postings => "postings",
    }
}

/// The state engine's decoded-node cache: counters the cache keeps in
/// atomics, as of the last node sample.
fn export_cache(families: &mut Families, cache: &CacheStats) {
    for map in MapKind::ALL {
        let labels = format!("{{map=\"{}\"}}", map_label(map));
        families.counter(
            "state_cache_hits_total",
            "Trie node loads answered by the decoded-node cache, without a store read.",
            &labels,
            cache.hits_by_map[map.index()],
        );
        families.counter(
            "state_cache_misses_total",
            "Trie node loads that read the store (one object read and a decode each).",
            &labels,
            cache.misses_by_map[map.index()],
        );
    }
    families.counter(
        "state_cache_evictions_total",
        "Decoded nodes evicted from the cache to stay within its byte and entry limits.",
        "",
        cache.evictions,
    );
    families.counter(
        "state_cache_miss_read_seconds_total",
        "Wall time of the store reads of cache misses, summed over reader threads; \
         decoding excluded.",
        "",
        cache.miss_read_ns as f64 / 1e9,
    );
}

/// The store's logical counters: objects and records through the storage
/// contract. Namespaces are summed, so the `space` label stays bounded and
/// no namespace id is exposed.
fn export_store(families: &mut Families, database: &str, stats: &StoreStats) {
    let mut namespaces = SpaceStats::default();
    for (space, stats) in &stats.spaces {
        if matches!(space, SpaceId::Namespace(_)) {
            namespaces.reads += stats.reads;
            namespaces.read_bytes += stats.read_bytes;
            namespaces.objects_written += stats.objects_written;
            namespaces.bytes_written += stats.bytes_written;
        }
    }
    for (space, stats) in [
        ("catalog", stats.space(SpaceId::Catalog)),
        ("host", stats.space(SpaceId::Host)),
        ("namespace", namespaces),
    ] {
        let labels = format!("{{database=\"{database}\",space=\"{space}\"}}");
        for (name, help, value) in [
            (
                "db_object_reads_total",
                "Answered store object reads, found or absent; reads the engine cache \
                 answers never reach the store.",
                stats.reads,
            ),
            (
                "db_object_read_bytes_total",
                "Encoded bytes of the objects store reads returned.",
                stats.read_bytes,
            ),
            (
                "db_objects_written_total",
                "Objects newly stored by committed batches; an object already stored is \
                 not counted again.",
                stats.objects_written,
            ),
            (
                "db_objects_written_bytes_total",
                "Encoded bytes (values only) of the objects newly stored.",
                stats.bytes_written,
            ),
        ] {
            families.counter(name, help, &labels, value);
        }
    }
    let labels = format!("{{database=\"{database}\"}}");
    for (name, help, value) in [
        (
            "db_record_reads_total",
            "Answered store record reads, found or absent.",
            stats.record_reads,
        ),
        (
            "db_record_read_bytes_total",
            "Bytes of the records store reads returned.",
            stats.record_read_bytes,
        ),
        (
            "db_records_written_total",
            "Records newly stored by committed batches.",
            stats.records_written,
        ),
        (
            "db_entries_removed_total",
            "Records and objects removed by committed batches (expiry); objects deleted \
             by garbage collection are in state_gc_deleted_total.",
            stats.removed,
        ),
    ] {
        families.counter(name, help, &labels, value);
    }
}

impl Activity {
    fn export(&self, families: &mut Families) {
        for (name, help, count) in [
            (
                "node_tick_total",
                "Producer attempts or follower polls, including empty/busy attempts.",
                &self.calls,
            ),
            (
                "node_tick_errors_total",
                "Failed producer attempts or follower polls excluding contention.",
                &self.errors,
            ),
            (
                "node_tick_busy_total",
                "Producer attempts or follower polls encountering ServerBusy.",
                &self.busy,
            ),
            (
                "blocks_produced_total",
                "Blocks produced by the automatic production loop since exporter start.",
                &self.produced,
            ),
        ] {
            families.counter(name, help, "", count.load(Ordering::Relaxed));
        }
        families.counter(
            "node_tick_duration_seconds_total",
            "Total elapsed producer attempt or follower poll time.",
            "",
            self.nanos.load(Ordering::Relaxed) as f64 / 1e9,
        );
    }
}

/// The garbage-collection counters the store keeps in every commit: no
/// walk, no read.
fn export_gc(
    families: &mut Families,
    labels: &str,
    state: &arkiv_db_storage::gc::GcState,
    report: Option<arkiv_db_storage::gc::GcReport>,
    seconds: f64,
) {
    for (name, help, value) in [
        (
            "state_gc_suspended",
            "1 when an invariant failed and collection stopped until a rebuild.",
            u64::from(state.suspended),
        ),
        ("state_gc_epoch", "Commit epoch of the head.", state.epoch),
        (
            "state_gc_stored_objects",
            "Objects in the object tables, live or dead.",
            state.stored_objects,
        ),
        (
            "state_gc_stored_bytes",
            "Encoded bytes of the stored objects (values only).",
            state.stored_bytes,
        ),
        (
            "state_gc_live_objects",
            "Stored objects that are not dead.",
            state.live_objects(),
        ),
        (
            "state_gc_live_bytes",
            "Stored bytes not queued for deletion (pending objects included).",
            state.live_bytes(),
        ),
        (
            "state_gc_dead_objects",
            "Dead objects not yet deleted: pending release or queued.",
            state.dead_objects,
        ),
        (
            "state_gc_pending_release",
            "Dead objects whose references are not released yet (backlog).",
            state.pending,
        ),
        (
            "state_gc_queued_objects",
            "Released dead objects waiting for the horizon.",
            state.queued,
        ),
        (
            "state_gc_queued_bytes",
            "Bytes of the queued dead objects.",
            state.queued_bytes,
        ),
        (
            "state_gc_shared_entries",
            "Rows of the sparse count table (objects referenced twice or more).",
            state.shared,
        ),
        (
            "state_gc_phantom_entries",
            "Rows counting speculative references to absent objects.",
            state.phantoms,
        ),
    ] {
        families.gauge(name, help, labels, value);
    }
    if let Some(report) = report {
        for (name, help, value) in [
            (
                "state_gc_last_released",
                "Dead objects released by the last commit.",
                report.released,
            ),
            (
                "state_gc_last_deleted",
                "Objects deleted by the last commit.",
                report.deleted,
            ),
            (
                "state_gc_last_eligible",
                "Deletable objects when the last commit started (deletion backlog).",
                report.eligible,
            ),
        ] {
            families.gauge(name, help, labels, value);
        }
    }
    for (name, help, value) in [
        (
            "state_gc_born_total",
            "Objects inserted since the counts were built.",
            state.born_total,
        ),
        (
            "state_gc_died_total",
            "Objects whose count reached zero.",
            state.died_total,
        ),
        (
            "state_gc_released_total",
            "Dead objects whose references were released.",
            state.released_total,
        ),
        (
            "state_gc_deleted_total",
            "Objects deleted.",
            state.deleted_total,
        ),
        (
            "state_gc_deleted_bytes_total",
            "Bytes of the objects deleted.",
            state.deleted_bytes_total,
        ),
        (
            "state_gc_resurrected_total",
            "Dead objects a later commit brought back.",
            state.resurrected_total,
        ),
        (
            "state_gc_over_budget_total",
            "Commits that released or deleted past their budget to shrink a backlog.",
            state.over_budget_total,
        ),
    ] {
        families.counter(name, help, labels, value);
    }
    families.counter(
        "state_gc_seconds_total",
        "Wall time of garbage collection work inside commits in this process: reference \
         counting, release, deletion and the GC tables; excludes storing the block's own \
         objects and records and the durable commit (page writes, fsync).",
        labels,
        seconds,
    );
}

/// A file's size from one `stat`: cheap enough for every scrape.
struct FileSize {
    apparent: u64,
    allocated: Option<u64>,
}

impl FileSize {
    fn of(path: &std::path::Path) -> Option<Self> {
        let file = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let allocated = {
            use std::os::unix::fs::MetadataExt;
            Some(file.blocks() * 512)
        };
        #[cfg(not(unix))]
        let allocated = None;
        Some(Self {
            apparent: file.len(),
            allocated,
        })
    }

    fn export(&self, families: &mut Families, labels: &str) {
        families.gauge(
            "db_file_apparent_bytes",
            "File length including sparse holes; read at scrape time.",
            labels,
            self.apparent,
        );
        if let Some(allocated) = self.allocated {
            families.gauge(
                "db_file_allocated_bytes",
                "Filesystem allocated bytes (Unix st_blocks times 512); read at scrape time.",
                labels,
                allocated,
            );
        }
    }
}

impl Sample {
    /// The latest table walk of the database `labels` name.
    fn export(&self, families: &mut Families, labels: &str) {
        families.gauge(
            "db_stats_refresh_success",
            "Whether the latest storage sample succeeded.",
            labels,
            u8::from(self.success),
        );
        families.gauge(
            "db_stats_refresh_duration_seconds",
            "Last storage sampling duration.",
            labels,
            self.duration,
        );
        families.counter(
            "db_stats_refresh_errors_total",
            "Failed storage sampling attempts.",
            labels,
            self.errors,
        );
        if let Some(timestamp) = self.timestamp {
            families.gauge(
                "db_stats_timestamp_seconds",
                "Last successful storage sample Unix timestamp; old samples are retained on \
                 failure.",
                labels,
                timestamp,
            );
        }
        let Some(data) = &self.data else {
            return;
        };
        let overhead = self
            .allocated
            .map(|bytes| bytes.saturating_sub(data.stored_bytes));
        for (name, help, value) in [
            (
                "db_stored_bytes",
                "Encoded keys and values across redb user tables; includes application indexes \
                 and history, not payload-only bytes.",
                Some(data.stored_bytes),
            ),
            (
                "db_table_metadata_bytes",
                "B-tree metadata within user tables; excludes database directory and allocator.",
                Some(data.metadata_bytes),
            ),
            (
                "db_table_fragmented_bytes",
                "Unused bytes inside user table pages; excludes database free pages.",
                Some(data.fragmented_bytes),
            ),
            (
                "db_entries",
                "Entries in all redb user tables, including historical objects and indexes.",
                Some(data.entries),
            ),
            (
                "db_tables",
                "Number of redb user tables.",
                Some(data.tables),
            ),
            (
                "db_disk_overhead_bytes",
                "Allocated filesystem bytes at the walk minus encoded table bytes, floored at \
                 zero; includes free pages and all metadata.",
                overhead,
            ),
        ] {
            if let Some(value) = value {
                families.gauge(name, help, labels, value);
            }
        }
    }
}

async fn scrape(
    State(metrics): State<Arc<Metrics>>,
) -> ([(&'static str, &'static str); 1], String) {
    (
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        metrics.render(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_db_redb::RedbConfig;
    /// A producer's exporter over two databases in one fresh file.
    fn two_databases() -> (PathBuf, Metrics) {
        walking_databases(true)
    }

    fn walking_databases(table_stats: bool) -> (PathBuf, Metrics) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rogue-metrics-{}-{}.redb",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Arc::new(RedbStore::create(&path, RedbConfig::default()).unwrap());
        let metrics = Metrics {
            databases: vec![
                Database {
                    name: "state",
                    path: path.clone(),
                    store: store.clone(),
                },
                Database {
                    name: "history",
                    path: path.clone(),
                    store,
                },
            ],
            table_stats,
            samples: RwLock::new(vec![Sample::default(), Sample::default()]),
            node: RwLock::new(NodeSample::default()),
            started: Instant::now(),
            role: "producer",
            activity: Activity::default(),
        };
        (path, metrics)
    }

    #[test]
    fn cached_scrape_preserves_sample_on_failure_and_declares_families_once() {
        let (path, metrics) = two_databases();
        let before = metrics.render();
        assert!(!before.contains("rogue_one_db_stored_bytes{"));
        metrics.refresh();
        let rendered = metrics.render();
        assert!(rendered.contains("rogue_one_db_stored_bytes{database=\"state\"}"));
        assert_eq!(
            rendered
                .matches("# TYPE rogue_one_db_stored_bytes gauge")
                .count(),
            1
        );
        assert!(!rendered.contains(path.to_str().unwrap()));
        // Declarations and samples of each family must be contiguous, across DBs.
        let mut declared = std::collections::HashSet::new();
        let mut current = "";
        for line in rendered.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                current = rest.split_whitespace().next().unwrap();
                assert!(declared.insert(current));
            } else if !line.starts_with('#') {
                let name = line.split(['{', ' ']).next().unwrap();
                assert!(
                    name == current
                        || ["_bucket", "_count", "_sum"]
                            .iter()
                            .any(|suffix| name.strip_suffix(suffix) == Some(current)),
                    "{line} outside {current}"
                );
            }
        }

        assert!(rendered.contains("layer=\"backend\",operation=\"sync_data\",le=\"+Inf\"}"));
        let samples = metrics.samples.read().unwrap();
        assert!(samples[0].success);
        assert!(samples[0].data.as_ref().unwrap().stored_bytes > 0);
        let previous = samples[0].timestamp;
        drop(samples);
        std::fs::remove_file(&path).unwrap();
        metrics.refresh();
        let samples = metrics.samples.read().unwrap();
        assert!(!samples[0].success);
        assert_eq!(samples[0].timestamp, previous);
        assert_eq!(samples[0].errors, 1);
    }

    /// Every line of a scrape with its value masked: values are clocks,
    /// durations and byte counts.
    fn shape(rendered: &str) -> String {
        rendered
            .lines()
            .map(|line| match line.rsplit_once(' ') {
                Some((sample, _)) if !line.starts_with('#') => format!("{sample} _\n"),
                _ => format!("{line}\n"),
            })
            .collect()
    }

    #[test]
    fn scrape_text_keeps_its_families_help_and_sample_order() {
        let (path, metrics) = two_databases();
        *metrics.node.write().unwrap() = NodeSample {
            head: Some((7, 1_700_000_000.5)),
            timestamp: Some(1_700_000_001.25),
            pool: Some((3, 4096)),
            paused: Some(true),
            observed: Some(9),
            retention: Some(RetentionSample {
                floor: 3,
                durable_floor: 2,
                horizon: 2,
                lowest_pin: Some(4),
                pinned_handles: 2,
                record_floor: 2,
                record_backlog: 1,
            }),
            cache: Some(CacheStats {
                hits: 5,
                misses: 2,
                evictions: 1,
                miss_read_ns: 1_500,
                ..CacheStats::default()
            }),
        };
        metrics.refresh();
        let rendered = metrics.render();
        std::fs::remove_file(&path).unwrap();
        if std::env::var_os("ARKIV_BLESS_METRICS_SHAPE").is_some() {
            std::fs::write(
                concat!(env!("CARGO_MANIFEST_DIR"), "/src/signed/metrics-shape.txt"),
                shape(&rendered),
            )
            .unwrap();
        }
        assert_eq!(shape(&rendered), include_str!("metrics-shape.txt"));
    }

    #[test]
    fn default_scrape_reports_file_sizes_without_a_table_walk() {
        let (path, metrics) = walking_databases(false);
        let reads = metrics.databases[0].store.probe().stats().read_bytes;
        let rendered = metrics.render();
        assert!(rendered.contains("rogue_one_db_file_allocated_bytes{database=\"state\"}"));
        assert!(rendered.contains("rogue_one_db_file_apparent_bytes{database=\"history\"}"));
        assert!(!rendered.contains("db_stored_bytes"));
        assert!(!rendered.contains("db_stats_refresh"));
        // The scrape read no page of the database.
        assert_eq!(metrics.databases[0].store.probe().stats().read_bytes, reads);
        std::fs::remove_file(&path).unwrap();
        // A missing file drops its gauges instead of failing the scrape.
        assert!(!metrics.render().contains("db_file_apparent_bytes{"));
    }

    /// A state file with garbage collection exports its counters at each
    /// scrape, read from the store's cached state: no table is walked.
    #[test]
    fn collecting_state_exports_gc_counters_without_reading() {
        use arkiv_db::durable::{DurableEngine, EngineGenesis, Initialization, Options, Retention};
        let (path, metrics) = walking_databases(false);
        let store = metrics.databases[0].store.clone();
        let options = Options {
            retention: Retention::Window(4),
            ..Options::default()
        };
        let Initialization::Committed(mut engine) = DurableEngine::initialize(
            store.clone(),
            EngineGenesis::new([5; 32], arkiv_db::PriceSchedule::FIXTURE),
            options,
        )
        .unwrap() else {
            panic!("initialization commits")
        };
        engine.enable_gc(None).unwrap();
        for _ in 0..3 {
            let branch = engine.begin(engine.head(), None).unwrap();
            engine.prepare(branch.try_seal().unwrap()).unwrap();
            engine.commit().unwrap();
        }
        let reads = store.probe().stats().read_bytes;
        let rendered = metrics.render();
        assert_eq!(store.probe().stats().read_bytes, reads);
        for family in [
            "rogue_one_state_gc_live_objects{database=\"state\"}",
            "rogue_one_state_gc_pending_release{database=\"state\"}",
            "rogue_one_state_gc_deleted_total{database=\"state\"}",
            "rogue_one_state_gc_last_eligible{database=\"state\"}",
            "rogue_one_state_gc_seconds_total{database=\"state\"}",
        ] {
            assert!(rendered.contains(family), "{family} missing");
        }
        // The GC time is the collection work alone: part of the commits,
        // never the whole of them.
        let commits = store.operation_timings()[3];
        assert_eq!(commits.operation, "commit");
        let collecting = store.gc_seconds();
        assert!(collecting > 0.0);
        assert!(collecting < commits.total_nanoseconds as f64 / 1e9);
        drop(engine);
        std::fs::remove_file(&path).unwrap();
    }

    /// The value of the sample `series` (name and labels) in a scrape.
    fn value(rendered: &str, series: &str) -> f64 {
        let line = rendered
            .lines()
            .find(|line| {
                line.rsplit_once(' ')
                    .is_some_and(|(name, _)| name == series)
            })
            .unwrap_or_else(|| panic!("{series} missing"));
        line.rsplit_once(' ').unwrap().1.parse().unwrap()
    }

    /// The store's object and record counters are exported per database,
    /// with every namespace summed under one `space` label.
    #[test]
    fn store_counters_sum_namespaces_and_expose_no_namespace_id() {
        use arkiv_db_storage::versioned::{CommitBatch, CommitOutcome, Head, ObjectReader, Table};
        let (path, metrics) = walking_databases(false);
        let store = metrics.databases[0].store.clone();
        let object = |tag: u8, len: usize| {
            let bytes = vec![tag; len];
            (alloy_primitives::keccak256(&bytes).0, bytes)
        };
        let head = Head {
            height: 0,
            hash: [1; 32],
        };
        let mut batch = CommitBatch::new(None, head);
        let (host, host_bytes) = object(1, 10);
        batch.put_object(SpaceId::Host, host, host_bytes);
        let (first, first_bytes) = object(2, 100);
        batch.put_object(SpaceId::Namespace(0xabcdef), first, first_bytes);
        let (second, second_bytes) = object(3, 1000);
        batch.put_object(SpaceId::Namespace(0x123456), second, second_bytes);
        batch.put_record(Table::Blocks, 0u64.to_be_bytes().to_vec(), vec![7; 40]);
        assert!(matches!(store.commit(&batch), CommitOutcome::Committed));
        store
            .read_object(SpaceId::Namespace(0xabcdef), &first)
            .unwrap();
        store
            .read_object(SpaceId::Namespace(0x123456), &second)
            .unwrap();
        store
            .read_object(SpaceId::Namespace(0x123456), &first)
            .unwrap();
        store
            .read_record(Table::Blocks, &0u64.to_be_bytes())
            .unwrap();

        let rendered = metrics.render();
        std::fs::remove_file(&path).unwrap();
        for (series, expected) in [
            (
                "db_objects_written_total{database=\"state\",space=\"host\"}",
                1.,
            ),
            (
                "db_objects_written_bytes_total{database=\"state\",space=\"host\"}",
                10.,
            ),
            (
                "db_objects_written_total{database=\"state\",space=\"namespace\"}",
                2.,
            ),
            (
                "db_objects_written_bytes_total{database=\"state\",space=\"namespace\"}",
                1100.,
            ),
            (
                "db_objects_written_total{database=\"state\",space=\"catalog\"}",
                0.,
            ),
            (
                "db_object_reads_total{database=\"state\",space=\"namespace\"}",
                3.,
            ),
            (
                "db_object_read_bytes_total{database=\"state\",space=\"namespace\"}",
                1100.,
            ),
            (
                "db_object_reads_total{database=\"state\",space=\"host\"}",
                0.,
            ),
            ("db_records_written_total{database=\"state\"}", 1.),
            ("db_record_reads_total{database=\"state\"}", 1.),
            ("db_record_read_bytes_total{database=\"state\"}", 40.),
            ("db_entries_removed_total{database=\"state\"}", 0.),
        ] {
            assert_eq!(
                value(&rendered, &format!("rogue_one_{series}")),
                expected,
                "{series}"
            );
        }
        assert!(!rendered.to_lowercase().contains("abcdef"));
        assert!(!rendered.contains("123456"));
    }

    /// Cache hits and misses are exported per map, and add up.
    #[test]
    fn cache_counters_are_exported_per_map() {
        let mut cache = CacheStats {
            evictions: 4,
            miss_read_ns: 2_500_000_000,
            ..CacheStats::default()
        };
        cache.hits_by_map[MapKind::Rows.index()] = 7;
        cache.misses_by_map[MapKind::Rows.index()] = 3;
        cache.misses_by_map[MapKind::Catalog.index()] = 1;
        let mut families = Families::default();
        export_cache(&mut families, &cache);
        let rendered = families.render();
        for (series, expected) in [
            ("state_cache_hits_total{map=\"rows\"}", 7.),
            ("state_cache_hits_total{map=\"catalog\"}", 0.),
            ("state_cache_misses_total{map=\"rows\"}", 3.),
            ("state_cache_misses_total{map=\"catalog\"}", 1.),
            ("state_cache_evictions_total", 4.),
            ("state_cache_miss_read_seconds_total", 2.5),
        ] {
            assert_eq!(
                value(&rendered, &format!("rogue_one_{series}")),
                expected,
                "{series}"
            );
        }
        assert_eq!(
            rendered
                .matches("rogue_one_state_cache_hits_total{")
                .count(),
            7
        );
    }

    #[test]
    fn attempts_distinguish_contention_failure_and_production() {
        let activity = Activity::default();
        for result in [
            Ok(true),
            Ok(false),
            Err(ErrorCode::ServerBusy.into()),
            Err(ErrorCode::StorageUnavailable.into()),
        ] {
            activity.record(Duration::from_millis(5), &result);
        }
        assert_eq!(activity.calls.load(Ordering::Relaxed), 4);
        assert_eq!(activity.produced.load(Ordering::Relaxed), 1);
        assert_eq!(activity.errors.load(Ordering::Relaxed), 1);
        assert_eq!(activity.busy.load(Ordering::Relaxed), 1);
        assert_eq!(activity.nanos.load(Ordering::Relaxed), 20_000_000);
    }
}
