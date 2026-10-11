//! The key and leaf encodings of the engine's trie maps (state format 2).
//!
//! Every map is an [`arkiv_db_trie::MerkleMap`] with bounded, prefix-free
//! keys; it orders entries by the unsigned bytewise order of the key
//! encodings, so each key encoding below preserves the order the engine
//! relies on, and no encoding is a strict prefix of another of its map (a
//! trie branch holds no value). Leaf values are canonical-codec encodings
//! (codec version [`CODEC_VERSION`](crate::CODEC_VERSION)) and never empty.
//!
//! `bytes⁰` is [`order::encode_bytes`]: `00 → 01 01`, `01 → 01 02`, then a
//! `00` terminator. `text⁺` is [`order::encode_shifted`]: every UTF-8 byte
//! plus one, then a `00` terminator (exactly `len + 1` bytes; UTF-8 never
//! holds `ff`). Both are prefix-free and order as the strings do.
//!
//! | Map | Key | Bytes | Leaf value |
//! | --- | --- | --- | --- |
//! | catalog | [`NamespaceId`] big-endian | 8 | [`NamespaceState`]: `descriptor ‖ next_record u64 ‖ records ‖ keys ‖ rows ‖ terms`, each map as `root ‖ len u64` |
//! | host | raw `[u8; 32]` | 32 | [`Blob`]: `u32 len ‖ bytes` |
//! | records (raw) | [`RawKey`]: `bytes⁰(key)` | 1–41 | [`Blob`]: `u32 len ‖ bytes` |
//! | keys | [`RecordKey`]: `len ‖ bytes` | 2–33 | [`RecordId`]: `u64` (nonzero) |
//! | rows | [`RecordId`] big-endian | 8 | [`StoredRecord`]: `RecordKey ‖ Vec<(CellName, Cell)>` sorted by name |
//! | terms | [`TermKey`]: `bytes⁰(name) ‖ tag ‖ 00` (existence) or `bytes⁰(name) ‖ tag ‖ 01 ‖ value` | 4–[`TermKey::MAX_LEN`] | [`Posting`]: `posting root ‖ posting len u64` (len ≥ 1) |
//! | postings | [`RecordId`] big-endian | 8 | [`Member`]: `01` |
//!
//! A term's `value` is `text⁺(utf8)` for `Str` and the type's fixed-width
//! order-preserving term bytes ([`Value::term_bytes`]) for every other
//! indexable type. Names never contain `00` or `01` (the name grammar), so
//! `bytes⁰(name)` is the name and a `00`.
//!
//! A nested map is written as its root hash and entry count; `(EMPTY_ROOT_HASH,
//! 0)` is the empty map and every other pairing of the empty root or a zero
//! count is corrupt. The catalog's nested maps belong to the namespace's
//! object space, not the catalog's, so [`NamespaceState`] stages nothing
//! itself: whoever stages the catalog stages them into the namespace's space
//! (see the engine's staging walk). A term leaf's posting map shares the
//! namespace's space and is staged through [`TrieValue::stage_nested`].
//!
//! Decoding checks everything a leaf alone can check (codec rules, name
//! grammar, sorted unique cells, attributes indexable and at most 128 bytes,
//! nonzero ids, map handles, the key and row counts agreeing, the allocator
//! ahead of the rows); what depends on the key the leaf sits under (a
//! catalog leaf's descriptor id, a row's key) is checked where the leaf is
//! read.

use std::sync::Arc;

use arkiv_db_trie::{
    CodecError, EMPTY_ROOT_HASH, MerkleMap, ObjectSink, TrieError, TrieKey, TrieValue,
    order::{decode_bytes, decode_shifted, encode_bytes, encode_shifted, encoded_bytes_max},
};

use crate::{
    codec::{B256, Decode, DecodeError, Decoder, Encode, Encoder},
    engine::HostKey,
    namespace::{NamespaceDescriptor, NamespaceId},
    record::{Cell, CellName, Limits, RecordId, RecordKey, TypeTag, Value},
    typed::TypedState,
};

/// The namespace catalog: `NamespaceId → NamespaceState`.
pub(crate) type Catalog = MerkleMap<NamespaceId, NamespaceState>;
/// The host-state map.
pub(crate) type HostMap = MerkleMap<HostKey, Blob>;
/// A namespace's raw byte records.
pub(crate) type Records = MerkleMap<RawKey, Blob>;
/// A namespace's key bindings.
pub(crate) type Keys = MerkleMap<RecordKey, RecordId>;
/// A namespace's live rows.
pub(crate) type Rows = MerkleMap<RecordId, StoredRecord>;
/// A namespace's term index.
pub(crate) type Terms = MerkleMap<TermKey, Posting>;
/// The ordered set of record ids holding one term.
pub(crate) type PostingMap = MerkleMap<RecordId, Member>;

fn codec_error(error: DecodeError) -> CodecError {
    CodecError::new(error.to_string())
}

/// Decodes a codec value that must span all of `bytes`.
fn decode_leaf<T: Decode>(bytes: &[u8]) -> Result<T, CodecError> {
    crate::codec::decode_exact(bytes).map_err(codec_error)
}

fn encode_leaf<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    crate::codec::encode(value)
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The ordinal as 8 big-endian bytes, so the catalog iterates in id order.
impl TrieKey for NamespaceId {
    const MAX_LEN: usize = Self::ENCODED_LEN;

    fn encode(&self) -> Vec<u8> {
        self.to_bytes().to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let bytes = bytes
            .try_into()
            .map_err(|_| CodecError::new("namespace key is not 8 bytes"))?;
        Self::from_bytes(bytes).ok_or_else(|| CodecError::new("namespace key of zero"))
    }
}

/// The ordinal as 8 big-endian bytes, so rows and postings iterate in id
/// order.
impl TrieKey for RecordId {
    const MAX_LEN: usize = 8;

    fn encode(&self) -> Vec<u8> {
        self.ordinal().to_be_bytes().to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let bytes: [u8; 8] = bytes
            .try_into()
            .map_err(|_| CodecError::new("record id key is not 8 bytes"))?;
        Self::from_ordinal(u64::from_be_bytes(bytes))
            .ok_or_else(|| CodecError::new("record id key of zero"))
    }
}

/// `len ‖ bytes` (2 to 33 bytes): prefix-free, since the length byte fixes
/// the width. The key map is only read by point lookups, so its order (by
/// length first) is never observed.
impl TrieKey for RecordKey {
    const MAX_LEN: usize = 1 + RecordKey::MAX_LEN;

    fn encode(&self) -> Vec<u8> {
        let bytes = self.as_bytes();
        let mut out = Vec::with_capacity(1 + bytes.len());
        out.push(u8::try_from(bytes.len()).expect("record keys are at most 32 bytes"));
        out.extend_from_slice(bytes);
        out
    }

    /// Rejects a length byte of 0 or above 32 and a length disagreeing with
    /// the bytes that follow.
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (&len, key) = bytes
            .split_first()
            .ok_or_else(|| CodecError::new("record key is empty"))?;
        if usize::from(len) != key.len() {
            return Err(CodecError::new("record key length byte disagrees"));
        }
        RecordKey::new(key).ok_or_else(|| CodecError::new("record key length out of range"))
    }
}

/// A raw record key of at most [`Limits::MAX_RAW_KEY_LEN`] bytes, empty
/// included, keyed as `bytes⁰(key)` ([`order::encode_bytes`], 1 to 41
/// bytes).
///
/// The encoding orders exactly as the keys' bytewise order, and the encoding
/// of a prefix is at most the encoding of every key that starts with it and
/// above every smaller key, so a prefix scan seeks to the encoding of the
/// prefix itself.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RawKey(pub(crate) Arc<[u8]>);

impl TrieKey for RawKey {
    const MAX_LEN: usize = encoded_bytes_max(Limits::MAX_RAW_KEY_LEN);

    /// # Panics
    ///
    /// If the key is longer than [`Limits::MAX_RAW_KEY_LEN`], which
    /// admission rejects before any key reaches a map.
    fn encode(&self) -> Vec<u8> {
        assert!(
            self.0.len() <= Limits::MAX_RAW_KEY_LEN,
            "admission bounds raw keys and prefixes by 20 bytes"
        );
        let mut out = Vec::with_capacity(self.0.len() + 1);
        encode_bytes(&self.0, &mut out);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (key, rest) = decode_bytes(bytes, Limits::MAX_RAW_KEY_LEN)?;
        if !rest.is_empty() {
            return Err(CodecError::new("trailing bytes after a raw key"));
        }
        Ok(Self(Arc::from(key)))
    }
}

/// Whether a term is a value term or the existence term of its
/// `(name, type)`; the presence byte of the term key.
const PRESENCE_EXISTENCE: u8 = 0;
const PRESENCE_VALUE: u8 = 1;

/// The widest fixed-width term value (`U256`, `I256`, `Dec`, `Bytes32`,
/// `Key`).
const WIDEST_SCALAR_TERM: usize = 32;

/// One index term: a value term (`value = Some`, its term bytes) or the
/// existence term of a `(name, type)` (`value = None`).
///
/// Keyed as `bytes⁰(name) ‖ tag ‖ 00` (existence) or `bytes⁰(name) ‖ tag ‖
/// 01 ‖ value` (value term), where `value` is `text⁺(utf8)` for `Str` and the
/// fixed-width term bytes of every other type. Within one `(name, tag)` the
/// value parts are prefix-free (one fixed width per scalar type; `text⁺` is
/// prefix-free), the name encoding is prefix-free, and existence and value
/// keys differ at the presence byte, so the whole key set is prefix-free.
/// Every part preserves order, so terms order by name, then type id, then
/// existence before every value, then value bytes: every `(name, type)` is
/// one contiguous run in value order, as the derived order of this
/// struct's fields is.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TermKey {
    pub(crate) name: CellName,
    pub(crate) tag: TypeTag,
    pub(crate) value: Option<Arc<[u8]>>,
}

impl TermKey {
    /// The existence term of `(name, tag)`.
    pub(crate) fn existence(name: &CellName, tag: TypeTag) -> Self {
        Self {
            name: name.clone(),
            tag,
            value: None,
        }
    }

    /// The value term of an indexable value.
    pub(crate) fn value(name: &CellName, value: &Value) -> Self {
        Self {
            name: name.clone(),
            tag: value.tag(),
            value: Some(Arc::from(
                value.term_bytes().expect("attribute values are indexable"),
            )),
        }
    }

    /// The value term with raw term `bytes`: a seek position inside the
    /// `(name, tag)` run. For `Str` it sorts at or before every value that
    /// starts with `bytes`; for a scalar type, empty `bytes` is the start of
    /// the run.
    pub(crate) fn run_start(name: &CellName, tag: TypeTag, bytes: &[u8]) -> Self {
        Self {
            name: name.clone(),
            tag,
            value: Some(Arc::from(bytes)),
        }
    }

    /// `bytes⁰(name) ‖ tag ‖ 01`: the prefix shared by every value term of
    /// `(name, tag)` and by no other key. The run of value terms is
    /// `[run, prefix_successor(run))`.
    pub(crate) fn value_run(name: &CellName, tag: TypeTag) -> Vec<u8> {
        let mut out = Vec::with_capacity(name.as_str().len() + 3);
        encode_bytes(name.as_str().as_bytes(), &mut out);
        out.extend([tag.id(), PRESENCE_VALUE]);
        out
    }

    /// The run of `Str` value terms of `(name, Str)` whose value starts with
    /// `prefix`: `[run ‖ shift(prefix), prefix_successor(run ‖ shift(prefix)))`.
    /// Prefix reads seek to `text⁺(prefix)`, the run's smallest key, and stop
    /// at the first value without the prefix; this states the run exactly.
    #[cfg(test)]
    pub(crate) fn str_prefix_run(name: &CellName, prefix: &[u8]) -> arkiv_db_trie::KeyRange {
        let mut start = Self::value_run(name, TypeTag::Str);
        arkiv_db_trie::order::shift_bytes(prefix, &mut start).expect("UTF-8 never holds ff");
        arkiv_db_trie::KeyRange::prefix(&start)
    }

    /// The longest term key under `limits`: `bytes⁰(name)`, tag, presence
    /// and the longer of a `text⁺` value at the indexed-string limit and the
    /// widest scalar. Bounds proof-walk depth and reservations for one
    /// instance; [`TrieKey::MAX_LEN`] is this at the hard caps.
    pub(crate) const fn max_len_for(limits: &Limits) -> usize {
        let value = limits.indexed_value_len() + 1;
        let value = if value > WIDEST_SCALAR_TERM {
            value
        } else {
            WIDEST_SCALAR_TERM
        };
        // Names never contain 00 or 01, so `bytes⁰(name)` is `len + 1`.
        (limits.max_cell_name_len + 1) + 2 + value
    }
}

impl TrieKey for TermKey {
    /// At the hard caps: a 32-byte name (33 encoded), tag, presence and a
    /// `Str` value at [`Value::INDEXED_LEN_CEILING`] (plus its terminator).
    const MAX_LEN: usize = (CellName::MAX_LEN + 1) + 2 + Value::INDEXED_LEN_CEILING + 1;

    /// # Panics
    ///
    /// If the name exceeds 32 bytes or a `Str` value the indexed-string
    /// ceiling, which name validation and admission rule out before any term
    /// is built.
    fn encode(&self) -> Vec<u8> {
        let name = self.name.as_str().as_bytes();
        let value_len = self.value.as_ref().map_or(0, |value| value.len() + 1);
        let mut key = Vec::with_capacity(name.len() + 3 + value_len);
        encode_bytes(name, &mut key);
        key.push(self.tag.id());
        match &self.value {
            None => key.push(PRESENCE_EXISTENCE),
            Some(bytes) => {
                key.push(PRESENCE_VALUE);
                if self.tag == TypeTag::Str {
                    assert!(
                        bytes.len() <= Value::INDEXED_LEN_CEILING,
                        "indexed values and query bounds are within the ceiling"
                    );
                    encode_shifted(bytes, &mut key).expect("UTF-8 never holds ff");
                } else {
                    key.extend_from_slice(bytes);
                }
            }
        }
        key
    }

    /// Rejects a name outside the grammar, an unknown or unindexable type,
    /// a presence byte other than `00`/`01`, an existence term with value
    /// bytes, value bytes that are not the term bytes of the type, a `Str`
    /// value over the ceiling or without its terminator, and trailing bytes.
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (name, rest) = decode_bytes(bytes, CellName::MAX_LEN)?;
        let name = std::str::from_utf8(&name)
            .ok()
            .and_then(|name| CellName::new(name).ok())
            .ok_or_else(|| CodecError::new("term name is not a valid cell name"))?;
        let (&tag_id, rest) = rest
            .split_first()
            .ok_or_else(|| CodecError::new("term key has no type"))?;
        let tag = TypeTag::ALL
            .into_iter()
            .find(|tag| tag.id() == tag_id && tag.is_indexable())
            .ok_or_else(|| CodecError::new("term type is not an indexable type id"))?;
        let (&presence, value) = rest
            .split_first()
            .ok_or_else(|| CodecError::new("term key has no presence byte"))?;
        let value = match presence {
            PRESENCE_EXISTENCE if value.is_empty() => None,
            PRESENCE_VALUE => {
                let term: Vec<u8> = if tag == TypeTag::Str {
                    let (text, trailing) = decode_shifted(value, Value::INDEXED_LEN_CEILING)?;
                    if !trailing.is_empty() {
                        return Err(CodecError::new("trailing bytes after a term value"));
                    }
                    text
                } else {
                    value.to_vec()
                };
                if Value::from_term_bytes(tag, &term).is_none() {
                    return Err(CodecError::new("term value is not the type's term bytes"));
                }
                Some(Arc::from(term))
            }
            _ => return Err(CodecError::new("term presence and value disagree")),
        };
        Ok(Self { name, tag, value })
    }
}

// ---------------------------------------------------------------------------
// Leaf values
// ---------------------------------------------------------------------------

/// A byte string leaf: raw record values and host values, `u32 len ‖
/// bytes`, so an empty value is still a nonempty leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Blob(pub(crate) Arc<[u8]>);

impl TrieValue for Blob {
    fn encode(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.bytes(&self.0);
        encoder.into_bytes()
    }

    /// Never: the length prefix is 4 bytes.
    fn encodes_empty(&self) -> bool {
        false
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut decoder = Decoder::new(bytes);
        let value = decoder.bytes().map_err(codec_error)?;
        decoder.finish().map_err(codec_error)?;
        Ok(Self(Arc::from(value)))
    }

    fn heap_bytes(&self) -> usize {
        arc_bytes(self.0.len())
    }
}

/// Heap bytes of an `Arc<[T]>`/`Arc<str>` holding `payload` bytes: the two
/// reference counts plus the payload.
const fn arc_bytes(payload: usize) -> usize {
    2 * std::mem::size_of::<usize>() + payload
}

/// A key binding's value: the record id, `u64` (nonzero).
impl TrieValue for RecordId {
    fn encode(&self) -> Vec<u8> {
        encode_leaf(self)
    }

    /// Never: 8 bytes.
    fn encodes_empty(&self) -> bool {
        false
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_leaf(bytes)
    }
}

/// A posting entry's value: one `01` byte (the key is the whole content).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Member;

impl TrieValue for Member {
    fn encode(&self) -> Vec<u8> {
        vec![1]
    }

    fn encodes_empty(&self) -> bool {
        false
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        match bytes {
            [1] => Ok(Self),
            _ => Err(CodecError::new("a posting entry's value is not 01")),
        }
    }
}

/// Writes a nested map handle: `root ‖ len u64`.
fn put_handle<K: TrieKey, V: TrieValue>(encoder: &mut Encoder, map: &MerkleMap<K, V>) {
    encoder.put(&map.root_hash());
    encoder.u64(map.len());
}

/// Reads a nested map handle; the pairing must describe a map.
fn get_handle<K: TrieKey, V: TrieValue>(
    decoder: &mut Decoder<'_>,
) -> Result<MerkleMap<K, V>, CodecError> {
    let root: B256 = decoder.get().map_err(codec_error)?;
    let len = decoder.u64().map_err(codec_error)?;
    MerkleMap::from_root(root, len).map_err(|error: TrieError| CodecError::new(error.to_string()))
}

/// A term leaf's value: the term's posting map, which is never empty.
#[derive(Clone, Debug)]
pub(crate) struct Posting(pub(crate) PostingMap);

impl TrieValue for Posting {
    fn encode(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        put_handle(&mut encoder, &self.0);
        encoder.into_bytes()
    }

    /// Never: 40 bytes. Answered without encoding, so inserting a term
    /// leaf does not hash its posting map (hashing waits for a root).
    fn encodes_empty(&self) -> bool {
        false
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut decoder = Decoder::new(bytes);
        let map = get_handle(&mut decoder)?;
        decoder.finish().map_err(codec_error)?;
        if map.is_empty() {
            return Err(CodecError::new("a term leaf holds an empty posting"));
        }
        Ok(Self(map))
    }

    /// The posting map lives in the namespace's space, like the term map.
    fn stage_nested(&self, sink: &mut dyn ObjectSink) {
        self.0.stage(sink);
    }
}

/// A live record as stored: its key and its cells sorted by name.
///
/// Cloning is O(1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredRecord {
    pub(crate) key: RecordKey,
    pub(crate) cells: Arc<[(CellName, Cell)]>,
}

/// `RecordKey (u8 len ‖ bytes) ‖ Vec<(CellName, Cell)>` with the cells in
/// strictly ascending name order and every attribute indexable: a type
/// other than `Bytes` and at most [`Value::INDEXED_LEN_CEILING`] encoded
/// bytes, as admission guarantees for every stored row (the instance's own
/// indexed-string limit is at most that ceiling).
impl TrieValue for StoredRecord {
    fn encode(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.put(&self.key);
        encoder.put(&self.cells[..]);
        encoder.into_bytes()
    }

    /// Never: at least the key and the cell count.
    fn encodes_empty(&self) -> bool {
        false
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut decoder = Decoder::new(bytes);
        let key: RecordKey = decoder.get().map_err(codec_error)?;
        let cells: Vec<(CellName, Cell)> = decoder.get().map_err(codec_error)?;
        decoder.finish().map_err(codec_error)?;
        if cells.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(CodecError::new(
                "row cells are not in strictly ascending name order",
            ));
        }
        // Index maintenance builds the terms of every attribute, so a row
        // whose attribute has no term is corrupt, not a panic later.
        if cells.iter().any(|(_, cell)| {
            cell.is_attribute()
                && (!cell.value.tag().is_indexable()
                    || cell.value.encoded_len() > Value::INDEXED_LEN_CEILING)
        }) {
            return Err(CodecError::new(
                "a row attribute is not indexable or longer than the indexed ceiling",
            ));
        }
        Ok(Self {
            key,
            cells: Arc::from(cells),
        })
    }

    /// The key, the cell array and every name, string and byte payload.
    fn heap_bytes(&self) -> usize {
        let cells = self.cells.iter().map(|(name, cell)| {
            arc_bytes(name.as_str().len())
                + match &cell.value {
                    Value::Str(text) => arc_bytes(text.len()),
                    Value::Bytes(bytes) => arc_bytes(bytes.len()),
                    _ => 0,
                }
        });
        arc_bytes(self.key.as_bytes().len())
            + arc_bytes(std::mem::size_of_val::<[(CellName, Cell)]>(&self.cells))
            + cells.sum::<usize>()
    }
}

/// Everything the engine keeps for one namespace at one state: the catalog
/// leaf (`NamespaceLeaf` in the documents).
///
/// Cloning is O(1): the descriptor is copied and every map is shared. The
/// raw byte keyspace (`records`) and the typed record store with its
/// indexes (`typed`) are sibling fields; neither is reachable through the
/// other.
#[derive(Clone, Debug)]
pub(crate) struct NamespaceState {
    pub(crate) descriptor: NamespaceDescriptor,
    pub(crate) records: Records,
    pub(crate) typed: TypedState,
}

/// `descriptor ‖ next_record u64 ‖ records ‖ keys ‖ rows ‖ terms`, each map
/// as `root ‖ len u64`.
impl TrieValue for NamespaceState {
    fn encode(&self) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.put(&self.descriptor);
        encoder.u64(self.typed.next_record);
        put_handle(&mut encoder, &self.records);
        put_handle(&mut encoder, &self.typed.keys);
        put_handle(&mut encoder, &self.typed.rows);
        put_handle(&mut encoder, &self.typed.terms);
        encoder.into_bytes()
    }

    /// Never: at least the descriptor. Answered without encoding, so
    /// committing a namespace's leaf does not hash its maps (hashing waits
    /// for a root).
    fn encodes_empty(&self) -> bool {
        false
    }

    /// Also rejects a key count other than the row count (every live
    /// record has exactly one binding) and an allocator that is not above
    /// every possible live id.
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut decoder = Decoder::new(bytes);
        let descriptor: NamespaceDescriptor = decoder.get().map_err(codec_error)?;
        let next_record = decoder.u64().map_err(codec_error)?;
        let records = get_handle(&mut decoder)?;
        let keys: Keys = get_handle(&mut decoder)?;
        let rows: Rows = get_handle(&mut decoder)?;
        let terms = get_handle(&mut decoder)?;
        decoder.finish().map_err(codec_error)?;
        if keys.len() != rows.len() {
            return Err(CodecError::new("namespace leaf: key and row counts differ"));
        }
        if next_record <= rows.len() {
            return Err(CodecError::new(
                "namespace leaf: record allocator is not above the live rows",
            ));
        }
        Ok(Self {
            descriptor,
            records,
            typed: TypedState {
                next_record,
                keys,
                rows,
                terms,
            },
        })
    }
}

/// Whether a `(root, len)` pair can describe a map: `(EMPTY_ROOT_HASH, 0)`
/// or another root with entries.
pub(crate) fn valid_handle(root: &B256, len: u64) -> bool {
    (*root == EMPTY_ROOT_HASH) == (len == 0)
}

#[cfg(test)]
mod tests {
    //! The key encodings order exactly as the orders the engine relied on
    //! before it moved to tries: bytewise for raw keys, and the derived
    //! order of `(name, type, Option<term bytes>)` for terms. The expected
    //! order is Rust's own slice and tuple ordering over an independent
    //! model of each key, never the encoding under test.

    use std::cmp::Ordering;

    use arkiv_db_trie::{MemoryOnly, Probe};

    use super::*;

    /// SplitMix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// Raw keys biased toward the cases where an escaped encoding could go
    /// wrong: empty keys, zero and one bytes, trailing zeros, prefixes of
    /// each other and the full 20 bytes.
    fn raw_keys(rng: &mut Rng) -> Vec<Vec<u8>> {
        let mut keys = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0; 20],
            vec![0xff; 20],
            vec![1],
            vec![1, 0],
            vec![1, 0, 0],
            b"a".to_vec(),
            b"a\x00".to_vec(),
            b"a\xff".to_vec(),
            b"ab".to_vec(),
        ];
        for _ in 0..400 {
            let len = rng.below(21);
            let alphabet = [0u8, 1, 0x7f, 0x80, 0xff];
            let key: Vec<u8> = (0..len)
                .map(|_| alphabet[rng.below(alphabet.len())])
                .collect();
            keys.push(key);
        }
        keys
    }

    fn raw(key: &[u8]) -> Vec<u8> {
        RawKey(Arc::from(key)).encode()
    }

    #[test]
    fn raw_key_order_is_bytewise_order() {
        let mut rng = Rng(0x5eed_0001);
        let keys = raw_keys(&mut rng);
        for left in &keys {
            assert_eq!(
                RawKey::decode(&raw(left)).unwrap().0.as_ref(),
                left.as_slice()
            );
            for right in &keys {
                assert_eq!(
                    raw(left).cmp(&raw(right)),
                    left.cmp(right),
                    "{left:?} vs {right:?}"
                );
            }
        }
        let encodings: Vec<_> = keys.iter().map(|key| raw(key)).collect();
        assert!(arkiv_db_trie::is_prefix_free(&{
            let mut unique = encodings.clone();
            unique.sort();
            unique.dedup();
            unique
        }));
        // A prefix seeks at or before every key that extends it, and after
        // every smaller key that does not.
        for prefix in &keys {
            for key in &keys {
                if key.starts_with(prefix) {
                    assert!(raw(prefix) <= raw(key), "{prefix:?} <= {key:?}");
                } else if key < prefix {
                    assert!(raw(key) < raw(prefix), "{key:?} < {prefix:?}");
                }
            }
        }
        // Hand-written vectors: the empty key, a zero byte, a one byte, a
        // full key.
        assert_eq!(raw(b""), [0x00]);
        assert_eq!(raw(b"\x00"), [0x01, 0x01, 0x00]);
        assert_eq!(raw(b"a\x01"), [b'a', 0x01, 0x02, 0x00]);
        let mut full = vec![0xffu8; 20];
        full.push(0);
        assert_eq!(raw(&[0xff; 20]), full);
        assert_eq!(raw(&[0; 20]).len(), RawKey::MAX_LEN);
        assert!(RawKey::decode(b"ab").is_err(), "no terminator");
        assert!(RawKey::decode(&[0x01, 0x03, 0x00]).is_err(), "bad escape");
        assert!(
            RawKey::decode(&[b'a', 0x00, 0x00]).is_err(),
            "trailing bytes"
        );
        let mut long = vec![b'a'; 21];
        long.push(0);
        assert!(RawKey::decode(&long).is_err(), "length above 20");
    }

    #[test]
    fn record_keys_are_length_prefixed() {
        let key = |bytes: &[u8]| RecordKey::new(bytes).unwrap();
        assert_eq!(TrieKey::encode(&key(b"r")), [1, b'r']);
        assert_eq!(
            TrieKey::encode(&key(&[0; 32])).len(),
            RecordKey::MAX_LEN + 1
        );
        for bytes in [&b"r"[..], &[0; 32], b"\0a"] {
            let encoded = TrieKey::encode(&key(bytes));
            assert_eq!(
                <RecordKey as TrieKey>::decode(&encoded).unwrap(),
                key(bytes)
            );
        }
        for bad in [&[][..], &[0], &[2, b'r'], &[1, b'r', 0], &[33; 34]] {
            assert!(<RecordKey as TrieKey>::decode(bad).is_err(), "{bad:?}");
        }
    }

    /// The model of a term: `(name, type id, None | Some(term bytes))`,
    /// ordered by Rust's tuple order, exactly the old derived `TermKey`
    /// order.
    type Model = (String, u8, Option<Vec<u8>>);

    fn term(model: &Model) -> TermKey {
        let name = CellName::new(&model.0).unwrap();
        let tag = TypeTag::ALL
            .into_iter()
            .find(|tag| tag.id() == model.1)
            .unwrap();
        match &model.2 {
            None => TermKey::existence(&name, tag),
            Some(bytes) => TermKey::run_start(&name, tag, bytes),
        }
    }

    fn value_model(name: &str, value: &Value) -> Model {
        (name.to_owned(), value.tag().id(), value.term_bytes())
    }

    fn term_models(rng: &mut Rng) -> Vec<Model> {
        let names = ["a", "aa", "a0", "b", "B", "$exp", "z:1", &"n".repeat(20)];
        let mut values = vec![
            Value::I64(i64::MIN),
            Value::I64(-2),
            Value::I64(-1),
            Value::I64(0),
            Value::I64(1),
            Value::I64(i64::MAX),
            Value::U64(0),
            Value::U64(1),
            Value::U64(u64::MAX),
            Value::Bool(false),
            Value::Bool(true),
            Value::str(""),
            Value::str("\0"),
            Value::str("a"),
            Value::str("a\0"),
            Value::str("ab"),
            Value::str(&"x".repeat(64)),
            Value::str(&"x".repeat(63)),
            Value::str(&format!("{}\u{7f}", "x".repeat(63))),
            Value::str("é"),
        ];
        for _ in 0..60 {
            values.push(match rng.below(4) {
                0 => Value::I64(rng.next() as i64),
                1 => Value::U64(rng.next()),
                2 => Value::Bool(rng.next().is_multiple_of(2)),
                _ => {
                    let len = rng.below(65);
                    let text: String = (0..len)
                        .map(|_| ['\0', 'a', 'b', '\u{7f}'][rng.below(4)])
                        .collect();
                    Value::str(&text)
                }
            });
        }
        let mut models = Vec::new();
        for name in names {
            for tag in TypeTag::ALL.into_iter().filter(|tag| tag.is_indexable()) {
                models.push((name.to_owned(), tag.id(), None));
            }
            for value in &values {
                models.push(value_model(name, value));
            }
        }
        models
    }

    #[test]
    fn term_order_is_the_derived_tuple_order() {
        let mut rng = Rng(0x5eed_0002);
        let models = term_models(&mut rng);
        let mut encodings = Vec::new();
        for left in &models {
            let encoded = term(left).encode();
            assert!(encoded.len() <= TermKey::max_len_for(&Limits::DEFAULT));
            assert_eq!(TermKey::decode(&encoded).unwrap(), term(left));
            encodings.push(encoded.clone());
            for right in &models {
                assert_eq!(
                    encoded.cmp(&term(right).encode()),
                    left.cmp(right),
                    "{left:?} vs {right:?}"
                );
                // The struct's derived order is the same tuple order.
                assert_eq!(term(left).cmp(&term(right)), left.cmp(right));
            }
        }
        encodings.sort();
        encodings.dedup();
        assert!(arkiv_db_trie::is_prefix_free(&encodings));
        // Existence sorts directly before the empty value of its run; they
        // differ at the presence byte.
        let name = CellName::new("s").unwrap();
        let existence = TermKey::existence(&name, TypeTag::Str).encode();
        let empty = TermKey::value(&name, &Value::str("")).encode();
        assert_eq!(existence.cmp(&empty), Ordering::Less);
        assert_eq!(existence, [b's', 0, 2, 0]);
        assert_eq!(empty, [b's', 0, 2, 1, 0]);
    }

    #[test]
    fn a_term_map_iterates_in_model_order() {
        let mut rng = Rng(0x5eed_0003);
        let mut models = term_models(&mut rng);
        let mut shuffled = models.clone();
        for index in (1..shuffled.len()).rev() {
            shuffled.swap(index, rng.below(index + 1));
        }
        let mut map: MerkleMap<TermKey, Member> = MerkleMap::new();
        let mut probe = Probe::new();
        for model in &shuffled {
            map = map
                .insert(&term(model), Member, &MemoryOnly, &mut probe)
                .unwrap()
                .0;
        }
        models.sort();
        models.dedup();
        let iterated: Vec<TermKey> = map
            .iter(&MemoryOnly)
            .map(|entry| entry.unwrap().0)
            .collect();
        let expected: Vec<TermKey> = models.iter().map(term).collect();
        assert_eq!(iterated, expected);
    }

    #[test]
    fn term_keys_have_the_documented_layout() {
        // Built by hand: "c" and its terminator, type Str (2), value
        // presence, "red" shifted by one and its terminator.
        let name = CellName::new("c").unwrap();
        let red = TermKey::value(&name, &Value::str("red")).encode();
        assert_eq!(red, [b'c', 0, 2, 1, b's', b'f', b'e', 0]);
        // Embedded NUL and multibyte UTF-8 keep their length plus one.
        assert_eq!(
            TermKey::value(&name, &Value::str("a\0é")).encode(),
            [b'c', 0, 2, 1, 0x62, 0x01, 0xc4, 0xaa, 0]
        );
        // The existence term of n: I64 (17).
        let name = CellName::new("n").unwrap();
        let existence = TermKey::existence(&name, TypeTag::I64).encode();
        assert_eq!(existence, [b'n', 0, 17, 0]);
        // A scalar value term: its fixed-width term bytes, no terminator.
        let one = TermKey::value(&name, &Value::I64(1)).encode();
        assert_eq!(one, [b'n', 0, 17, 1, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            TermKey::decode(&one).unwrap(),
            TermKey::value(&name, &Value::I64(1))
        );
        // The longest keys: a 32-byte name and a ceiling-length string, and
        // the default instance bound equals the old fixed width.
        let longest = TermKey::value(
            &CellName::new(&"n".repeat(CellName::MAX_LEN)).unwrap(),
            &Value::str(&"\0".repeat(Value::INDEXED_LEN_CEILING)),
        );
        assert_eq!(longest.encode().len(), TermKey::MAX_LEN);
        assert_eq!(TermKey::decode(&longest.encode()).unwrap(), longest);
        assert_eq!(TermKey::max_len_for(&Limits::DEFAULT), 164);
        // Malformed keys are rejected.
        let mut bad_presence = existence.clone();
        bad_presence[3] = 2;
        assert!(TermKey::decode(&bad_presence).is_err());
        let mut bytes_type = existence.clone();
        bytes_type[2] = TypeTag::Bytes.id();
        assert!(TermKey::decode(&bytes_type).is_err());
        let mut existence_with_value = existence.clone();
        existence_with_value.push(0);
        assert!(TermKey::decode(&existence_with_value).is_err());
        assert!(
            TermKey::decode(&one[..one.len() - 1]).is_err(),
            "I64 needs 8 bytes"
        );
        let mut long_i64 = one.clone();
        long_i64.push(0);
        assert!(TermKey::decode(&long_i64).is_err(), "I64 has 8 bytes");
        assert!(
            TermKey::decode(&red[..red.len() - 1]).is_err(),
            "no terminator"
        );
        let mut trailing = red.clone();
        trailing.push(1);
        assert!(TermKey::decode(&trailing).is_err(), "trailing bytes");
        let mut not_utf8 = red.clone();
        not_utf8[4] = 0xff;
        assert!(TermKey::decode(&not_utf8).is_err(), "invalid UTF-8");
        assert!(
            TermKey::decode(&[b'n', 17, 0]).is_err(),
            "unterminated name"
        );
        assert!(TermKey::decode(&[b'1', 0, 17, 0]).is_err(), "name grammar");
        assert!(
            TermKey::decode(&[0x01, 0x01, 0, 17, 0]).is_err(),
            "name grammar"
        );
        let over = TermKey::value(
            &CellName::new("s").unwrap(),
            &Value::str(&"x".repeat(Value::INDEXED_LEN_CEILING)),
        );
        let mut over = over.encode();
        over.insert(4, b'y');
        assert!(TermKey::decode(&over).is_err(), "above the ceiling");
    }

    #[test]
    fn str_prefix_runs_hold_exactly_the_strings_with_the_prefix() {
        let name = CellName::new("s").unwrap();
        let strings = [
            "",
            "\0",
            "\0\0",
            "\u{1}",
            "a",
            "a\0",
            "a\0b",
            "a\u{1}",
            "aa",
            "ab",
            "b",
            "é",
            "éa",
            "\u{10ffff}",
        ];
        for prefix in strings {
            let run = TermKey::str_prefix_run(&name, prefix.as_bytes());
            for text in strings {
                let key = TermKey::value(&name, &Value::str(text)).encode();
                assert_eq!(
                    run.contains(&key),
                    text.starts_with(prefix),
                    "{prefix:?} {text:?}"
                );
            }
            // Never another type, name or the existence term.
            for other in [
                TermKey::existence(&name, TypeTag::Str).encode(),
                TermKey::value(&CellName::new("t").unwrap(), &Value::str(prefix)).encode(),
                TermKey::value(&CellName::new("sa").unwrap(), &Value::str(prefix)).encode(),
                TermKey::value(&name, &Value::Bool(true)).encode(),
            ] {
                assert!(!run.contains(&other));
            }
        }
    }

    #[test]
    fn leaf_values_never_encode_empty() {
        let key = RecordKey::new(b"k").unwrap();
        let row = StoredRecord {
            key: key.clone(),
            cells: Arc::from(Vec::new()),
        };
        assert!(!row.encodes_empty() && !row.encode().is_empty());
        let blob = Blob(Arc::from(&b""[..]));
        assert!(!blob.encodes_empty() && !blob.encode().is_empty());
        assert!(!Member.encodes_empty() && !Member.encode().is_empty());
        let id = RecordId::from_ordinal(1).unwrap();
        assert!(!id.encodes_empty() && !TrieValue::encode(&id).is_empty());
        let posting = Posting(PostingMap::new());
        assert!(!posting.encodes_empty() && !posting.encode().is_empty());
        let leaf = NamespaceState {
            descriptor: NamespaceDescriptor {
                id: NamespaceId::from_ordinal(1).unwrap(),
                name: crate::NamespaceName::new("n").unwrap(),
                owner: crate::OwnerId::new([0; 20]),
                revision: crate::Revision::INITIAL,
            },
            records: Records::new(),
            typed: TypedState::new(),
        };
        assert!(!leaf.encodes_empty() && !leaf.encode().is_empty());
    }

    #[test]
    fn leaf_values_round_trip_and_reject_noncanonical_bytes() {
        assert_eq!(Blob(Arc::from(&b""[..])).encode(), [0, 0, 0, 0]);
        assert_eq!(
            Blob(Arc::from(&b"ab"[..])).encode(),
            [0, 0, 0, 2, b'a', b'b']
        );
        assert!(Blob::decode(&[0, 0, 0, 1]).is_err());
        assert_eq!(Member.encode(), [1]);
        assert!(Member::decode(&[0]).is_err());
        assert!(Member::decode(&[1, 1]).is_err());
        let id = RecordId::from_ordinal(258).unwrap();
        assert_eq!(TrieValue::encode(&id), [0, 0, 0, 0, 0, 0, 1, 2]);
        assert!(<RecordId as TrieValue>::decode(&[0; 8]).is_err());
        // A posting leaf holds a nonempty map.
        let mut empty = EMPTY_ROOT_HASH.to_vec();
        empty.extend(0u64.to_be_bytes());
        assert!(Posting::decode(&empty).is_err());
        let mut mismatched = EMPTY_ROOT_HASH.to_vec();
        mismatched.extend(1u64.to_be_bytes());
        assert!(Posting::decode(&mismatched).is_err());
        // Row cells must be strictly ascending by name.
        let key = RecordKey::new(b"k").unwrap();
        let cell = Cell::attribute(Value::Bool(true));
        let row = |names: &[&str]| StoredRecord {
            key: key.clone(),
            cells: names
                .iter()
                .map(|name| (CellName::new(name).unwrap(), cell.clone()))
                .collect::<Vec<_>>()
                .into(),
        };
        let sorted = row(&["a", "b"]);
        assert_eq!(StoredRecord::decode(&sorted.encode()).unwrap(), sorted);
        assert!(StoredRecord::decode(&row(&["b", "a"]).encode()).is_err());
        assert!(StoredRecord::decode(&row(&["a", "a"]).encode()).is_err());
        // Attributes must be indexable and at most 64 bytes; fields need not.
        let one = |cell: Cell| StoredRecord {
            key: key.clone(),
            cells: vec![(CellName::new("c").unwrap(), cell)].into(),
        };
        let long = "x".repeat(Value::INDEXED_LEN_CEILING + 1);
        for accepted in [
            one(Cell::attribute(Value::str(&"x".repeat(64)))),
            one(Cell::field(Value::bytes(b"b"))),
            one(Cell::field(Value::str(&long))),
        ] {
            assert_eq!(StoredRecord::decode(&accepted.encode()).unwrap(), accepted);
        }
        for rejected in [
            one(Cell::attribute(Value::bytes(b"b"))),
            one(Cell::attribute(Value::str(&long))),
        ] {
            assert!(StoredRecord::decode(&rejected.encode()).is_err());
        }
    }
}
