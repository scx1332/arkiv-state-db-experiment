//! Namespace isolation measurements (gate 2).
//!
//! Namespace A (`Namespace(1)`) is written once and then frozen. Between
//! phases, namespace B (`Namespace(2)`) grows, the number of namespaces
//! grows, and the history grows (more blocks). In every phase the store is
//! closed and reopened, and A's whole object set is read in a fixed order:
//!
//! - **uncached**: a fresh open with a zero-byte redb cache, so every page
//!   of every read comes from the backend;
//! - **cold / warm**: a fresh open with a small cache, reading A twice.
//!
//! The application-level reads (the store's per-space counters) and the
//! backend reads are recorded per phase together with the physical layout
//! (redb's table-directory height and A's table height).

use std::path::Path;

use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{
    BackendStats, CommitBatch, Head, ObjectId, ObjectReader, SpaceId, SpaceStats, Table,
    VersionedStore, height_key,
};

use super::{BenchDir, block, commit, object};

/// Namespace A.
pub const A: SpaceId = SpaceId::Namespace(1);
/// Namespace B.
pub const B: SpaceId = SpaceId::Namespace(2);

/// Sizes of one isolation run.
#[derive(Clone, Debug)]
pub struct Params {
    /// Objects in A.
    pub a_objects: u64,
    /// Bytes per object.
    pub object_len: usize,
    /// B's object count as multiples of A's, in phase order (the first is
    /// the baseline).
    pub b_factors: Vec<u64>,
    /// Total namespace counts to grow to after B's growth, in order.
    pub namespace_counts: Vec<u64>,
    /// Blocks of history added in the last phase.
    pub history_blocks: u64,
    /// Objects per committed block while growing.
    pub objects_per_block: u64,
    /// redb cache for the cold/warm passes.
    pub small_cache: usize,
}

/// Measurements of one phase.
#[derive(Clone, Debug)]
pub struct Phase {
    /// What grew.
    pub name: String,
    /// B's objects.
    pub b_objects: u64,
    /// Namespaces holding objects.
    pub namespaces: u64,
    /// Committed blocks.
    pub blocks: u64,
    /// File size.
    pub file_bytes: u64,
    /// redb's table-directory height.
    pub directory_height: u32,
    /// Tables in the file.
    pub tables: u64,
    /// A's table height.
    pub a_height: u32,
    /// Application-level reads per space during the uncached pass.
    pub app: Vec<(SpaceId, SpaceStats)>,
    /// Backend I/O of the uncached pass.
    pub uncached: BackendStats,
    /// Backend I/O of the first pass with the small cache.
    pub cold: BackendStats,
    /// Backend I/O of the second pass with the small cache.
    pub warm: BackendStats,
    /// Object reads per pass.
    pub reads: u64,
}

impl Phase {
    /// The page-read bound for the uncached pass derived from B-tree depth:
    /// each object read descends the table directory and A's table once.
    pub fn uncached_bound(&self) -> u64 {
        self.reads * u64::from(self.directory_height + self.a_height)
    }
}

struct Chain {
    parent: Option<Head>,
    height: u64,
}

impl Chain {
    fn commit(&mut self, store: &RedbStore, mut batch: CommitBatch) {
        batch.expected_head = self.parent;
        batch.new_head = super::head(self.height, 1);
        // Keep the records consistent with the (re-numbered) head.
        batch.records.retain(|(table, _, _)| *table == Table::Meta);
        let hash = batch.new_head.hash;
        batch
            .put_record(
                Table::Blocks,
                height_key(self.height),
                super::block_bytes(self.height, 1, 300),
            )
            .put_record(Table::BlockIndex, hash, height_key(self.height));
        self.parent = Some(batch.new_head);
        self.height += 1;
        commit(store, batch);
    }
}

fn a_objects(params: &Params) -> Vec<(ObjectId, Vec<u8>)> {
    (0..params.a_objects)
        .map(|n| object(0xa, n, params.object_len))
        .collect()
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

fn read_a(store: &RedbStore, objects: &[(ObjectId, Vec<u8>)]) {
    for (id, bytes) in objects {
        assert_eq!(
            store.read_object(A, id).expect("read A").as_ref(),
            Some(bytes),
            "A's object changed"
        );
    }
}

fn measure(
    path: &Path,
    params: &Params,
    name: &str,
    chain: &Chain,
    b_objects: u64,
    namespaces: u64,
) -> Phase {
    let objects = a_objects(params);
    let (directory_height, tables, a_height) = {
        let store = RedbStore::open(path, RedbConfig::default()).expect("open for layout");
        let layout = store.layout(&[A]).expect("layout");
        (
            layout.directory_height,
            layout.tables,
            layout.spaces[&A].height,
        )
    };

    let store = RedbStore::open(path, RedbConfig::with_cache_size(0)).expect("uncached open");
    let before = store.stats();
    read_a(&store, &objects);
    let delta = store.stats().since(&before);
    drop(store);

    let store = RedbStore::open(path, RedbConfig::with_cache_size(params.small_cache))
        .expect("small-cache open");
    let start = store.stats().backend.expect("backend");
    read_a(&store, &objects);
    let mid = store.stats().backend.expect("backend");
    read_a(&store, &objects);
    let end = store.stats().backend.expect("backend");
    drop(store);

    Phase {
        name: name.to_owned(),
        b_objects,
        namespaces,
        blocks: chain.height,
        file_bytes: file_len(path),
        directory_height,
        tables,
        a_height,
        app: delta.spaces.into_iter().collect(),
        uncached: delta.backend.expect("backend"),
        cold: mid.since(&start),
        warm: end.since(&mid),
        reads: objects.len() as u64,
    }
}

/// Runs every phase and returns their measurements, baseline first.
pub fn run(dir: &BenchDir, params: &Params) -> Vec<Phase> {
    let path = dir.file("isolation.redb");
    let mut chain = Chain {
        parent: None,
        height: 0,
    };
    let mut phases = Vec::new();

    // Baseline: A, and B at the first factor, interleaved in the same blocks.
    let a = a_objects(params);
    let store = RedbStore::create(&path, RedbConfig::default()).expect("create");
    let mut b_objects = 0;
    let first_b = params.b_factors[0] * params.a_objects;
    for chunk in a.chunks(params.objects_per_block as usize) {
        let mut batch = CommitBatch::default();
        for (id, bytes) in chunk {
            batch.put_object(A, *id, bytes.clone());
        }
        let b_now = (chunk.len() as u64 * params.b_factors[0]).min(first_b - b_objects);
        for n in b_objects..b_objects + b_now {
            let (id, bytes) = object(0xb, n, params.object_len);
            batch.put_object(B, id, bytes);
        }
        b_objects += b_now;
        chain.commit(&store, batch);
    }
    drop(store);
    let mut namespaces = 2;
    phases.push(measure(
        &path, params, "baseline", &chain, b_objects, namespaces,
    ));

    // B grows.
    for factor in &params.b_factors[1..] {
        let store = RedbStore::open(&path, RedbConfig::default()).expect("open");
        let target = factor * params.a_objects;
        while b_objects < target {
            let mut batch = CommitBatch::default();
            let end = (b_objects + params.objects_per_block).min(target);
            for n in b_objects..end {
                let (id, bytes) = object(0xb, n, params.object_len);
                batch.put_object(B, id, bytes);
            }
            b_objects = end;
            chain.commit(&store, batch);
        }
        drop(store);
        phases.push(measure(
            &path,
            params,
            &format!("B x{factor}"),
            &chain,
            b_objects,
            namespaces,
        ));
    }

    // The namespace count grows: each new namespace gets two objects.
    for count in &params.namespace_counts {
        let store = RedbStore::open(&path, RedbConfig::default()).expect("open");
        while namespaces < *count {
            let mut batch = CommitBatch::default();
            let end = (namespaces + params.objects_per_block / 2).min(*count);
            for ns in namespaces..end {
                let space = SpaceId::Namespace(ns + 1);
                for n in 0..2 {
                    let (id, bytes) = object(0xc0_0000 + ns, n, params.object_len);
                    batch.put_object(space, id, bytes);
                }
            }
            namespaces = end;
            chain.commit(&store, batch);
        }
        drop(store);
        phases.push(measure(
            &path,
            params,
            &format!("{count} namespaces"),
            &chain,
            b_objects,
            namespaces,
        ));
    }

    // History grows: many small blocks touching B, the catalog and records.
    if params.history_blocks > 0 {
        let store = RedbStore::open(&path, RedbConfig::default()).expect("open");
        for _ in 0..params.history_blocks {
            let height = chain.height;
            let mut batch = block(
                None,
                height,
                1,
                &[(B, 4, params.object_len), (SpaceId::Catalog, 1, 120)],
            );
            batch
                .records
                .retain(|(table, _, _)| *table == Table::Manifests || *table == Table::Inclusions);
            b_objects += 4;
            chain.commit(&store, batch);
        }
        drop(store);
        phases.push(measure(
            &path,
            params,
            &format!("+{} blocks", params.history_blocks),
            &chain,
            b_objects,
            namespaces,
        ));
    }
    phases
}
