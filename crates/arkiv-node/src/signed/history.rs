//! Separate, content-addressed retained bodies. Presence is never inclusion.
//!
//! This store has its own profile and storage sequence, independent of chain
//! height. Callers publish engine references only after `persist` succeeds.
//!
//! Each block body persisted with [`History::persist_block`] also gets a
//! height index row (`Blocks[height] = body reference ‖ entry sequence ‖
//! certificate`), so a block can be served by height after the node's own
//! per-height records expired, and so bodies can be pruned by height
//! ([`History::prune_below`]). A prune is its own history entry (tag 4).

use super::{ErrorCode, Result, protocol::SignedHeader};
use alloy_primitives::keccak256;
use arkiv_db::{B256, Decode, DecodeError, Decoder, Encode, Encoder, decode_exact, digest, encode};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Head, SpaceId, Table, VersionedStore,
};
use std::sync::Arc;

pub const BODY_CAP: usize = 1024 * 1024;
const PROFILE: &[u8] = b"ARKSIGNEDHISTORY1";
const FRAME: &[u8] = b"ARKSIGNEDBODY1";
const FRAME_OVERHEAD: usize = FRAME.len() + 32 + 4;
const PROFILE_KEY: &[u8] = b"signed-history/profile";
const ID_KEY: &[u8] = b"signed-history/identity";
const CONFLICT_KEY: &[u8] = b"signed-history/conflict";
const CONFLICT_CAP: usize = 1024;
const CERTIFICATE_LEN: usize = 4 + 32 + 156 + 65;
const ENTRY_CAP: usize = 1 + 8 + 32;
const PRUNED_KEY: &[u8] = b"signed-history/pruned";
/// Present when every persisted block body has its height index row: in
/// stores created with the index, and after [`crate::signed::migrate`].
pub const INDEXED_KEY: &[u8] = b"signed-history/height-index";
const INDEX_CAP: usize = 36 + 8 + CERTIFICATE_LEN;

/// What the height index holds for one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedBlock {
    pub body: BodyRef,
    /// The history entry that persisted the body.
    pub entry: u64,
    pub certificate: SignedHeader,
}

/// How far [`History::prune_below`] has deleted: heights `1..height` and
/// entries `1..entry`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryPruned {
    pub height: u64,
    pub entry: u64,
}

/// Hash of the complete stored frame and length of its unframed body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyRef {
    pub id: B256,
    pub len: u32,
}

impl Encode for BodyRef {
    fn encode_to(&self, e: &mut Encoder) {
        e.put(&self.id);
        e.u32(self.len);
    }
}

impl Decode for BodyRef {
    const MIN_ENCODED_LEN: usize = 36;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let value = Self {
            id: d.get()?,
            len: d.u32()?,
        };
        if value.len as usize > BODY_CAP {
            return Err(DecodeError::OutOfRange {
                kind: "signed body length",
            });
        }
        Ok(value)
    }
}

struct Pending {
    reference: BodyRef,
    batch: CommitBatch,
}

pub struct History {
    store: Arc<dyn VersionedStore>,
    genesis_hash: B256,
    head: Head,
    pending: Option<Pending>,
    frozen: bool,
    observation_failed: bool,
}

impl History {
    pub fn create(store: Arc<dyn VersionedStore>, genesis_hash: B256) -> Result<Self> {
        let head = entry_head(genesis_hash, 0, &[0]);
        let mut batch = CommitBatch::new(None, head);
        let mut identity = genesis_hash.to_vec();
        identity.extend_from_slice(&store.database_id().0);
        batch
            .put_record(Table::Meta, PROFILE_KEY, PROFILE)
            .put_record(Table::Meta, ID_KEY, identity)
            .put_record(Table::Meta, INDEXED_KEY, vec![1])
            .put_record(Table::Manifests, 0u64.to_be_bytes(), vec![0]);
        match store.commit(&batch) {
            CommitOutcome::Committed => {}
            CommitOutcome::NotCommitted(_) => return Err(ErrorCode::StorageUnavailable.into()),
            CommitOutcome::Indeterminate(_) => return Err(ErrorCode::CommitUncertain.into()),
        }
        Ok(Self {
            store,
            genesis_hash,
            head,
            pending: None,
            frozen: false,
            observation_failed: false,
        })
    }

    /// Validates only bounded identity and head records, then the head body.
    /// Older retained bodies are checked lazily by `read`.
    pub fn open(store: Arc<dyn VersionedStore>, genesis_hash: B256) -> Result<Self> {
        check_identity(store.as_ref(), genesis_hash)?;
        if store
            .read_record_bounded(Table::Meta, CONFLICT_KEY, CONFLICT_CAP)?
            .is_some()
        {
            return Err(ErrorCode::ChainConflict.into());
        }
        let head = store.head()?.ok_or(ErrorCode::StorageCorrupt)?;
        let entry = store
            .read_record_bounded(Table::Manifests, &head.height.to_be_bytes(), ENTRY_CAP)?
            .ok_or(ErrorCode::StorageCorrupt)?;
        if entry_head(genesis_hash, head.height, &entry) != head {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        let history = Self {
            store,
            genesis_hash,
            head,
            pending: None,
            frozen: false,
            observation_failed: false,
        };
        match entry.as_slice() {
            [0] if head.height == 0 => {}
            [1, bytes @ ..] if head.height > 0 => {
                let reference: BodyRef = decode_exact(bytes).map_err(|e| {
                    ErrorCode::StorageCorrupt.caused_by(format_args!("history head: {e}"))
                })?;
                history.read(&reference)?;
            }
            [4, bytes @ ..] if head.height > 0 => {
                let _: (u64, u64) = decode_exact(bytes).map_err(|e| {
                    ErrorCode::StorageCorrupt.caused_by(format_args!("history head: {e}"))
                })?;
            }
            [3, bytes @ ..] if head.height > 0 => {
                let (height, hash): (u64, B256) = decode_exact(bytes).map_err(|e| {
                    ErrorCode::StorageCorrupt.caused_by(format_args!("history head: {e}"))
                })?;
                let certificate = history
                    .observation(height)?
                    .ok_or(ErrorCode::StorageCorrupt)?;
                if certificate.header.hash() != hash {
                    return Err(ErrorCode::StorageCorrupt.into());
                }
            }
            _ => return Err(ErrorCode::StorageCorrupt.into()),
        }
        Ok(history)
    }

    /// The reference [`Self::persist`] would return for `bytes`, without
    /// writing.
    pub fn reference(&self, bytes: &[u8]) -> Result<BodyRef> {
        self.ready()?;
        self.frame(bytes).map(|(reference, _)| reference)
    }

    /// Idempotently persists the exact framed bytes. Different candidates at
    /// the same chain height never compete for a storage key.
    pub fn persist(&mut self, bytes: &[u8]) -> Result<BodyRef> {
        self.persist_inner(bytes, None)
    }

    /// [`Self::persist`] for the body of the block `certificate` names,
    /// with its height index row. A later candidate at the same height
    /// replaces the row (the earlier one never published).
    pub fn persist_block(&mut self, certificate: &SignedHeader, bytes: &[u8]) -> Result<BodyRef> {
        // The row's layout is [`index_row`]'s.
        self.persist_inner(bytes, Some(certificate))
    }

    fn persist_inner(&mut self, bytes: &[u8], block: Option<&SignedHeader>) -> Result<BodyRef> {
        self.ready()?;
        let (reference, frame) = self.frame(bytes)?;
        if self.store.head()? != Some(self.head) {
            self.frozen = true;
            return Err(ErrorCode::StorageCorrupt.into());
        }
        if let Some(existing) =
            self.store
                .read_object_bounded(SpaceId::Host, &reference.id.0, frame.len())?
        {
            self.unframe(&reference, &existing)?;
            if existing != frame {
                return Err(ErrorCode::StorageCorrupt.into());
            }
            return Ok(reference);
        }
        let mut entry = vec![1];
        entry.extend_from_slice(&encode(&reference));
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(ErrorCode::CapacityReached)?;
        let mut batch = CommitBatch::new(
            Some(self.head),
            entry_head(self.genesis_hash, sequence, &entry),
        );
        batch
            .put_object(SpaceId::Host, reference.id.0, frame)
            .put_record(Table::Manifests, sequence.to_be_bytes(), entry);
        if let Some(certificate) = block {
            let height = certificate.header.height.to_be_bytes();
            let row = index_row(&reference, sequence, certificate);
            batch
                .remove_record(Table::Blocks, height.to_vec())
                .put_record(Table::Blocks, height.to_vec(), row);
        }
        self.pending = Some(Pending { reference, batch });
        self.commit_pending()
    }

    /// Explicitly reconciles and retries the retained batch, without accepting
    /// replacement bytes. Observed committed data after a failed sync remains
    /// durability-unknown; it cannot authorize engine publication.
    pub fn retry_pending(&mut self) -> Result<BodyRef> {
        if self.frozen {
            return Err(ErrorCode::ChainConflict.into());
        }
        if self.observation_failed {
            return Err(ErrorCode::CommitUncertain.into());
        }
        let pending = self.pending.as_ref().ok_or(ErrorCode::NoPendingHistory)?;
        let actual = self.store.recover()?;
        check_identity(self.store.as_ref(), self.genesis_hash)?;
        if actual == Some(pending.batch.new_head) {
            for (table, key, value) in &pending.batch.records {
                if self
                    .store
                    .read_record_bounded(*table, key, value.len())?
                    .as_deref()
                    != Some(value.as_slice())
                {
                    self.frozen = true;
                    return Err(ErrorCode::StorageCorrupt.into());
                }
            }
            let frame = self
                .store
                .read_object_bounded(
                    SpaceId::Host,
                    &pending.reference.id.0,
                    pending.reference.len as usize + FRAME_OVERHEAD,
                )?
                .ok_or(ErrorCode::HistoryUnavailable)?;
            self.unframe(&pending.reference, &frame)?;
            return Err(ErrorCode::CommitUncertain.into());
        }
        if actual != pending.batch.expected_head {
            self.frozen = true;
            return Err(ErrorCode::StorageCorrupt.into());
        }
        for (table, key, value) in &pending.batch.records {
            if pending.batch.removes_record(*table, key) {
                continue;
            }
            if self
                .store
                .read_record_bounded(*table, key, value.len())?
                .is_some()
            {
                self.frozen = true;
                return Err(ErrorCode::StorageCorrupt.into());
            }
        }
        if self
            .store
            .read_object_bounded(
                SpaceId::Host,
                &pending.reference.id.0,
                pending.reference.len as usize + FRAME_OVERHEAD,
            )?
            .is_some()
        {
            self.frozen = true;
            return Err(ErrorCode::StorageCorrupt.into());
        }
        self.commit_pending()
    }

    /// Retries the caller's retained candidate even if an earlier failure
    /// happened before this adapter could construct its pending batch. The
    /// recovered store must still have exactly the acknowledged head.
    pub fn retry_exact(&mut self, bytes: &[u8]) -> Result<BodyRef> {
        self.retry_exact_inner(bytes, None)
    }

    /// [`Self::retry_exact`] for [`Self::persist_block`].
    pub fn retry_exact_block(
        &mut self,
        certificate: &SignedHeader,
        bytes: &[u8],
    ) -> Result<BodyRef> {
        self.retry_exact_inner(bytes, Some(certificate))
    }

    fn retry_exact_inner(&mut self, bytes: &[u8], block: Option<&SignedHeader>) -> Result<BodyRef> {
        if self.frozen {
            return Err(ErrorCode::ChainConflict.into());
        }
        if self.observation_failed {
            return Err(ErrorCode::CommitUncertain.into());
        }
        let (reference, frame) = self.frame(bytes)?;
        if let Some(pending) = &self.pending {
            if pending.reference != reference
                || pending
                    .batch
                    .objects
                    .first()
                    .map(|(_, _, bytes)| bytes.as_slice())
                    != Some(frame.as_slice())
            {
                return Err(ErrorCode::PendingHistoryMismatch.into());
            }
            return self.retry_pending();
        }
        let actual = self.store.recover()?;
        check_identity(self.store.as_ref(), self.genesis_hash)?;
        if actual != Some(self.head) {
            self.frozen = true;
            return Err(ErrorCode::StorageCorrupt.into());
        }
        self.persist_inner(bytes, block)
    }

    /// Retains the first authenticated certificate seen at a chain height,
    /// including samples ahead of executed state. The caller authenticates it
    /// before this call and authenticates any returned conflicting certificate
    /// before recording equivocation. Matching native headers are idempotent.
    pub fn remember_header(&mut self, certificate: &SignedHeader) -> Result<Option<SignedHeader>> {
        self.ready()?;
        if certificate.genesis_hash != self.genesis_hash {
            return Err(ErrorCode::IdentityMismatch.into());
        }
        if let Some(prior) = self.observation(certificate.header.height)? {
            return Ok((prior.header.hash() != certificate.header.hash()).then_some(prior));
        }
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(ErrorCode::CapacityReached)?;
        let mut entry = vec![3];
        entry.extend_from_slice(&encode(&(
            certificate.header.height,
            certificate.header.hash(),
        )));
        let next = entry_head(self.genesis_hash, sequence, &entry);
        let mut batch = CommitBatch::new(Some(self.head), next);
        batch
            .put_record(
                Table::Inclusions,
                certificate.header.height.to_be_bytes(),
                encode(certificate),
            )
            .put_record(Table::Manifests, sequence.to_be_bytes(), entry);
        match self.store.commit(&batch) {
            CommitOutcome::Committed => {
                self.head = next;
                Ok(None)
            }
            CommitOutcome::NotCommitted(_) => {
                self.observation_failed = true;
                Err(ErrorCode::StorageUnavailable.into())
            }
            CommitOutcome::Indeterminate(_) => {
                self.observation_failed = true;
                Err(ErrorCode::CommitUncertain.into())
            }
        }
    }

    fn observation(&self, height: u64) -> Result<Option<SignedHeader>> {
        self.store
            .read_record_bounded(Table::Inclusions, &height.to_be_bytes(), CERTIFICATE_LEN)?
            .map(|bytes| {
                let certificate: SignedHeader = decode_exact(&bytes).map_err(|e| {
                    ErrorCode::StorageCorrupt
                        .caused_by(format_args!("observed header {height}: {e}"))
                })?;
                if certificate.genesis_hash != self.genesis_hash
                    || certificate.header.height != height
                {
                    return Err(ErrorCode::StorageCorrupt.into());
                }
                Ok(certificate)
            })
            .transpose()
    }

    fn commit_pending(&mut self) -> Result<BodyRef> {
        let pending = self.pending.as_ref().ok_or(ErrorCode::NoPendingHistory)?;
        match self.store.commit(&pending.batch) {
            CommitOutcome::Committed => {
                let pending = self.pending.take().expect("pending checked");
                self.head = pending.batch.new_head;
                Ok(pending.reference)
            }
            CommitOutcome::NotCommitted(_) => Err(ErrorCode::StorageUnavailable.into()),
            CommitOutcome::Indeterminate(_) => Err(ErrorCode::CommitUncertain.into()),
        }
    }

    /// The height index row of `height`, if the body of that height was
    /// persisted with [`Self::persist_block`] and not pruned.
    pub fn indexed(&self, height: u64) -> Result<Option<IndexedBlock>> {
        let Some(row) =
            self.store
                .read_record_bounded(Table::Blocks, &height.to_be_bytes(), INDEX_CAP)?
        else {
            return Ok(None);
        };
        let corrupt =
            || ErrorCode::StorageCorrupt.caused_by(format_args!("history index {height}"));
        if row.len() < 44 {
            return Err(corrupt());
        }
        let body: BodyRef = decode_exact(&row[..36]).map_err(|_| corrupt())?;
        let entry = u64::from_be_bytes(row[36..44].try_into().expect("8 bytes"));
        let certificate: SignedHeader = decode_exact(&row[44..]).map_err(|_| corrupt())?;
        if certificate.genesis_hash != self.genesis_hash || certificate.header.height != height {
            return Err(corrupt());
        }
        Ok(Some(IndexedBlock {
            body,
            entry,
            certificate,
        }))
    }

    /// How far bodies are pruned (see [`Self::prune_below`]).
    pub fn pruned(&self) -> Result<HistoryPruned> {
        let Some(bytes) = self
            .store
            .read_record_bounded(Table::Meta, PRUNED_KEY, 16)?
        else {
            return Ok(HistoryPruned::default());
        };
        if bytes.len() != 16 {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(HistoryPruned {
            height: u64::from_be_bytes(bytes[..8].try_into().expect("8 bytes")),
            entry: u64::from_be_bytes(bytes[8..].try_into().expect("8 bytes")),
        })
    }

    /// Deletes the bodies, index rows and observed headers of heights below
    /// `floor` (height zero is kept), at least `budget` heights and a
    /// sixteenth of the backlog per call, with the history entries up to
    /// the last pruned body's. The caller passes a floor the node's own
    /// durable records already expired: nothing published still names
    /// these bodies. Returns the new position.
    pub fn prune_below(&mut self, floor: u64, budget: u64) -> Result<HistoryPruned> {
        self.ready()?;
        let mut pruned = self.pruned()?;
        let start = pruned.height.max(1);
        if floor <= start {
            return Ok(pruned);
        }
        let backlog = floor - start;
        let count = arkiv_db::durable::records::deletion_count(backlog, budget);
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(ErrorCode::CapacityReached)?;
        let mut entry = vec![4];
        let mut batch = CommitBatch::new(Some(self.head), Head::default());
        let mut last_entry = None;
        for height in start..start + count {
            let key = height.to_be_bytes();
            if let Some(indexed) = self.indexed(height)? {
                batch
                    .remove_object(SpaceId::Host, indexed.body.id.0)
                    .remove_record(Table::Blocks, key.to_vec());
                last_entry = Some(indexed.entry);
            }
            batch.remove_record(Table::Inclusions, key.to_vec());
        }
        pruned.height = start + count;
        if let Some(last) = last_entry {
            // Every entry up to the last pruned body's, which is below the
            // head entry: the bodies of the retained heights came later.
            let first = pruned.entry.max(1);
            if last >= self.head.height {
                return Err(ErrorCode::StorageCorrupt.into());
            }
            for number in first..=last {
                batch.remove_record(Table::Manifests, number.to_be_bytes().to_vec());
            }
            pruned.entry = pruned.entry.max(last + 1);
        }
        let mut position = pruned.height.to_be_bytes().to_vec();
        position.extend_from_slice(&pruned.entry.to_be_bytes());
        entry.extend_from_slice(&encode(&(pruned.height, pruned.entry)));
        let next = entry_head(self.genesis_hash, sequence, &entry);
        batch.new_head = next;
        batch
            .remove_record(Table::Meta, PRUNED_KEY.to_vec())
            .put_record(Table::Meta, PRUNED_KEY, position)
            .put_record(Table::Manifests, sequence.to_be_bytes(), entry);
        match self.store.commit(&batch) {
            CommitOutcome::Committed => {
                self.head = next;
                Ok(pruned)
            }
            CommitOutcome::NotCommitted(_) => Err(ErrorCode::StorageUnavailable.into()),
            CommitOutcome::Indeterminate(_) => {
                // Settle which head is durable so later bodies can persist.
                match self.store.recover() {
                    Ok(Some(head)) if head == next => {
                        self.head = next;
                        Ok(pruned)
                    }
                    Ok(Some(head)) if head == self.head => {
                        Err(ErrorCode::StorageUnavailable.into())
                    }
                    _ => {
                        self.frozen = true;
                        Err(ErrorCode::CommitUncertain.into())
                    }
                }
            }
        }
    }

    /// The body of the block at `height`, read through the height index.
    pub fn read_height(&self, height: u64) -> Result<Option<(IndexedBlock, Vec<u8>)>> {
        let Some(indexed) = self.indexed(height)? else {
            return Ok(None);
        };
        let bytes = self.read(&indexed.body)?;
        Ok(Some((indexed, bytes)))
    }

    pub fn read(&self, reference: &BodyRef) -> Result<Vec<u8>> {
        self.ready()?;
        if reference.len as usize > BODY_CAP {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        let frame = self
            .store
            .read_object_bounded(
                SpaceId::Host,
                &reference.id.0,
                reference.len as usize + FRAME_OVERHEAD,
            )?
            .ok_or(ErrorCode::HistoryUnavailable)?;
        Ok(self.unframe(reference, &frame)?.to_vec())
    }

    /// The caller must authenticate both conflicting certificates first.
    /// A failure still fences this handle; a durable marker fences reopen.
    pub fn freeze_conflict(&mut self, evidence: &[u8]) -> Result<()> {
        if evidence.is_empty() || evidence.len() > CONFLICT_CAP {
            return Err(ErrorCode::InvalidRequest.into());
        }
        if self.frozen {
            return Err(ErrorCode::ChainConflict.into());
        }
        self.frozen = true;
        // The pending body may already have committed. Never publish another
        // sequence or resume its publication after authentic equivocation.
        if self.pending.is_some() {
            return Err(ErrorCode::CommitUncertain.into());
        }
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(ErrorCode::CapacityReached)?;
        let mut entry = vec![2];
        entry.extend_from_slice(digest("arkiv/signed-conflict/v1", evidence).as_slice());
        let next = entry_head(self.genesis_hash, sequence, &entry);
        let mut batch = CommitBatch::new(Some(self.head), next);
        batch
            .put_record(Table::Meta, CONFLICT_KEY, evidence)
            .put_record(Table::Manifests, sequence.to_be_bytes(), entry);
        match self.store.commit(&batch) {
            CommitOutcome::Committed => {
                self.head = next;
                Ok(())
            }
            CommitOutcome::NotCommitted(_) => Err(ErrorCode::StorageUnavailable.into()),
            CommitOutcome::Indeterminate(_) => Err(ErrorCode::CommitUncertain.into()),
        }
    }

    fn ready(&self) -> Result<()> {
        if self.frozen {
            Err(ErrorCode::ChainConflict.into())
        } else if self.pending.is_some() || self.observation_failed {
            Err(ErrorCode::CommitUncertain.into())
        } else {
            Ok(())
        }
    }

    fn frame(&self, bytes: &[u8]) -> Result<(BodyRef, Vec<u8>)> {
        if bytes.len() > BODY_CAP {
            return Err(ErrorCode::CapacityReached.into());
        }
        let len = bytes.len() as u32;
        let mut frame = Vec::with_capacity(FRAME_OVERHEAD + bytes.len());
        frame.extend_from_slice(FRAME);
        frame.extend_from_slice(self.genesis_hash.as_slice());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(bytes);
        Ok((
            BodyRef {
                id: keccak256(&frame),
                len,
            },
            frame,
        ))
    }

    fn unframe<'a>(&self, reference: &BodyRef, frame: &'a [u8]) -> Result<&'a [u8]> {
        if frame.len() != FRAME_OVERHEAD + reference.len as usize
            || keccak256(frame) != reference.id
            || !frame.starts_with(FRAME)
            || frame.get(FRAME.len()..FRAME.len() + 32) != Some(self.genesis_hash.as_slice())
            || frame.get(FRAME.len() + 32..FRAME_OVERHEAD)
                != Some(reference.len.to_be_bytes().as_slice())
        {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(&frame[FRAME_OVERHEAD..])
    }
}

/// A height index row: body reference ‖ entry sequence ‖ certificate.
pub fn index_row(body: &BodyRef, entry: u64, certificate: &SignedHeader) -> Vec<u8> {
    let mut row = encode(body);
    row.extend_from_slice(&entry.to_be_bytes());
    row.extend_from_slice(&encode(certificate));
    row
}

/// The body reference of a history entry that persisted a body.
pub fn entry_body(entry: &[u8]) -> Option<BodyRef> {
    match entry {
        [1, bytes @ ..] => decode_exact(bytes).ok(),
        _ => None,
    }
}

fn check_identity(store: &dyn VersionedStore, genesis_hash: B256) -> Result<()> {
    let mut identity = genesis_hash.to_vec();
    identity.extend_from_slice(&store.database_id().0);
    if store
        .read_record_bounded(Table::Meta, PROFILE_KEY, 32)?
        .as_deref()
        != Some(PROFILE)
        || store
            .read_record_bounded(Table::Meta, ID_KEY, 48)?
            .as_deref()
            != Some(identity.as_slice())
    {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    Ok(())
}

fn entry_head(genesis_hash: B256, sequence: u64, entry: &[u8]) -> Head {
    let mut bytes = genesis_hash.to_vec();
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(entry);
    Head {
        height: sequence,
        hash: digest("arkiv/signed-history-entry/v1", &bytes).0,
    }
}
