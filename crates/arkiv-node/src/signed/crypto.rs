//! Strict secp256k1 prehash signing for the native signed profile.
use alloy_primitives::{Address, B256, Signature};
use arkiv_db::OwnerId;
use k256::ecdsa::{Signature as ScalarSignature, SigningKey};

use super::{Error, Result};

/// A local signing key. Debug output deliberately contains only its address.
pub struct Signer(SigningKey);

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("address", &self.address())
            .finish()
    }
}

impl Signer {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        SigningKey::from_slice(&bytes)
            .map(Self)
            .map_err(|_| Error("InvalidSigningKey"))
    }

    pub fn address(&self) -> OwnerId {
        OwnerId::new(Address::from_public_key(self.0.verifying_key()).into_array())
    }

    /// RFC6979 signing of the already computed digest, without EIP-191 framing.
    pub fn sign_digest(&self, digest: B256) -> Result<[u8; 65]> {
        let (signature, recovery) = self
            .0
            .sign_prehash_recoverable(digest.as_slice())
            .map_err(|_| Error("SigningFailed"))?;
        // The wire format has only y parity; an x-reduced recovery id is not representable.
        if recovery.is_x_reduced() {
            return Err(Error("SigningFailed"));
        }
        let bytes = Signature::from_signature_and_parity(signature, recovery.is_y_odd()).as_rsy();
        if recover(digest, &bytes)? != self.address() {
            return Err(Error("SigningFailed"));
        }
        Ok(bytes)
    }
}

/// Reject noncanonical parity, invalid scalars and high-S before recovery.
/// Alloy recovery normalizes high-S internally, so recovery alone is insufficient.
pub fn validate_signature(bytes: &[u8; 65]) -> Result<Signature> {
    if bytes[64] > 1 {
        return Err(Error("InvalidSignature"));
    }
    ScalarSignature::from_slice(&bytes[..64]).map_err(|_| Error("InvalidSignature"))?;
    let signature = Signature::from_bytes_and_parity(&bytes[..64], bytes[64] == 1);
    if signature.normalize_s().is_some() {
        return Err(Error("InvalidSignature"));
    }
    Ok(signature)
}

pub fn recover(digest: B256, bytes: &[u8; 65]) -> Result<OwnerId> {
    validate_signature(bytes)?
        .recover_address_from_prehash(&digest)
        .map(|address| OwnerId::new(address.into_array()))
        .map_err(|_| Error("InvalidSignature"))
}
