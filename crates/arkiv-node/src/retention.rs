//! Node-side state-retention checks shared by the signed and simulator hosts.
//!
//! The durable engine refuses snapshots below its state floor
//! (`arkiv_db::durable::Retention`). Node read paths check the floor up front,
//! before opening a snapshot; the refusal names the requested height and the
//! floor:
//!
//! - state at height `h` (balances, nonces, proofs, queries, the state tree
//!   explorer) needs `h >= stateFloor`;
//! - receipts, logs and the `logsBloom` of block `h > 0` are derived from body
//!   `h` and state `h - 1`, so they need `h >= receiptFloor`, where
//!   `receiptFloor = stateFloor + 1` when `stateFloor > 0` and `0` otherwise.
//!   Block 0 has no logs. A receipt refusal reports the block height and the
//!   receipt floor.
//!
//! - per-height records (the feed, the block and its body reference, the
//!   request inclusions, the transaction and block hash index) of a node
//!   with a window expire with the state: they need `h >= recordFloor` (or
//!   `h = 0`, the anchor that never expires), where `recordFloor <=
//!   stateFloor` (see `arkiv_db::durable::records`). A refusal reports the
//!   height and the record floor. An archive node keeps them all.
//!
//! Block bodies in `history.redb` are a separate choice (`--body-retention`):
//! kept by default, so followers and light clients can still start from
//! genesis, or expired with the records.
//!
//! Garbage collection (`arkiv_db::durable::gc`) is a property of the state
//! file: [`collect_new_state`] turns it on when a node with a retention
//! window creates or adopts its state, and it then stays on. An existing
//! file without it keeps every object until `arkiv-proposer state
//! gc-enable` counts it offline.

use crate::error::{Error, Result};
use arkiv_db::{Snapshot, SnapshotId, durable::DurableEngine};
use serde_json::{Value as Json, json};
use std::path::Path;

/// The lowest height whose state is served.
pub fn state_floor(engine: &DurableEngine) -> u64 {
    engine.state_floor().sequence()
}

/// The lowest height whose receipts/logs are served (see module docs).
pub fn receipt_floor(engine: &DurableEngine) -> u64 {
    match state_floor(engine) {
        0 => 0,
        floor => floor + 1,
    }
}

/// Refuses state at `height` below the floor. Heights above the head are
/// left to the caller's own unknown-block handling.
pub fn require_state(engine: &DurableEngine, height: u64) -> Result<()> {
    let floor = state_floor(engine);
    if height < floor {
        return Err(Error::pruned(height, floor));
    }
    Ok(())
}

/// Refuses receipts/logs of block `height` below the receipt floor.
pub fn require_receipts(engine: &DurableEngine, height: u64) -> Result<()> {
    let floor = receipt_floor(engine);
    if height > 0 && height < floor {
        return Err(Error::pruned(height, floor));
    }
    Ok(())
}

/// Opens the snapshot at `height` after the up-front floor check; the
/// engine's own refusal is the backstop.
pub fn state(engine: &DurableEngine, height: u64) -> Result<Snapshot> {
    require_state(engine, height)?;
    Ok(engine.snapshot(SnapshotId::from_sequence(height))?)
}

/// Status fields describing the served state history. `stateGc` is `on`
/// when the state file deletes what no retained snapshot reaches,
/// `suspended` when its counts failed a check and need a rebuild, `off`
/// when it keeps every object.
pub fn status(engine: &DurableEngine) -> Json {
    let retention = engine.retention_status().retention;
    json!({
        "stateRetention": retention.mode(),
        "stateRetentionWindow": retention.window().map(|w| w.to_string()),
        "stateFloor": state_floor(engine).to_string(),
        "receiptFloor": receipt_floor(engine).to_string(),
        "recordFloor": engine.record_floor().floor.to_string(),
        "stateGc": gc_mode(engine),
    })
}

/// `on`, `suspended` or `off` (see [`status`]).
pub fn gc_mode(engine: &DurableEngine) -> &'static str {
    match engine.gc_state() {
        Ok(Some(state)) if state.suspended => "suspended",
        Ok(Some(_)) => "on",
        _ => "off",
    }
}

/// Turns garbage collection on for a state a node with a retention window
/// has just created or adopted (still at genesis): one scan of its
/// objects, spilling to `run_dir/gc-seed` (removed afterwards). Archive
/// nodes and files that already collect are left alone. A file that is
/// past genesis without garbage collection is only reported: counting it
/// is an offline step (`arkiv-proposer state gc-enable`), not something a
/// restart should start unasked.
pub fn collect_new_state(engine: &mut DurableEngine, run_dir: &Path) -> Result<()> {
    if engine.retention_status().retention.window().is_none() || engine.collects_garbage() {
        return Ok(());
    }
    if engine.head().sequence() > 0 {
        eprintln!(
            "{}",
            json!({
                "event": "state_gc_off",
                "reason": "the state file predates garbage collection and keeps every object; \
                           stop the node and run `arkiv-proposer state gc-enable --run-dir DIR`",
            })
        );
        return Ok(());
    }
    let spill = run_dir.join("gc-seed");
    let _ = std::fs::remove_dir_all(&spill);
    let started = std::time::Instant::now();
    let state = engine.enable_gc(Some(&spill))?;
    let _ = std::fs::remove_dir_all(&spill);
    eprintln!(
        "{}",
        json!({
            "event": "state_gc_enabled",
            "storedObjects": state.stored_objects,
            "storedBytes": state.stored_bytes,
            "sharedRows": state.shared,
            "seconds": started.elapsed().as_secs_f64(),
        })
    );
    Ok(())
}

/// The REST error body: `{"error":"HistoryPruned","requestedHeight":"N","stateFloor":"F"}`
/// (details when known), or `{"error":code}`.
pub fn error_body(e: &Error) -> Json {
    match e.pruned_at() {
        Some((requested, floor)) => json!({
            "error": e.code.as_str(),
            "requestedHeight": requested.to_string(),
            "stateFloor": floor.to_string(),
        }),
        None => json!({ "error": e.code.as_str() }),
    }
}
