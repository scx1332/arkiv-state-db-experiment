//! Project namespaces: immutable identities and protected owner metadata.
//!
//! A namespace is the isolation boundary for Arkiv's workload. Its identity
//! never changes; its owner and revision change only through dedicated,
//! host-authorized transitions. Records of different namespaces live in
//! separate maps (the catalog leaf of each namespace holds the roots of its
//! own raw, key, row and term maps; see `layout::NamespaceState`), so
//! identical keys never alias and index structures are sibling fields
//! rather than entries in the record keyspace.

use std::fmt;

use crate::record::{CellName, NameFault};

/// A namespace's immutable readable name, using the cell-name ASCII grammar.
/// Registration, uniqueness and name-to-ID resolution belong to the host;
/// all engine addresses continue to use the stable [`NamespaceId`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespaceName {
    bytes: [u8; Self::MAX_LEN],
    len: u8,
}

impl NamespaceName {
    /// Maximum number of ASCII characters (and bytes).
    pub const MAX_LEN: usize = 20;

    /// Validates a nonempty, case-sensitive name before it reaches the engine.
    pub fn new(name: &str) -> Result<Self, NameFault> {
        if name.len() > Self::MAX_LEN {
            return Err(NameFault::TooLong);
        }
        CellName::validate(name)?;
        let mut bytes = [0; Self::MAX_LEN];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self {
            bytes,
            len: name.len() as u8,
        })
    }

    /// The validated name text.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("namespace names are validated ASCII")
    }
}

impl fmt::Debug for NamespaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("NamespaceName")
            .field(&self.as_str())
            .finish()
    }
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Immutable identity of a namespace.
///
/// IDs are ordinals allocated from committed engine state: the *n*-th
/// namespace creation published on a chain receives ordinal *n*, starting at 1.
/// The allocator is part of the rolled-back state, so a creation inside a
/// rolled-back transaction or a discarded candidate releases its ordinal; a
/// published ordinal is never reassigned. IDs carry no owner or project name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespaceId(u64);

impl NamespaceId {
    /// Byte width of the canonical encoding.
    pub const ENCODED_LEN: usize = 8;

    /// Names the namespace with allocation ordinal `ordinal`; zero is invalid.
    ///
    /// This only spells an identity. Whether the namespace exists is decided
    /// by the selected snapshot or branch state.
    pub const fn from_ordinal(ordinal: u64) -> Option<Self> {
        if ordinal == 0 {
            None
        } else {
            Some(Self(ordinal))
        }
    }

    /// Returns the allocation ordinal (never zero).
    pub const fn ordinal(self) -> u64 {
        self.0
    }

    /// Canonical fixed-width encoding: the ordinal as big-endian bytes.
    ///
    /// Big-endian keeps byte order equal to allocation order wherever IDs are
    /// used as key prefixes.
    pub const fn to_bytes(self) -> [u8; Self::ENCODED_LEN] {
        self.0.to_be_bytes()
    }

    /// Decodes the canonical encoding; zero is not a valid ID.
    pub const fn from_bytes(bytes: [u8; Self::ENCODED_LEN]) -> Option<Self> {
        Self::from_ordinal(u64::from_be_bytes(bytes))
    }
}

impl fmt::Debug for NamespaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "NamespaceId({})", self.0)
    }
}

impl fmt::Display for NamespaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ns:{:016x}", self.0)
    }
}

/// A host-authenticated principal, such as an account address.
///
/// The engine never interprets or verifies these bytes. The host validates
/// signatures and passes the authenticated actor of each request; the engine
/// only compares it with the owner recorded in the namespace descriptor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerId([u8; 20]);

impl OwnerId {
    /// Wraps raw principal bytes.
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Returns the raw principal bytes.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Debug for OwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "OwnerId(0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        write!(formatter, ")")
    }
}

/// Number of successful ownership transfers since creation.
///
/// A transfer must name the current revision; this rejects stale requests even
/// when the host has already authorized them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

impl Revision {
    /// The revision of a freshly created namespace.
    pub const INITIAL: Self = Self(0);
}

/// Protected namespace metadata.
///
/// Descriptors are never stored under record keys; only the dedicated
/// namespace operations modify them, and every retained snapshot keeps the
/// descriptor that was current at that snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceDescriptor {
    /// Immutable identity.
    pub id: NamespaceId,
    /// Readable name assigned at creation, preserved by ownership transfers.
    pub name: NamespaceName,
    /// Current owner as authenticated by the host.
    pub owner: OwnerId,
    /// Number of transfers applied so far.
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_encode_as_big_endian_ordinals_and_reject_zero() {
        let id = NamespaceId::from_ordinal(258).unwrap();
        assert_eq!(id.ordinal(), 258);
        assert_eq!(id.to_bytes(), [0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(NamespaceId::from_bytes([0, 0, 0, 0, 0, 0, 1, 2]), Some(id));
        assert_eq!(NamespaceId::from_bytes([0; 8]), None);
        assert_eq!(NamespaceId::from_ordinal(0), None);
        assert_eq!(id.to_string(), "ns:0000000000000102");
        assert_eq!(format!("{id:?}"), "NamespaceId(258)");
        let later = NamespaceId::from_ordinal(259).unwrap();
        assert!(id < later);
        assert!(id.to_bytes() < later.to_bytes());
    }

    #[test]
    fn owner_ids_are_opaque_bytes() {
        let owner = OwnerId::new([0xab; 20]);
        assert_eq!(owner.as_bytes(), &[0xab; 20]);
        assert!(format!("{owner:?}").starts_with("OwnerId(0xabab"));
        assert_ne!(owner, OwnerId::new([0xac; 20]));
    }
}
