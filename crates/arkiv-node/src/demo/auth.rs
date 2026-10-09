//! Pinned single-producer development authentication; not consensus.
use super::{DemoError, Result};
use crate::{ChainSpec, Genesis, Header, validate};
use arkiv_db::{B256, Decode, DecodeError, Decoder, Encode, Encoder, decode_exact, digest, encode};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustConfig {
    pub genesis: Genesis,
    pub genesis_hash: B256,
    pub public_key: [u8; 32],
}

fn version(d: &mut Decoder<'_>, kind: &'static str) -> std::result::Result<(), DecodeError> {
    let version = d.u32()?;
    if version != 1 {
        return Err(DecodeError::UnsupportedVersion { kind, version });
    }
    Ok(())
}
impl Encode for TrustConfig {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(1);
        e.bytes(&encode(&self.genesis));
        e.put(&self.genesis_hash);
        e.fixed(&self.public_key);
    }
}
impl Decode for TrustConfig {
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        version(d, "TrustConfigV1")?;
        Ok(Self {
            genesis: decode_exact(d.bytes()?)?,
            genesis_hash: d.get()?,
            public_key: d.get()?,
        })
    }
}
impl TrustConfig {
    pub fn validate(&self) -> Result<ChainSpec> {
        let spec = ChainSpec::new(self.genesis.clone()).map_err(|_| DemoError::InvalidHeader)?;
        if spec.genesis_hash() != self.genesis_hash {
            return Err(DemoError::InvalidHeader);
        }
        VerifyingKey::from_bytes(&self.public_key).map_err(|_| DemoError::InvalidHeader)?;
        Ok(spec)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHeader {
    pub genesis_hash: B256,
    pub header: Header,
    pub signature: [u8; 64],
}
impl Encode for SignedHeader {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(1);
        e.put(&self.genesis_hash);
        e.put(&self.header);
        e.fixed(&self.signature);
    }
}
impl Decode for SignedHeader {
    const MIN_ENCODED_LEN: usize = 4 + 32 + 156 + 64;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        version(d, "SignedHeaderV1")?;
        Ok(Self {
            genesis_hash: d.get()?,
            header: d.get()?,
            signature: d.get()?,
        })
    }
}
pub fn signing_message(genesis_hash: B256, header: &Header) -> B256 {
    let mut e = Encoder::new();
    e.u32(1);
    e.put(&genesis_hash);
    e.put(&header.hash());
    digest("arkiv/demo-sign/v1", &e.into_bytes())
}
impl SignedHeader {
    pub fn sign(genesis_hash: B256, header: Header, key: &SigningKey) -> Self {
        Self {
            genesis_hash,
            header,
            signature: key
                .sign(signing_message(genesis_hash, &header).as_slice())
                .to_bytes(),
        }
    }
    pub fn authenticate(&self, trust: &TrustConfig) -> Result<()> {
        if self.genesis_hash != trust.genesis_hash || self.header.chain_id != trust.genesis.chain_id
        {
            return Err(DemoError::InvalidHeader);
        }
        let key =
            VerifyingKey::from_bytes(&trust.public_key).map_err(|_| DemoError::InvalidHeader)?;
        key.verify_strict(
            signing_message(self.genesis_hash, &self.header).as_slice(),
            &Signature::from_bytes(&self.signature),
        )
        .map_err(|_| DemoError::InvalidHeader)
    }
}

/// Headers only: no engine, executor, body cache or secret key.
#[derive(Debug)]
pub struct HeaderChain {
    trust: TrustConfig,
    spec: ChainSpec,
    headers: Vec<SignedHeader>,
    conflicts: Vec<SignedHeader>,
    capacity: usize,
}
impl HeaderChain {
    pub fn new(trust: TrustConfig, genesis: SignedHeader, capacity: usize) -> Result<Self> {
        let spec = trust.validate()?;
        genesis.authenticate(&trust)?;
        if genesis.header != *spec.genesis_header() || capacity == 0 {
            return Err(DemoError::InvalidHeader);
        }
        Ok(Self {
            trust,
            spec,
            headers: vec![genesis],
            conflicts: Vec::new(),
            capacity,
        })
    }
    pub fn trust(&self) -> &TrustConfig {
        &self.trust
    }
    pub fn head(&self) -> &SignedHeader {
        self.headers.last().expect("genesis")
    }
    pub fn at(&self, height: u64) -> Option<&SignedHeader> {
        self.headers.get(usize::try_from(height).ok()?)
    }
    pub fn conflicts(&self) -> &[SignedHeader] {
        &self.conflicts
    }
    /// Authentication and continuity are checked without claiming execution.
    pub fn accept(&mut self, signed: SignedHeader) -> Result<bool> {
        if !self.conflicts.is_empty() {
            return Err(DemoError::ChainConflict);
        }
        signed.authenticate(&self.trust)?;
        if let Some(existing) = self.at(signed.header.height) {
            if existing.header == signed.header {
                return Ok(false);
            }
            self.conflicts = vec![existing.clone(), signed];
            return Err(DemoError::ChainConflict);
        }
        if self.head().header.height.checked_add(1) != Some(signed.header.height) {
            return Err(DemoError::InvalidHeader);
        }
        if signed.header.parent_hash != self.head().header.hash() {
            self.conflicts = vec![self.head().clone(), signed];
            return Err(DemoError::ChainConflict);
        }
        validate::check_header(&self.spec, &self.head().header, &signed.header)
            .map_err(|_| DemoError::InvalidHeader)?;
        if self.headers.len() >= self.capacity {
            return Err(DemoError::CapacityReached);
        }
        self.headers.push(signed);
        Ok(true)
    }
}
