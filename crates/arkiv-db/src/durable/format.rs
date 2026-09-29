//! Engine-only durable profile; logical state encodings remain unchanged.

use super::{DurableError, corrupt, incompatible};
use crate::{
    B256, Decode, DecodeError, Decoder, Encode, Encoder, Limits, PriceSchedule, StateHeader,
    decode_exact, digest, encode,
};
use arkiv_db_trie::EMPTY_ROOT_HASH;

pub const PROFILE: &[u8; 8] = if cfg!(feature = "arkiv-v2") {
    b"ARKENG02"
} else {
    b"ARKENG01"
};
const PREFILLED_PROFILE: &[u8; 8] = if cfg!(feature = "arkiv-v2") {
    b"ARKENG03"
} else {
    b"ARKENG04"
};
pub const PROFILE_KEY: &[u8] = b"engine/profile";
pub const GENESIS_KEY: &[u8] = b"engine/genesis";
pub const GENESIS_BYTES: usize = 8 + 6 * 4 + 32 + 48 + 256 + 8;
pub const GENESIS_MAX_BYTES: usize = GENESIS_BYTES + StateHeader::MIN_ENCODED_LEN;
pub const MANIFEST_MAX_BYTES: usize = 4 + 32 + 8 + 1 + 40 + 348 + 32 + 32;

/// Immutable identity and admission bounds of an engine-only database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineGenesis {
    pub deployment: [u8; 32],
    pub limits: Limits,
    pub pricing: PriceSchedule,
    pub max_object_bytes: u64,
    /// Explicit initial state for an atomic prefilled genesis. `None` keeps
    /// the original empty-genesis encoding and identity byte for byte.
    pub initial_state: Option<StateHeader>,
}

impl EngineGenesis {
    pub fn new(deployment: [u8; 32], pricing: PriceSchedule) -> Self {
        Self {
            deployment,
            limits: Limits::DEFAULT,
            pricing,
            max_object_bytes: 1024 * 1024,
            initial_state: None,
        }
    }

    pub fn validate(&self) -> Result<(), DurableError> {
        let cap = usize::try_from(self.max_object_bytes)
            .map_err(|_| incompatible("object limit does not fit usize"))?;
        if cap == 0 || cap > isize::MAX as usize {
            return Err(incompatible("object limit must be finite and allocatable"));
        }
        // Sequence/string counts in existing codecs are u32. Never permit an
        // admitted value whose later canonical encoding would panic/narrow.
        for n in [
            self.limits.max_cells,
            self.limits.max_value_len,
            self.limits.max_page,
            self.limits.max_groups,
            self.limits.max_literals,
        ] {
            u32::try_from(n).map_err(|_| incompatible("engine limit exceeds codec width"))?;
        }
        self.limits
            .max_cells
            .checked_mul(self.limits.max_value_len.saturating_add(32))
            .ok_or_else(|| incompatible("row size arithmetic overflow"))?;
        if let Some(header) = &self.initial_state {
            decode_exact::<StateHeader>(&encode(header))
                .map_err(|_| incompatible("invalid initial state header"))?;
            if header.pricing != self.pricing {
                return Err(incompatible("initial state pricing does not match genesis"));
            }
            if header.catalog_len > self.limits.namespace_cap {
                return Err(incompatible("initial state exceeds namespace cap"));
            }
        }
        Ok(())
    }

    pub fn id(&self) -> B256 {
        digest("arkiv/engine-genesis/v1", &encode(self))
    }

    pub fn initial_header(&self) -> StateHeader {
        if let Some(header) = &self.initial_state {
            return header.clone();
        }
        StateHeader {
            format: crate::state::STATE_FORMAT,
            catalog_root: EMPTY_ROOT_HASH,
            catalog_len: 0,
            next_namespace: 1,
            pricing: self.pricing.clone(),
            host_root: EMPTY_ROOT_HASH,
            host_len: 0,
        }
    }

    pub fn envelope(&self) -> Vec<u8> {
        let mut bytes = encode(self);
        bytes.extend_from_slice(self.id().as_slice());
        bytes
    }

    pub(crate) fn from_envelope(bytes: &[u8]) -> Result<Self, DurableError> {
        if bytes.len() != GENESIS_BYTES + 32 && bytes.len() != GENESIS_MAX_BYTES + 32 {
            return Err(corrupt("genesis envelope size"));
        }
        let split = bytes.len() - 32;
        let genesis: Self = decode_exact(&bytes[..split]).map_err(decode_error)?;
        genesis
            .validate()
            .map_err(|error| corrupt(format!("invalid stored genesis: {error}")))?;
        if genesis.id().as_slice() != &bytes[split..] {
            return Err(corrupt("genesis digest"));
        }
        Ok(genesis)
    }
}

impl Encode for EngineGenesis {
    fn encode_to(&self, e: &mut Encoder) {
        for b in if self.initial_state.is_some() {
            PREFILLED_PROFILE
        } else {
            PROFILE
        } {
            e.u8(*b);
        }
        for version in [
            crate::LAYOUT_VERSION,
            crate::CODEC_VERSION,
            crate::state::STATE_FORMAT,
            3,
            2,
            crate::proof::PROOF_VERSION,
        ] {
            e.u32(version);
        }
        for b in self.deployment {
            e.u8(b);
        }
        e.put(&self.limits);
        e.put(&self.pricing);
        e.u64(self.max_object_bytes);
        if let Some(header) = &self.initial_state {
            e.put(header);
        }
    }
}

impl Decode for EngineGenesis {
    const MIN_ENCODED_LEN: usize = GENESIS_BYTES;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let profile = d.array::<8>()?;
        let prefilled = if &profile == PREFILLED_PROFILE {
            true
        } else if &profile == PROFILE {
            false
        } else {
            return Err(DecodeError::Invalid {
                kind: "engine profile",
                reason: "wrong magic",
            });
        };
        for expected in [
            crate::LAYOUT_VERSION,
            crate::CODEC_VERSION,
            crate::state::STATE_FORMAT,
            3,
            2,
            crate::proof::PROOF_VERSION,
        ] {
            let version = d.u32()?;
            if version != expected {
                return Err(DecodeError::UnsupportedVersion {
                    kind: "engine profile",
                    version,
                });
            }
        }
        let mut deployment = [0; 32];
        for b in &mut deployment {
            *b = d.u8()?;
        }
        Ok(Self {
            deployment,
            limits: d.get()?,
            pricing: d.get()?,
            max_object_bytes: d.u64()?,
            initial_state: if prefilled { Some(d.get()?) } else { None },
        })
    }
}

/// One immutable engine publication; identities are not protocol block hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineManifest {
    pub genesis: B256,
    pub sequence: u64,
    pub parent: Option<(u64, B256)>,
    pub state: StateHeader,
    pub state_root: B256,
}

impl EngineManifest {
    pub fn id(&self) -> B256 {
        digest("arkiv/engine-manifest/v1", &encode(self))
    }
    pub fn envelope(&self) -> Vec<u8> {
        let mut bytes = encode(self);
        bytes.extend_from_slice(self.id().as_slice());
        bytes
    }
    pub(crate) fn from_envelope(bytes: &[u8]) -> Result<Self, DurableError> {
        let split = bytes
            .len()
            .checked_sub(32)
            .ok_or_else(|| corrupt("manifest envelope truncated"))?;
        let manifest: Self = decode_exact(&bytes[..split]).map_err(decode_error)?;
        if manifest.id().as_slice() != &bytes[split..] {
            return Err(corrupt("manifest digest"));
        }
        Ok(manifest)
    }
}

impl Encode for EngineManifest {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(1);
        e.put(&self.genesis);
        e.u64(self.sequence);
        match self.parent {
            None => e.u8(0),
            Some((sequence, hash)) => {
                e.u8(1);
                e.u64(sequence);
                e.put(&hash);
            }
        }
        e.put(&self.state);
        e.put(&self.state_root);
    }
}

impl Decode for EngineManifest {
    const MIN_ENCODED_LEN: usize = 4 + 32 + 8 + 1 + StateHeader::MIN_ENCODED_LEN + 32;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let version = d.u32()?;
        if version != 1 {
            return Err(DecodeError::UnsupportedVersion {
                kind: "engine manifest",
                version,
            });
        }
        let genesis = d.get()?;
        let sequence = d.u64()?;
        let parent = match d.u8()? {
            0 => None,
            1 => Some((d.u64()?, d.get()?)),
            _ => {
                return Err(DecodeError::Invalid {
                    kind: "engine manifest",
                    reason: "invalid parent option",
                });
            }
        };
        let state: StateHeader = d.get()?;
        let state_root = d.get()?;
        if (sequence == 0 && parent.is_some())
            || (sequence > 0 && parent.map(|p| p.0) != Some(sequence - 1))
            || state.root() != state_root
        {
            return Err(DecodeError::Invalid {
                kind: "engine manifest",
                reason: "parent or state root mismatch",
            });
        }
        Ok(Self {
            genesis,
            sequence,
            parent,
            state,
            state_root,
        })
    }
}

pub(crate) fn decode_error(e: DecodeError) -> DurableError {
    match e {
        DecodeError::UnsupportedVersion { .. } => incompatible(e.to_string()),
        _ => corrupt(e.to_string()),
    }
}

pub(crate) fn profile_bytes() -> Vec<u8> {
    [PROFILE.as_slice(), &1u32.to_be_bytes()].concat()
}
