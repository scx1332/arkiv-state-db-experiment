//! Separate, content-addressed retained bodies. Presence is never inclusion.
//!
//! This store has its own profile and storage sequence, independent of chain
//! height. Callers publish engine references only after `persist` succeeds.
use super::{Error, Result, protocol::SignedHeader};
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
            .put_record(Table::Manifests, 0u64.to_be_bytes(), vec![0]);
        match store.commit(batch) {
            CommitOutcome::Committed => {}
            CommitOutcome::NotCommitted(_) => return Err(Error("StorageUnavailable")),
            CommitOutcome::Indeterminate(_) => return Err(Error("CommitUncertain")),
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
            return Err(Error("ChainConflict"));
        }
        let head = store.head()?.ok_or(Error("StorageCorrupt"))?;
        let entry = store
            .read_record_bounded(Table::Manifests, &head.height.to_be_bytes(), ENTRY_CAP)?
            .ok_or(Error("StorageCorrupt"))?;
        if entry_head(genesis_hash, head.height, &entry) != head {
            return Err(Error("StorageCorrupt"));
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
                let reference: BodyRef =
                    decode_exact(bytes).map_err(|_| Error("StorageCorrupt"))?;
                history.read(&reference)?;
            }
            [3, bytes @ ..] if head.height > 0 => {
                let (height, hash): (u64, B256) =
                    decode_exact(bytes).map_err(|_| Error("StorageCorrupt"))?;
                let certificate = history
                    .observation(height)?
                    .ok_or(Error("StorageCorrupt"))?;
                if certificate.header.hash() != hash {
                    return Err(Error("StorageCorrupt"));
                }
            }
            _ => return Err(Error("StorageCorrupt")),
        }
        Ok(history)
    }

    /// Idempotently persists the exact framed bytes. Different candidates at
    /// the same chain height never compete for a storage key.
    pub fn reference(&self, bytes: &[u8]) -> Result<BodyRef> {
        self.ready()?;
        self.frame(bytes).map(|(reference, _)| reference)
    }

    pub fn persist(&mut self, bytes: &[u8]) -> Result<BodyRef> {
        self.ready()?;
        let (reference, frame) = self.frame(bytes)?;
        if self.store.head()? != Some(self.head) {
            self.frozen = true;
            return Err(Error("StorageCorrupt"));
        }
        if let Some(existing) =
            self.store
                .read_object_bounded(SpaceId::Host, &reference.id.0, frame.len())?
        {
            self.unframe(&reference, &existing)?;
            if existing != frame {
                return Err(Error("StorageCorrupt"));
            }
            return Ok(reference);
        }
        let mut entry = vec![1];
        entry.extend_from_slice(&encode(&reference));
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
        let mut batch = CommitBatch::new(
            Some(self.head),
            entry_head(self.genesis_hash, sequence, &entry),
        );
        batch
            .put_object(SpaceId::Host, reference.id.0, frame)
            .put_record(Table::Manifests, sequence.to_be_bytes(), entry);
        self.pending = Some(Pending { reference, batch });
        self.commit_pending()
    }

    /// Explicitly reconciles and retries the retained batch, without accepting
    /// replacement bytes. Observed committed data after a failed sync remains
    /// durability-unknown; it cannot authorize engine publication.
    pub fn retry_pending(&mut self) -> Result<BodyRef> {
        if self.frozen {
            return Err(Error("ChainConflict"));
        }
        if self.observation_failed {
            return Err(Error("CommitUncertain"));
        }
        let pending = self.pending.as_ref().ok_or(Error("NoPendingHistory"))?;
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
                    return Err(Error("StorageCorrupt"));
                }
            }
            let frame = self
                .store
                .read_object_bounded(
                    SpaceId::Host,
                    &pending.reference.id.0,
                    pending.reference.len as usize + FRAME_OVERHEAD,
                )?
                .ok_or(Error("HistoryUnavailable"))?;
            self.unframe(&pending.reference, &frame)?;
            return Err(Error("CommitUncertain"));
        }
        if actual != pending.batch.expected_head {
            self.frozen = true;
            return Err(Error("StorageCorrupt"));
        }
        for (table, key, value) in &pending.batch.records {
            if self
                .store
                .read_record_bounded(*table, key, value.len())?
                .is_some()
            {
                self.frozen = true;
                return Err(Error("StorageCorrupt"));
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
            return Err(Error("StorageCorrupt"));
        }
        self.commit_pending()
    }

    /// Retries the caller's retained candidate even if an earlier failure
    /// happened before this adapter could construct its pending batch. The
    /// recovered store must still have exactly the acknowledged head.
    pub fn retry_exact(&mut self, bytes: &[u8]) -> Result<BodyRef> {
        if self.frozen {
            return Err(Error("ChainConflict"));
        }
        if self.observation_failed {
            return Err(Error("CommitUncertain"));
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
                return Err(Error("PendingHistoryMismatch"));
            }
            return self.retry_pending();
        }
        let actual = self.store.recover()?;
        check_identity(self.store.as_ref(), self.genesis_hash)?;
        if actual != Some(self.head) {
            self.frozen = true;
            return Err(Error("StorageCorrupt"));
        }
        self.persist(bytes)
    }

    /// Retains the first authenticated certificate seen at a chain height,
    /// including samples ahead of executed state. The caller authenticates it
    /// before this call and authenticates any returned conflicting certificate
    /// before recording equivocation. Matching native headers are idempotent.
    pub fn remember_header(&mut self, certificate: &SignedHeader) -> Result<Option<SignedHeader>> {
        self.ready()?;
        if certificate.genesis_hash != self.genesis_hash {
            return Err(Error("IdentityMismatch"));
        }
        if let Some(prior) = self.observation(certificate.header.height)? {
            return Ok((prior.header.hash() != certificate.header.hash()).then_some(prior));
        }
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
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
        match self.store.commit(batch) {
            CommitOutcome::Committed => {
                self.head = next;
                Ok(None)
            }
            CommitOutcome::NotCommitted(_) => {
                self.observation_failed = true;
                Err(Error("StorageUnavailable"))
            }
            CommitOutcome::Indeterminate(_) => {
                self.observation_failed = true;
                Err(Error("CommitUncertain"))
            }
        }
    }

    fn observation(&self, height: u64) -> Result<Option<SignedHeader>> {
        self.store
            .read_record_bounded(Table::Inclusions, &height.to_be_bytes(), CERTIFICATE_LEN)?
            .map(|bytes| {
                let certificate: SignedHeader =
                    decode_exact(&bytes).map_err(|_| Error("StorageCorrupt"))?;
                if certificate.genesis_hash != self.genesis_hash
                    || certificate.header.height != height
                {
                    return Err(Error("StorageCorrupt"));
                }
                Ok(certificate)
            })
            .transpose()
    }

    fn commit_pending(&mut self) -> Result<BodyRef> {
        let pending = self.pending.as_ref().ok_or(Error("NoPendingHistory"))?;
        match self.store.commit(pending.batch.clone()) {
            CommitOutcome::Committed => {
                let pending = self.pending.take().expect("pending checked");
                self.head = pending.batch.new_head;
                Ok(pending.reference)
            }
            CommitOutcome::NotCommitted(_) => Err(Error("StorageUnavailable")),
            CommitOutcome::Indeterminate(_) => Err(Error("CommitUncertain")),
        }
    }

    pub fn read(&self, reference: &BodyRef) -> Result<Vec<u8>> {
        self.ready()?;
        if reference.len as usize > BODY_CAP {
            return Err(Error("StorageCorrupt"));
        }
        let frame = self
            .store
            .read_object_bounded(
                SpaceId::Host,
                &reference.id.0,
                reference.len as usize + FRAME_OVERHEAD,
            )?
            .ok_or(Error("HistoryUnavailable"))?;
        Ok(self.unframe(reference, &frame)?.to_vec())
    }

    /// The caller must authenticate both conflicting certificates first.
    /// A failure still fences this handle; a durable marker fences reopen.
    pub fn freeze_conflict(&mut self, evidence: &[u8]) -> Result<()> {
        if evidence.is_empty() || evidence.len() > CONFLICT_CAP {
            return Err(Error("InvalidRequest"));
        }
        if self.frozen {
            return Err(Error("ChainConflict"));
        }
        self.frozen = true;
        // The pending body may already have committed. Never publish another
        // sequence or resume its publication after authentic equivocation.
        if self.pending.is_some() {
            return Err(Error("CommitUncertain"));
        }
        let sequence = self
            .head
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
        let mut entry = vec![2];
        entry.extend_from_slice(digest("arkiv/signed-conflict/v1", evidence).as_slice());
        let next = entry_head(self.genesis_hash, sequence, &entry);
        let mut batch = CommitBatch::new(Some(self.head), next);
        batch
            .put_record(Table::Meta, CONFLICT_KEY, evidence)
            .put_record(Table::Manifests, sequence.to_be_bytes(), entry);
        match self.store.commit(batch) {
            CommitOutcome::Committed => {
                self.head = next;
                Ok(())
            }
            CommitOutcome::NotCommitted(_) => Err(Error("StorageUnavailable")),
            CommitOutcome::Indeterminate(_) => Err(Error("CommitUncertain")),
        }
    }

    fn ready(&self) -> Result<()> {
        if self.frozen {
            Err(Error("ChainConflict"))
        } else if self.pending.is_some() || self.observation_failed {
            Err(Error("CommitUncertain"))
        } else {
            Ok(())
        }
    }
    fn frame(&self, bytes: &[u8]) -> Result<(BodyRef, Vec<u8>)> {
        if bytes.len() > BODY_CAP {
            return Err(Error("CapacityReached"));
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
            return Err(Error("StorageCorrupt"));
        }
        Ok(&frame[FRAME_OVERHEAD..])
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
        return Err(Error("IdentityMismatch"));
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
