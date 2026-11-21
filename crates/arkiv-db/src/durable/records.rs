//! Expiry of per-height records (simulator profile).
//!
//! Every height `h` of the simulator profile leaves records beside its
//! state: the node record (`Blocks[h]`: certificate, body reference and
//! feed), the manifest (`Manifests[h]`), the manifest index entry
//! (`BlockIndex[id(h)]`), the request inclusions of the block
//! (`Inclusions[identity]`), and three metadata records,
//! `engine/state-floor/<h>`, `engine/record-floor/<h>` and
//! `engine/inclusions-at/<h>` (the identities the block included, so that
//! they can be found again by height).
//!
//! With [`Options::prune_records`](super::Options) and a retention window,
//! these records expire with the state they belong to. The **record floor**
//! after the commit of `S` is
//!
//! ```text
//! R(S) = max(R(S-1), min(stateFloor(S), S + 1 - inclusionWindow))
//! ```
//!
//! so it never passes the state floor (a feed record never outlives the
//! state its receipts are rebuilt from by more than the state window) and
//! it keeps the inclusions the next block's identity check needs (see
//! [`Options::inclusion_window`](super::Options)). Height zero is the
//! anchor and never expires: opening reads manifest 0, and followers read
//! the genesis record.
//!
//! - **Logical expiry.** A record of a height `0 < h < R` is answered as
//!   gone ([`EngineError::HistoryPruned`](crate::EngineError) for a block,
//!   absent for an inclusion) from the commit that raises `R`, whether or
//!   not its bytes are still stored.
//! - **Physical deletion** happens inside block commits, in the same
//!   atomic batch: each commit deletes the records of at least
//!   `record_delete_budget + 1` heights, and at least a sixteenth of the
//!   backlog, from the deletion cursor up to `R` ([`deletion_count`]). The
//!   budget is a floor: the backlog shrinks while `R` advances one height
//!   per block, and a migrated file drains geometrically.
//! - **Durability.** Each commit with a non-zero record floor or cursor
//!   writes `engine/record-floor/<S> = R ‖ cursor` (16 bytes) in its batch;
//!   opening reads the head's record only. A crash anywhere leaves either
//!   the old head with its records or the new head with its deletions.
//! - **Manifest index.** Deleting height `h` removes the index entry of
//!   `h - 1`, not of `h`: reading the lowest retained manifest checks its
//!   parent's entry.
//! - **Inclusions.** A stored identity may be included again once its
//!   inclusion is older than the inclusion window: the new block's batch
//!   replaces the row. Deleting height `h` therefore removes only the rows
//!   whose stored height is still `h`. A height without an inclusion list
//!   (written by a binary that predates it) leaves its inclusion rows on
//!   disk, answered as absent; `arkiv-proposer state migrate-records`
//!   writes the missing lists.

use super::*;
use crate::{DecodeError, Decoder};

/// Heights one commit deletes from a backlog of `backlog`: at least
/// `budget` and a sixteenth of the backlog, plus one, so the backlog
/// shrinks even while the floor rises one height a block; never more than
/// the backlog.
pub fn deletion_count(backlog: u64, budget: u64) -> u64 {
    backlog.min(budget.max(backlog / 16).saturating_add(1))
}

/// Metadata name of the record floor written by the commit of `sequence`.
pub(crate) fn record_floor_key(sequence: u64) -> String {
    format!("engine/record-floor/{sequence:016x}")
}

/// Metadata name of the identities the block at `height` included.
pub fn inclusion_list_key(height: u64) -> String {
    format!("engine/inclusions-at/{height:016x}")
}

/// The record floor and deletion cursor of one durable head.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordFloor {
    /// Records of heights `1..floor` are expired. Zero (or one): none.
    pub floor: u64,
    /// Records of heights `1..pruned` are deleted from the store; heights
    /// `pruned..floor` are the deletion backlog.
    pub pruned: u64,
}

impl RecordFloor {
    /// Whether the records of `height` are expired.
    pub fn expired(&self, height: u64) -> bool {
        height > 0 && height < self.floor
    }
    /// Heights whose records are expired but still stored.
    pub fn backlog(&self) -> u64 {
        self.floor.saturating_sub(self.pruned.max(1))
    }
    fn encode(&self) -> Vec<u8> {
        [self.floor.to_be_bytes(), self.pruned.to_be_bytes()].concat()
    }
}

/// The record floor recorded with the commit of `sequence`.
pub(crate) fn read_record_floor(
    store: &dyn VersionedStore,
    sequence: u64,
) -> Result<RecordFloor, DurableError> {
    let Some(bytes) =
        store.read_record_bounded(Table::Meta, record_floor_key(sequence).as_bytes(), 16)?
    else {
        return Ok(RecordFloor::default());
    };
    if bytes.len() != 16 {
        return Err(corrupt("malformed record floor"));
    }
    let floor = u64::from_be_bytes(bytes[..8].try_into().expect("8 bytes"));
    let pruned = u64::from_be_bytes(bytes[8..].try_into().expect("8 bytes"));
    if floor > sequence || pruned > floor.max(1) {
        return Err(corrupt("record floor outside the published range"));
    }
    Ok(RecordFloor { floor, pruned })
}

/// Encodes the identities a block included, for its inclusion list.
pub fn encode_inclusion_list<'a>(keys: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for key in keys {
        out.push(key.len() as u8);
        out.extend_from_slice(key);
    }
    out
}

fn decode_inclusion_list(bytes: &[u8]) -> Result<Vec<&[u8]>, DurableError> {
    let mut keys = Vec::new();
    let mut rest = bytes;
    while let Some((&len, tail)) = rest.split_first() {
        if len == 0 || tail.len() < len as usize {
            return Err(corrupt("malformed inclusion list"));
        }
        let (key, tail) = tail.split_at(len as usize);
        keys.push(key);
        rest = tail;
    }
    Ok(keys)
}

/// The height a stored simulator inclusion row names (its header only; the
/// digest is checked by full reads).
pub fn inclusion_height(bytes: &[u8]) -> Result<u64, DurableError> {
    let parse = || -> Result<u64, DecodeError> {
        let mut d = Decoder::new(bytes);
        if d.array::<8>()? != *b"ARKSIMR1" {
            return Err(DecodeError::Invalid {
                kind: "SimulatorInclusion",
                reason: "magic",
            });
        }
        d.u64()
    };
    parse().map_err(|e| corrupt(format!("malformed simulator inclusion: {e}")))
}

/// Largest inclusion list: [`simulator::MAX_INCLUSIONS`] keys of at most 61
/// bytes, each with its length byte.
pub const INCLUSION_LIST_MAX: usize = simulator::MAX_INCLUSIONS * 62;
/// Largest stored inclusion row.
const INCLUSION_ROW_MAX: usize = 8 + 8 + 32 + 1 + 61 + 4 + simulator::INCLUSION_MAX_BYTES + 32;

impl DurableEngine {
    /// The record floor of the durable head (see the module docs).
    pub fn record_floor(&self) -> RecordFloor {
        self.records
    }

    /// Fails with `HistoryPruned` if the records of `height` are expired.
    pub(crate) fn check_record(&self, height: u64) -> Result<(), DurableError> {
        if self.records.expired(height) {
            return Err(EngineError::HistoryPruned {
                requested: SnapshotId::from_sequence(height),
                floor: SnapshotId::from_sequence(self.records.floor),
            }
            .into());
        }
        Ok(())
    }

    /// The record floor of the commit of `sequence` whose state floor is
    /// `state_floor`, and the record changes it implies: the inclusion
    /// rows the packet replaces, and the deletion of expired heights.
    pub(crate) fn expire_records(
        &self,
        batch: &mut CommitBatch,
        sequence: u64,
        state_floor: u64,
        packet: Option<&simulator::SimulatorPacket>,
    ) -> Result<RecordFloor, DurableError> {
        let store = self.context.store.as_ref();
        let mut next = self.records;
        if let (Some(packet), Some(window)) = (packet, self.options.inclusion_window) {
            for item in &packet.inclusions {
                if let Some(row) =
                    store.read_record_bounded(Table::Inclusions, &item.key, INCLUSION_ROW_MAX)?
                {
                    // An inclusion older than the identity window may be
                    // repeated; a younger one stays and the insertion
                    // below conflicts.
                    if inclusion_height(&row)?.saturating_add(window) < sequence {
                        batch.remove_record(Table::Inclusions, item.key.clone());
                    }
                }
            }
            if !packet.inclusions.is_empty() {
                batch.put_record(
                    Table::Meta,
                    inclusion_list_key(sequence).into_bytes(),
                    encode_inclusion_list(packet.inclusions.iter().map(|i| i.key.as_slice())),
                );
            }
        }
        if self.simulator.is_some()
            && self.options.prune_records
            && self.options.retention.window().is_some()
        {
            let keep = sequence
                .saturating_add(1)
                .saturating_sub(self.options.inclusion_window.unwrap_or(0));
            next.floor = next.floor.max(state_floor.min(keep));
        }
        if next.floor <= 1 && next.pruned == 0 {
            return Ok(next);
        }
        let start = next.pruned.max(1);
        let backlog = next.floor.saturating_sub(start);
        let count = deletion_count(backlog, self.options.record_delete_budget);
        for height in start..start + count {
            self.delete_height(batch, height)?;
        }
        next.pruned = start + count;
        batch.put_record(
            Table::Meta,
            record_floor_key(sequence).into_bytes(),
            next.encode(),
        );
        Ok(next)
    }

    /// Adds the removal of every record of `height` to `batch`.
    fn delete_height(&self, batch: &mut CommitBatch, height: u64) -> Result<(), DurableError> {
        let store = self.context.store.as_ref();
        let key = height.to_be_bytes();
        if let Some(bytes) =
            store.read_record_bounded(Table::Manifests, &key, format::MANIFEST_MAX_BYTES)?
        {
            let manifest = EngineManifest::from_envelope(&bytes)?;
            if let Some((parent, id)) = manifest.parent
                && parent > 0
            {
                batch.remove_record(Table::BlockIndex, id.0.to_vec());
            }
        }
        let list = inclusion_list_key(height);
        if let Some(bytes) =
            store.read_record_bounded(Table::Meta, list.as_bytes(), INCLUSION_LIST_MAX)?
        {
            for identity in decode_inclusion_list(&bytes)? {
                if let Some(row) =
                    store.read_record_bounded(Table::Inclusions, identity, INCLUSION_ROW_MAX)?
                    && inclusion_height(&row)? == height
                {
                    batch.remove_record(Table::Inclusions, identity.to_vec());
                }
            }
            batch.remove_record(Table::Meta, list.into_bytes());
        }
        batch
            .remove_record(Table::Blocks, key.to_vec())
            .remove_record(Table::Manifests, key.to_vec())
            .remove_record(Table::Meta, retention::floor_key(height).into_bytes())
            .remove_record(Table::Meta, record_floor_key(height).into_bytes());
        Ok(())
    }
}
