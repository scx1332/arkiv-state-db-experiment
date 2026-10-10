//! Read-only reachability report for a durable redb file: per object space,
//! what is stored, what the head reaches, and what the union of the newest
//! N snapshots reaches. The difference is what pruning to that window could
//! reclaim. Nothing is written or deleted.
//!
//! ```sh
//! # An existing engine, simulator or signed-node file (work on a copy:
//! # opening a redb file may repair it):
//! cargo run --release --offline -p arkiv-db-redb --example reachability_report -- \
//!     path/to/node.redb --recent 1,16,128,1024
//! # A small churn workload created in a temporary directory, then reported:
//! cargo run --release --offline -p arkiv-db-redb --example reachability_report -- \
//!     --demo 2000 --recent 1,16,128,1024
//! ```
//!
//! "objects/bytes" are distinct `(space, id)` objects and their encoded value
//! bytes. Stored "bytes" are redb's key+value bytes of the space's table, so
//! the reachable figure adds the 32-byte key per object for comparison.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/engine_support/mod.rs"]
mod engine_support;

use arkiv_db::{
    Cell, Value, WriteOp,
    durable::reachability::{Reach, reachability},
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::SpaceId;
use engine_support::*;
use std::{path::PathBuf, sync::Arc, time::Instant};

fn space_name(space: SpaceId) -> String {
    match space {
        SpaceId::Catalog => "catalog".into(),
        SpaceId::Host => "host".into(),
        SpaceId::Namespace(n) => format!("ns {n}"),
    }
}

/// `publications` blocks of churn: one of 64 raw keys rewritten, one typed
/// row created and (after 32 blocks) the row from 32 blocks earlier deleted,
/// one host key rewritten. Live state stays roughly constant; history grows.
fn demo(path: &std::path::Path, publications: u64) {
    let store = Arc::new(RedbStore::create(path, RedbConfig::with_cache_size(8 << 20)).unwrap());
    let mut engine = initialize(store, 0);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    base(&mut branch);
    publish(&mut engine, branch);
    for n in 0..publications {
        let mut branch = engine.begin(engine.head(), None).unwrap();
        put(
            &mut branch,
            ns(1),
            &(n % 64).to_be_bytes(),
            &n.to_be_bytes(),
        );
        execute(
            &mut branch,
            WriteOp::CreateRecord {
                namespace: ns(2),
                key: key(1_000 + n),
                cells: vec![
                    (name("kind"), Cell::attribute(Value::U64(n % 8))),
                    (name("payload"), Cell::field(Value::bytes(&[n as u8; 64]))),
                ],
            },
        );
        if n >= 32 {
            execute(
                &mut branch,
                WriteOp::DeleteRecord {
                    namespace: ns(2),
                    key: key(1_000 + n - 32),
                },
            );
        }
        branch
            .try_host_put([0x48; 32], &n.to_be_bytes())
            .unwrap()
            .unwrap();
        publish(&mut engine, branch);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut demo_blocks = None;
    let mut recent = vec![1, 16, 128, 1024];
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--demo" => demo_blocks = Some(args.next().expect("--demo N").parse().unwrap()),
            "--recent" => {
                recent = args
                    .next()
                    .expect("--recent N[,N...]")
                    .split(',')
                    .map(|n| n.parse().expect("snapshot count"))
                    .collect()
            }
            other => path = Some(PathBuf::from(other)),
        }
    }
    let _dir;
    let path = match (path, demo_blocks) {
        (Some(path), None) => path,
        (None, Some(blocks)) => {
            _dir = common::BenchDir::new("reachability");
            let path = _dir.file("demo.redb");
            let start = Instant::now();
            demo(&path, blocks);
            eprintln!("demo: {blocks} publications in {:?}", start.elapsed());
            path
        }
        _ => {
            eprintln!("usage: reachability_report (<file.redb> | --demo N) [--recent N,N,...]");
            std::process::exit(2);
        }
    };
    let store = RedbStore::open(&path, RedbConfig::with_cache_size(64 << 20)).unwrap();
    let start = Instant::now();
    let report = reachability(&store, &recent).unwrap();
    let elapsed = start.elapsed();
    let layout = store.layout(&report.spaces()).unwrap();
    let file = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "# Reachability report\n\n- File: `{}` ({:.1} MiB)\n- Head: {}; durable state floor: {}; walk took {elapsed:?}\n- Stored = redb table entries and key+value bytes; reachable = distinct objects (and encoded bytes + 32-byte keys) reachable from the newest N snapshots.\n",
        path.display(),
        file as f64 / (1 << 20) as f64,
        report.head,
        report.floor,
    );
    let mut header = "| space | stored objects | stored bytes |".to_owned();
    let mut rule = "| --- | ---: | ---: |".to_owned();
    for level in &report.levels {
        header.push_str(&format!(
            " reach N={} (from {}) | % objs |",
            level.recent, level.oldest
        ));
        rule.push_str(" ---: | ---: |");
    }
    println!("{header}\n{rule}");
    let mut totals = (0u64, 0u64, vec![Reach::default(); report.levels.len()]);
    for space in report.spaces() {
        let Some(table) = layout.spaces.get(&space) else {
            continue;
        };
        totals.0 += table.entries;
        totals.1 += table.stored_bytes;
        let mut row = format!(
            "| {} | {} | {} |",
            space_name(space),
            table.entries,
            table.stored_bytes
        );
        for (i, level) in report.levels.iter().enumerate() {
            let reach = level.per_space.get(&space).copied().unwrap_or_default();
            totals.2[i].objects += reach.objects;
            totals.2[i].bytes += reach.bytes + 32 * reach.objects;
            row.push_str(&format!(
                " {} / {} B | {:.1} |",
                reach.objects,
                reach.bytes + 32 * reach.objects,
                percent(reach.objects, table.entries)
            ));
        }
        println!("{row}");
    }
    let mut row = format!("| **all** | {} | {} |", totals.0, totals.1);
    for reach in &totals.2 {
        row.push_str(&format!(
            " {} / {} B | {:.1} |",
            reach.objects,
            reach.bytes,
            percent(reach.objects, totals.0)
        ));
    }
    println!("{row}");
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}
