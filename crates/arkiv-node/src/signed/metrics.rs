//! Optional, private Prometheus exporter. Scrapes never walk redb or lock the node.
use super::{
    Error, Result,
    runtime::{Node, Shared},
};
use arkiv_db_redb::{DURATION_BUCKETS_NANOSECONDS, DataStats, OperationTimingSnapshot, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use axum::{Router, extract::State, routing::get};
use std::{
    fmt::Write,
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
#[derive(Default)]
struct Sample {
    data: Option<DataStats>,
    apparent: Option<u64>,
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
            Err(Error("ServerBusy")) => {
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
fn gauge(out: &mut String, name: &str, help: &str, labels: &str, value: impl std::fmt::Display) {
    let _ = writeln!(
        out,
        "# HELP rogue_one_{name} {help}\n# TYPE rogue_one_{name} gauge\nrogue_one_{name}{labels} {value}"
    );
}
fn counter(out: &mut String, name: &str, help: &str, labels: &str, value: u64) {
    let _ = writeln!(
        out,
        "# HELP rogue_one_{name} {help}\n# TYPE rogue_one_{name} counter\nrogue_one_{name}{labels} {value}"
    );
}
fn timings(out: &mut String, database: &str, layer: &str, samples: &[OperationTimingSnapshot]) {
    for s in samples {
        let labels = format!(
            "database=\"{database}\",layer=\"{layer}\",operation=\"{}\"",
            s.operation
        );
        for (upper, count) in DURATION_BUCKETS_NANOSECONDS
            .iter()
            .zip(s.cumulative_buckets)
        {
            let _ = writeln!(
                out,
                "rogue_one_db_operation_duration_seconds_bucket{{{labels},le=\"{}\"}} {count}",
                *upper as f64 / 1e9
            );
        }
        let _ = writeln!(
            out,
            "rogue_one_db_operation_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\nrogue_one_db_operation_duration_seconds_count{{{labels}}} {}\nrogue_one_db_operation_duration_seconds_sum{{{labels}}} {}\nrogue_one_db_operation_errors_total{{{labels}}} {}",
            s.count,
            s.count,
            s.total_nanoseconds as f64 / 1e9,
            s.errors
        );
    }
}
impl Metrics {
    pub async fn start(
        rt: Shared,
        databases: Vec<Database>,
        role: &'static str,
        listen: &str,
        interval: Duration,
    ) -> Result<Arc<Self>> {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|_| Error("MetricsBindFailed"))?;
        let metrics = Arc::new(Self {
            samples: RwLock::new((0..databases.len()).map(|_| Sample::default()).collect()),
            databases,
            node: RwLock::new(NodeSample::default()),
            started: Instant::now(),
            role,
            activity: Activity::default(),
        });
        let sampled = metrics.clone();
        tokio::spawn(async move {
            loop {
                // Cheap metadata only; never wait behind production or proofs.
                if let Ok(node) = rt.node.try_lock() {
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
                        }
                        Node::Light { light, .. } => {
                            sample.observed = Some(light.observed);
                        }
                    }
                    *sampled.node.write().unwrap_or_else(|e| e.into_inner()) = sample;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let sampled = metrics.clone();
        tokio::spawn(async move {
            loop {
                let worker = sampled.clone();
                // A single read-only scan at a time, with interval AFTER completion.
                // No node mutex and no redb writer transaction are acquired.
                if tokio::task::spawn_blocking(move || worker.refresh())
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(interval).await;
            }
        });
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
    fn refresh(&self) {
        for (i, db) in self.databases.iter().enumerate() {
            let started = Instant::now();
            let data = db.store.data_stats();
            let file = std::fs::metadata(&db.path);
            let mut samples = self.samples.write().unwrap_or_else(|e| e.into_inner());
            let sample = &mut samples[i];
            sample.duration = started.elapsed().as_secs_f64();
            sample.success = data.is_ok() && file.is_ok();
            if let (Ok(data), Ok(file)) = (data, file) {
                sample.data = Some(data);
                sample.apparent = Some(file.len());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    sample.allocated = Some(file.blocks() * 512);
                }
                sample.timestamp = Some(now());
            } else {
                sample.errors += 1;
            }
        }
    }
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(20000);
        gauge(
            &mut out,
            "node_info",
            "Node role; metric labels never contain keys or paths.",
            &format!("{{role=\"{}\"}}", self.role),
            1,
        );
        gauge(
            &mut out,
            "uptime_seconds",
            "Exporter uptime in seconds.",
            "",
            self.started.elapsed().as_secs_f64(),
        );
        {
            let n = self.node.read().unwrap_or_else(|e| e.into_inner());
            if let Some((height, timestamp)) = n.head {
                gauge(
                    &mut out,
                    "head_height",
                    "Last sampled locally verified head height.",
                    "",
                    height,
                );
                gauge(
                    &mut out,
                    "head_timestamp_seconds",
                    "Last sampled head Unix timestamp.",
                    "",
                    timestamp,
                );
                gauge(
                    &mut out,
                    "head_timestamp_lag_seconds",
                    "Wall clock minus sampled head timestamp; includes normal block interval and sample age.",
                    "",
                    now() - timestamp,
                );
            }
            if let Some(t) = n.timestamp {
                gauge(
                    &mut out,
                    "node_sample_timestamp_seconds",
                    "Last successful node sample Unix timestamp.",
                    "",
                    t,
                );
            }
            if let Some((count, bytes)) = n.pool {
                gauge(
                    &mut out,
                    "pending_transactions",
                    "Transactions in local pool.",
                    "",
                    count,
                );
                gauge(
                    &mut out,
                    "pending_transaction_bytes",
                    "Encoded transaction bytes in local pool.",
                    "",
                    bytes,
                );
            }
            if let Some(paused) = n.paused {
                gauge(
                    &mut out,
                    "production_paused",
                    "Whether full host is paused (producer only meaningful).",
                    "",
                    u8::from(paused),
                );
            }
            if let Some(observed) = n.observed {
                gauge(
                    &mut out,
                    "peer_observed_height",
                    "Authenticated peer head last observed by light node.",
                    "",
                    observed,
                );
            }
        }
        counter(
            &mut out,
            "node_tick_total",
            "Producer attempts or follower polls, including empty/busy attempts.",
            "",
            self.activity.calls.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "node_tick_errors_total",
            "Failed producer attempts or follower polls excluding contention.",
            "",
            self.activity.errors.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "node_tick_busy_total",
            "Producer attempts or follower polls encountering ServerBusy.",
            "",
            self.activity.busy.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "blocks_produced_total",
            "Blocks produced by the automatic production loop since exporter start.",
            "",
            self.activity.produced.load(Ordering::Relaxed),
        );
        let _ = writeln!(
            out,
            "# HELP rogue_one_node_tick_duration_seconds_total Total elapsed producer attempt or follower poll time.\n# TYPE rogue_one_node_tick_duration_seconds_total counter\nrogue_one_node_tick_duration_seconds_total {}",
            self.activity.nanos.load(Ordering::Relaxed) as f64 / 1e9
        );
        out.push_str("# HELP rogue_one_db_operation_duration_seconds Wall time of storage contract calls or backend I/O, including failures.\n# TYPE rogue_one_db_operation_duration_seconds histogram\n# HELP rogue_one_db_operation_errors_total Failed operation calls including indeterminate commits.\n# TYPE rogue_one_db_operation_errors_total counter\n");
        for db in &self.databases {
            timings(&mut out, db.name, "store", &db.store.operation_timings());
            timings(
                &mut out,
                db.name,
                "backend",
                &db.store.probe().operation_timings(),
            );
        }
        let samples = self.samples.read().unwrap_or_else(|e| e.into_inner());
        for (db, sample) in self.databases.iter().zip(samples.iter()) {
            let labels = format!("{{database=\"{}\"}}", db.name);
            gauge(
                &mut out,
                "db_stats_refresh_success",
                "Whether the latest storage sample succeeded.",
                &labels,
                u8::from(sample.success),
            );
            gauge(
                &mut out,
                "db_stats_refresh_duration_seconds",
                "Last storage sampling duration.",
                &labels,
                sample.duration,
            );
            counter(
                &mut out,
                "db_stats_refresh_errors_total",
                "Failed storage sampling attempts.",
                &labels,
                sample.errors,
            );
            if let Some(t) = sample.timestamp {
                gauge(
                    &mut out,
                    "db_stats_timestamp_seconds",
                    "Last successful storage sample Unix timestamp; old samples are retained on failure.",
                    &labels,
                    t,
                );
            }
            if let Some(bytes) = sample.apparent {
                gauge(
                    &mut out,
                    "db_file_apparent_bytes",
                    "File length including sparse holes.",
                    &labels,
                    bytes,
                );
            }
            if let Some(bytes) = sample.allocated {
                gauge(
                    &mut out,
                    "db_file_allocated_bytes",
                    "Filesystem allocated bytes (Unix st_blocks times 512).",
                    &labels,
                    bytes,
                );
            }
            if let Some(data) = &sample.data {
                gauge(
                    &mut out,
                    "db_stored_bytes",
                    "Encoded keys and values across redb user tables; includes application indexes and history, not payload-only bytes.",
                    &labels,
                    data.stored_bytes,
                );
                gauge(
                    &mut out,
                    "db_table_metadata_bytes",
                    "B-tree metadata within user tables; excludes database directory and allocator.",
                    &labels,
                    data.metadata_bytes,
                );
                gauge(
                    &mut out,
                    "db_table_fragmented_bytes",
                    "Unused bytes inside user table pages; excludes database free pages.",
                    &labels,
                    data.fragmented_bytes,
                );
                gauge(
                    &mut out,
                    "db_entries",
                    "Entries in all redb user tables, including historical objects and indexes.",
                    &labels,
                    data.entries,
                );
                gauge(
                    &mut out,
                    "db_tables",
                    "Number of redb user tables.",
                    &labels,
                    data.tables,
                );
                if let Some(bytes) = sample.allocated {
                    gauge(
                        &mut out,
                        "db_disk_overhead_bytes",
                        "Allocated filesystem bytes minus encoded table bytes, floored at zero; includes free pages and all metadata.",
                        &labels,
                        bytes.saturating_sub(data.stored_bytes),
                    );
                }
            }
            let io = db.store.probe().stats();
            counter(
                &mut out,
                "db_backend_read_bytes_total",
                "Successful backend read bytes, below redb cache; includes stats scans.",
                &labels,
                io.read_bytes,
            );
            counter(
                &mut out,
                "db_backend_write_bytes_total",
                "Successful backend write bytes.",
                &labels,
                io.write_bytes,
            );
            let stats = db.store.stats();
            counter(
                &mut out,
                "db_commits_total",
                "Successful durable storage commits.",
                &labels,
                stats.committed,
            );
            counter(
                &mut out,
                "db_commit_not_committed_total",
                "Rejected storage commits.",
                &labels,
                stats.not_committed,
            );
            counter(
                &mut out,
                "db_commit_indeterminate_total",
                "Storage commits with uncertain durability.",
                &labels,
                stats.indeterminate,
            );
        }
        group_families(&out)
    }
}
// Prometheus text requires every family's samples to be contiguous. In
// particular histogram buckets/count/sum must not be interleaved with errors.
fn group_families(text: &str) -> String {
    use std::collections::BTreeMap;
    type Family<'a> = (Option<&'a str>, Option<&'a str>, Vec<&'a str>);
    let mut families: BTreeMap<&str, Family<'_>> = BTreeMap::new();
    for line in text.lines().filter(|line| line.starts_with('#')) {
        let mut parts = line.split_whitespace();
        let (_, kind, name) = (parts.next(), parts.next().unwrap(), parts.next().unwrap());
        let family = families.entry(name).or_default();
        if kind == "HELP" {
            family.0 = Some(line);
        } else {
            family.1 = Some(line);
        }
    }
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let name = line.split(['{', ' ']).next().unwrap();
        let family = if families.contains_key(name) {
            name
        } else {
            ["_bucket", "_count", "_sum"]
                .iter()
                .find_map(|suffix| {
                    name.strip_suffix(suffix)
                        .filter(|base| families.contains_key(base))
                })
                .expect("declared metric family")
        };
        families.get_mut(family).unwrap().2.push(line);
    }
    let mut output = String::with_capacity(text.len());
    for (_, (help, kind, samples)) in families {
        for line in help.into_iter().chain(kind).chain(samples) {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
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
    #[test]
    fn cached_scrape_preserves_sample_on_failure_and_declares_families_once() {
        let path = std::env::temp_dir().join(format!(
            "rogue-metrics-{}-{}.redb",
            std::process::id(),
            now()
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
            samples: RwLock::new(vec![Sample::default(), Sample::default()]),
            node: RwLock::new(NodeSample::default()),
            started: Instant::now(),
            role: "producer",
            activity: Activity::default(),
        };
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
    #[test]
    fn attempts_distinguish_contention_failure_and_production() {
        let activity = Activity::default();
        for result in [
            Ok(true),
            Ok(false),
            Err(Error("ServerBusy")),
            Err(Error("StorageUnavailable")),
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
