//! Request identity and the pending and inclusion indexes.
//!
//! A request is identified by `(scope, chain_id, actor, request_id)`, with
//! scope `0x00` for user transactions and `0x01` for admin inputs, so the
//! same request id may be used once as a transaction and once as an admin
//! input. Its content is identified by its digest (`arkiv/tx/v1` or
//! `arkiv/admin/v1`).
//!
//! [`RequestIndex`] is the state machine the node keeps under one mutex:
//!
//! | Identity is | Same digest | Different digest |
//! | --- | --- | --- |
//! | included | [`Submission::Included`] | [`Submission::Conflict`] |
//! | pending | [`Submission::Pending`] | [`Submission::Conflict`] |
//! | unknown | [`Submission::Accepted`] (now pending) | — |
//!
//! An identity is pending from its acceptance until the block containing
//! it commits ([`RequestIndex::commit_block`] moves it to the inclusion
//! index, in the same critical section that publishes the block) or until
//! the node drops it ([`RequestIndex::drop_pending`]). Identities inside a
//! block that is being produced, or whose commit is indeterminate, stay
//! pending. Pending means held in memory only: a restart forgets it, and
//! only an inclusion survives. This module is a pure data structure; the
//! node runtime (step N1) wraps it with the queue.

use std::{collections::BTreeMap, fmt};

use arkiv_db::{B256, Decode, DecodeError, Decoder, Encode, Encoder, OwnerId};

use crate::{
    block::{BlockInputs, RequestId},
    validate::BlockFault,
};

const SCOPE_TX: u8 = 0x00;
const SCOPE_ADMIN: u8 = 0x01;

/// Which kind of request an identity names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// A user transaction (`0x00`).
    Tx,
    /// An admin input (`0x01`).
    Admin,
}

/// One byte: `00` transaction, `01` admin.
impl Encode for Scope {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::Tx => SCOPE_TX,
            Self::Admin => SCOPE_ADMIN,
        });
    }
}

impl Decode for Scope {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            SCOPE_TX => Ok(Self::Tx),
            SCOPE_ADMIN => Ok(Self::Admin),
            tag => Err(DecodeError::UnknownTag { kind: "Scope", tag }),
        }
    }
}

/// The identity of a request: `(scope, chain_id, actor, request_id)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestIdentity {
    /// Transaction or admin input.
    pub scope: Scope,
    /// The chain.
    pub chain_id: u64,
    /// The authenticated actor.
    pub actor: OwnerId,
    /// The client-chosen id.
    pub request_id: RequestId,
}

impl RequestIdentity {
    /// Every identity of a block's inputs with its scope and position:
    /// admin inputs first, then transactions. Admin inputs carry no chain
    /// id; theirs is `chain_id`.
    pub fn of_block(
        chain_id: u64,
        inputs: &BlockInputs,
    ) -> impl Iterator<Item = (Self, Scope, u32)> + '_ {
        let position = |index: usize| u32::try_from(index).expect("block counts fit in u32");
        let admin = inputs.admin.iter().enumerate().map(move |(index, input)| {
            let identity = Self {
                scope: Scope::Admin,
                chain_id,
                actor: input.actor,
                request_id: input.request_id,
            };
            (identity, Scope::Admin, position(index))
        });
        let txs = inputs.txs.iter().enumerate().map(move |(index, tx)| {
            let identity = Self {
                scope: Scope::Tx,
                chain_id: tx.chain_id,
                actor: tx.actor,
                request_id: tx.request_id,
            };
            (identity, Scope::Tx, position(index))
        });
        admin.chain(txs)
    }
}

/// `scope ‖ chain_id ‖ actor ‖ request_id` (61 bytes).
impl Encode for RequestIdentity {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.scope);
        encoder.u64(self.chain_id);
        encoder.put(&self.actor);
        encoder.put(&self.request_id);
    }
}

impl Decode for RequestIdentity {
    const MIN_ENCODED_LEN: usize = 1 + 8 + 20 + 32;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            scope: decoder.get()?,
            chain_id: decoder.u64()?,
            actor: decoder.get()?,
            request_id: decoder.get()?,
        })
    }
}

/// Where a request was included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inclusion {
    /// The block height.
    pub height: u64,
    /// Transaction or admin input.
    pub scope: Scope,
    /// Position among the block's transactions or admin inputs.
    pub index: u32,
    /// The request's content digest.
    pub digest: B256,
}

/// `height ‖ scope ‖ index u32 ‖ digest` (45 bytes).
impl Encode for Inclusion {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.height);
        encoder.put(&self.scope);
        encoder.u32(self.index);
        encoder.put(&self.digest);
    }
}

impl Decode for Inclusion {
    const MIN_ENCODED_LEN: usize = 8 + 1 + 4 + 32;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            height: decoder.u64()?,
            scope: decoder.get()?,
            index: decoder.u32()?,
            digest: decoder.get()?,
        })
    }
}

/// The answer to a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submission {
    /// The same request is already included; nothing changed.
    Included(Inclusion),
    /// The same request is already pending; nothing changed.
    Pending,
    /// The request is new and now pending; the caller queues it (or calls
    /// [`RequestIndex::drop_pending`] if the queue refuses it).
    Accepted,
    /// The identity is included or pending with different content.
    Conflict {
        /// The digest already recorded for the identity.
        existing: B256,
    },
}

/// What the index knows about an identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestStatus {
    /// Included by a committed block.
    Included(Inclusion),
    /// Accepted, not yet in a committed block; held in memory only.
    Pending {
        /// The pending request's digest.
        digest: B256,
    },
    /// Never accepted, or dropped.
    Unknown,
}

/// The pending and inclusion indexes; see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub struct RequestIndex {
    included: BTreeMap<RequestIdentity, Inclusion>,
    pending: BTreeMap<RequestIdentity, B256>,
}

impl RequestIndex {
    /// Empty indexes.
    pub const fn new() -> Self {
        Self {
            included: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    /// Submits a request with content digest `digest`; see the
    /// [module docs](self) for the transitions.
    pub fn submit(&mut self, identity: RequestIdentity, digest: B256) -> Submission {
        if let Some(inclusion) = self.included.get(&identity) {
            return if inclusion.digest == digest {
                Submission::Included(*inclusion)
            } else {
                Submission::Conflict {
                    existing: inclusion.digest,
                }
            };
        }
        match self.pending.get(&identity) {
            Some(existing) if *existing == digest => Submission::Pending,
            Some(existing) => Submission::Conflict {
                existing: *existing,
            },
            None => {
                self.pending.insert(identity, digest);
                Submission::Accepted
            }
        }
    }

    /// What the index knows about `identity`.
    pub fn status(&self, identity: &RequestIdentity) -> RequestStatus {
        if let Some(inclusion) = self.included.get(identity) {
            return RequestStatus::Included(*inclusion);
        }
        match self.pending.get(identity) {
            Some(digest) => RequestStatus::Pending { digest: *digest },
            None => RequestStatus::Unknown,
        }
    }

    /// Forgets a pending identity (its request left the queue without being
    /// included). Returns whether it was pending; included identities are
    /// never dropped.
    pub fn drop_pending(&mut self, identity: &RequestIdentity) -> bool {
        self.pending.remove(identity).is_some()
    }

    /// Checks that no identity of `inputs` repeats within the block or was
    /// included by an earlier block.
    pub fn check_block(&self, chain_id: u64, inputs: &BlockInputs) -> Result<(), BlockFault> {
        let mut seen = std::collections::BTreeSet::new();
        for (identity, scope, index) in RequestIdentity::of_block(chain_id, inputs) {
            if !seen.insert(identity) {
                return Err(BlockFault::DuplicateIdentity { scope, index });
            }
            if self.included.contains_key(&identity) {
                return Err(BlockFault::AlreadyIncluded { scope, index });
            }
        }
        Ok(())
    }

    /// Records the committed block at `height`: every identity of its
    /// inputs moves to the inclusion index (from the pending index when it
    /// was pending there). Checks [`check_block`](Self::check_block) first
    /// and changes nothing if it fails.
    pub fn commit_block(
        &mut self,
        chain_id: u64,
        height: u64,
        inputs: &BlockInputs,
    ) -> Result<(), BlockFault> {
        self.check_block(chain_id, inputs)?;
        let digests = inputs
            .admin
            .iter()
            .map(|input| input.digest())
            .chain(inputs.txs.iter().map(|tx| tx.digest()));
        for ((identity, scope, index), digest) in
            RequestIdentity::of_block(chain_id, inputs).zip(digests)
        {
            self.pending.remove(&identity);
            self.included.insert(
                identity,
                Inclusion {
                    height,
                    scope,
                    index,
                    digest,
                },
            );
        }
        Ok(())
    }

    /// Number of included identities.
    pub fn included_len(&self) -> usize {
        self.included.len()
    }

    /// Number of pending identities.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tx => "transaction",
            Self::Admin => "admin",
        })
    }
}
