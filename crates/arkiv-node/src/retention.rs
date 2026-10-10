//! Node-side state-retention checks shared by the signed and simulator hosts.
//!
//! The durable engine refuses snapshots below its state floor
//! (`arkiv_db::durable::Retention`). Node read paths check the floor up front,
//! before opening a snapshot, and record the refusal so the API layer can
//! render `requested`/`floor` details:
//!
//! - state at height `h` (balances, nonces, proofs, queries, the state tree
//!   explorer) needs `h >= stateFloor`;
//! - receipts, logs and the `logsBloom` of block `h > 0` are derived from body
//!   `h` and state `h - 1`, so they need `h >= receiptFloor`, where
//!   `receiptFloor = stateFloor + 1` when `stateFloor > 0` and `0` otherwise.
//!   Block 0 has no logs. A receipt refusal reports the block height and the
//!   receipt floor.
//!
//! Headers, bodies, feed records, inclusions and the hash index are retained
//! in full and never refused.
use crate::simulator::{Error, Result};
use arkiv_db::{
    EngineError, Snapshot, SnapshotId,
    durable::{DurableEngine, DurableError},
};
use serde_json::{Value as Json, json};
use std::cell::Cell;

/// The most recent refusal `(requested, floor)` of one host.
#[derive(Debug, Default)]
pub struct Refusal(Cell<Option<(u64, u64)>>);

impl Refusal {
    pub fn record(&self, requested: u64, floor: u64) -> Error {
        self.0.set(Some((requested, floor)));
        Error("HistoryPruned")
    }
    /// Takes the details of the last refusal, if one is pending.
    pub fn take(&self) -> Option<(u64, u64)> {
        self.0.take()
    }
}

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
pub fn require_state(engine: &DurableEngine, refusal: &Refusal, height: u64) -> Result<()> {
    let floor = state_floor(engine);
    if height < floor {
        return Err(refusal.record(height, floor));
    }
    Ok(())
}

/// Refuses receipts/logs of block `height` below the receipt floor.
pub fn require_receipts(engine: &DurableEngine, refusal: &Refusal, height: u64) -> Result<()> {
    let floor = receipt_floor(engine);
    if height > 0 && height < floor {
        return Err(refusal.record(height, floor));
    }
    Ok(())
}

/// Opens the snapshot at `height` after the up-front floor check; an engine
/// refusal (the backstop) is recorded too.
pub fn state(engine: &DurableEngine, refusal: &Refusal, height: u64) -> Result<Snapshot> {
    require_state(engine, refusal, height)?;
    engine
        .snapshot(SnapshotId::from_sequence(height))
        .map_err(|e| match e {
            DurableError::Engine(EngineError::HistoryPruned { requested, floor }) => {
                refusal.record(requested.sequence(), floor.sequence())
            }
            other => other.into(),
        })
}

/// Status fields describing the served state history.
pub fn status(engine: &DurableEngine) -> Json {
    let retention = engine.retention_status().retention;
    json!({
        "stateRetention": retention.mode(),
        "stateRetentionWindow": retention.window().map(|w| w.to_string()),
        "stateFloor": state_floor(engine).to_string(),
        "receiptFloor": receipt_floor(engine).to_string(),
    })
}

/// The REST error body: `{"error":"HistoryPruned","requestedHeight":"N","stateFloor":"F"}`
/// (details when known), or `{"error":code}`.
pub fn error_body(code: &str, refusal: Option<(u64, u64)>) -> Json {
    match (code, refusal) {
        ("HistoryPruned", Some((requested, floor))) => json!({
            "error": code,
            "requestedHeight": requested.to_string(),
            "stateFloor": floor.to_string(),
        }),
        _ => json!({ "error": code }),
    }
}
