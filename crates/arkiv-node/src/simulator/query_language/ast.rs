// SPDX-License-Identifier: GPL-3.0-or-later
// Adapted from Arkiv-Network/arkiv at 0fd58ce5bd6a0a196b0079f60d4679539346adc6.
//! Minimal local AST adapter for the imported Arkiv grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// Matches every live entity (`*`).
    All,
    /// Matches entities whose `key` equals `value`.
    Eq { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is greater than `value`.
    Gt { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is greater than or equal to `value`.
    Gte { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is less than `value`.
    Lt { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` is less than or equal to `value`.
    Lte { key: AnnotKey, value: AnnotVal },
    /// Matches entities whose `key` starts with `value` — the `STARTSWITH`
    /// operator, a raw UTF-8 byte prefix with no normalization.
    StartsWith { key: AnnotKey, value: AnnotVal },
    /// Matches entities satisfying both subqueries.
    And(Box<Query>, Box<Query>),
    /// Matches entities satisfying either subquery.
    Or(Box<Query>, Box<Query>),
    /// Matches entities not satisfying the subquery — the full complement,
    /// evaluated against the live-entity set.
    Not(Box<Query>),
}

/// What a predicate matches on: a built-in field, or a user attribute by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotKey {
    BuiltIn(BuiltIn),
    User(String),
}

/// The built-in (`$`-prefixed) fields you can query by name.
///
/// These are the *language's* names. The byte strings they are indexed under are
/// the `annotations` constants, which a host maps
/// them to — the two are deliberately decoupled, because the annotation bytes are
/// mixed into index-bucket addresses and renaming one relocates on-chain state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltIn {
    /// `$owner` — the current owner's address.
    Owner,
    /// `$creator` — the creating address, immutable.
    Creator,
    /// `$key` — the entity key.
    Key,
    /// `$expiresAt` — the expiry block.
    ExpiresAt,
    /// `$createdAt` — the creation block.
    CreatedAt,
}

/// A value a predicate compares against.
///
/// It is exactly the stored [`AttributeValue`], so a predicate and the attribute it
/// matches speak one type system: the query's bytes are derived the same way the
/// writer derived the index's (`AttributeValue::index_bytes`), and a value of one
/// type never matches an attribute of another.
pub type AnnotVal = AttributeValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeType {
    Bool,
    Int,
    U64,
    U256,
    Decimal,
    Bytes32,
    Str,
    EthereumAddress,
    EntityKey,
}
impl AttributeType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "i32",
            Self::U64 => "u64",
            Self::U256 => "u256",
            Self::Decimal => "dec",
            Self::Bytes32 => "bytes32",
            Self::Str => "str",
            Self::EthereumAddress => "addr",
            Self::EntityKey => "key",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeValue {
    Bool(bool),
    Int(i32),
    U64(u64),
    U256([u8; 32]),
    Decimal([u8; 32]),
    Bytes32([u8; 32]),
    Str(String),
    EthereumAddress([u8; 20]),
    EntityKey([u8; 32]),
}
impl AttributeValue {
    pub fn attr_type(&self) -> AttributeType {
        match self {
            Self::Bool(..) => AttributeType::Bool,
            Self::Int(..) => AttributeType::Int,
            Self::U64(..) => AttributeType::U64,
            Self::U256(..) => AttributeType::U256,
            Self::Decimal(..) => AttributeType::Decimal,
            Self::Bytes32(..) => AttributeType::Bytes32,
            Self::Str(..) => AttributeType::Str,
            Self::EthereumAddress(..) => AttributeType::EthereumAddress,
            Self::EntityKey(..) => AttributeType::EntityKey,
        }
    }
    pub fn u256_from_u64(n: u64) -> Self {
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&n.to_be_bytes());
        Self::U256(bytes)
    }
}
