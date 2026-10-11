//! The key and leaf encodings of the engine's trie maps (state format 1).
//!
//! Every map is an [`arkiv_db_trie::MerkleMap`] with a fixed key length; it
//! orders entries by the unsigned bytewise order of the key encodings, so
//! each key encoding below preserves the order the engine relies on. Leaf
//! values are canonical-codec encodings (codec version
//! [`CODEC_VERSION`](crate::CODEC_VERSION)) and never empty.
//!
//! | Map | Key | Bytes | Leaf value |
//! | --- | --- | --- | --- |
//! | catalog | [`NamespaceId`] big-endian | 8 | [`NamespaceState`]: `descriptor ‖ next_record u64 ‖ records ‖ keys ‖ rows ‖ terms`, each map as `root ‖ len u64` |
//! | host | raw `[u8; 32]` | 32 | [`Blob`]: `u32 len ‖ bytes` |
//! | records (raw) | [`RawKey`]: `key ‖ 0x00 pad to 20 ‖ len` | 21 | [`Blob`]: `u32 len ‖ bytes` |
//! | keys | [`RecordKey`]: `len ‖ bytes ‖ 0x00 pad` | 33 | [`RecordId`]: `u64` (nonzero) |
//! | rows | [`RecordId`] big-endian | 8 | [`StoredRecord`]: `RecordKey ‖ Vec<(CellName, Cell)>` sorted by name |
//! | terms | [`TermKey`]: `name ‖ 0x00 pad to 32 ‖ name_len ‖ tag ‖ presence ‖ term bytes ‖ 0x00 pad to 128 ‖ term_len` | 164 | [`Posting`]: `posting root ‖ posting len u64` (len ≥ 1) |
//! | postings | [`RecordId`] big-endian | 8 | [`Member`]: `01` |
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
    order::{KeyReader, KeyWriter, decode_padded, encode_padded},
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

/// The version-2 canonical encoding, `len ‖ bytes ‖ 0x00 padding` (33
/// bytes). The key map is only read by point lookups, so its order is
/// never observed.
impl TrieKey for RecordKey {
    const MAX_LEN: usize = Self::ENCODED_LEN;

    fn encode(&self) -> Vec<u8> {
        self.encoded().to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let bytes = bytes
            .try_into()
            .map_err(|_| CodecError::new("record key is not 33 bytes"))?;
        Self::decode(bytes).ok_or_else(|| CodecError::new("record key encoding is not canonical"))
    }
}

/// A raw record key of at most [`Limits::MAX_RAW_KEY_LEN`] bytes, empty
/// included, keyed as `key ‖ 0x00 pad to 20 ‖ len` (21 bytes).
///
/// The padded encoding orders exactly as the keys' bytewise order (see
/// [`arkiv_db_trie::order`]), and the encoding of a prefix is at most the
/// encoding of every key that starts with it and above every smaller key,
/// so a prefix scan seeks to the encoding of the prefix itself.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RawKey(pub(crate) Arc<[u8]>);

const RAW_KEY_MAX: u8 = Limits::MAX_RAW_KEY_LEN as u8;

impl TrieKey for RawKey {
    const MAX_LEN: usize = Limits::MAX_RAW_KEY_LEN + 1;

    /// # Panics
    ///
    /// If the key is longer than [`Limits::MAX_RAW_KEY_LEN`], which
    /// admission rejects before any key reaches a map.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::MAX_LEN);
        encode_padded(&self.0, RAW_KEY_MAX, &mut out)
            .expect("admission bounds raw keys and prefixes by 20 bytes");
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_padded(bytes, RAW_KEY_MAX).map(|key| Self(Arc::from(key)))
    }
}

/// Whether a term is a value term or the existence term of its
/// `(name, type)`; the presence byte of the term key.
const PRESENCE_EXISTENCE: u8 = 0;
const PRESENCE_VALUE: u8 = 1;

/// Width of the name field of a term key: [`CellName::MAX_LEN`].
const TERM_NAME_MAX: u8 = CellName::MAX_LEN as u8;
/// Width of the value field of a term key: [`Value::MAX_INDEXED_LEN`].
const TERM_VALUE_MAX: u8 = Value::MAX_INDEXED_LEN as u8;

/// One index term: a value term (`value = Some`, its term bytes) or the
/// existence term of a `(name, type)` (`value = None`).
///
/// Keyed as `name ‖ 0x00 pad to 32 ‖ name_len ‖ tag ‖ presence ‖ term bytes
/// ‖ 0x00 pad to 128 ‖ term_len` (164 bytes, presence `00` for existence with
/// an empty value field, `01` for a value). Every field has a fixed width
/// and the padded fields preserve bytewise order, so terms order by name,
/// then type id, then existence before every value, then value bytes: every
/// `(name, type)` is one contiguous run in value order, as the derived
/// order of this struct's fields is.
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
    /// `(name, tag)` run.
    pub(crate) fn run_start(name: &CellName, tag: TypeTag, bytes: &[u8]) -> Self {
        Self {
            name: name.clone(),
            tag,
            value: Some(Arc::from(bytes)),
        }
    }
}

impl TrieKey for TermKey {
    const MAX_LEN: usize = CellName::MAX_LEN + Value::MAX_INDEXED_LEN + 4;

    /// # Panics
    ///
    /// If the name exceeds 32 bytes or the value 128, which name validation
    /// and admission rule out before any term is built.
    fn encode(&self) -> Vec<u8> {
        let mut key = KeyWriter::new();
        key.padded(self.name.as_str().as_bytes(), TERM_NAME_MAX)
            .expect("cell names are at most 32 bytes")
            .byte(self.tag.id());
        match &self.value {
            None => key
                .byte(PRESENCE_EXISTENCE)
                .padded(&[], TERM_VALUE_MAX)
                .expect("empty"),
            Some(bytes) => key
                .byte(PRESENCE_VALUE)
                .padded(bytes, TERM_VALUE_MAX)
                .expect("indexed values and query bounds are at most 128 bytes"),
        };
        key.finish()
    }

    /// Rejects a name outside the grammar, an unknown or unindexable type,
    /// a presence byte other than `00`/`01`, an existence term with value
    /// bytes, and value bytes that are not the term bytes of the type.
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = KeyReader::new(bytes);
        let name = std::str::from_utf8(reader.padded(TERM_NAME_MAX)?)
            .ok()
            .and_then(|name| CellName::new(name).ok())
            .ok_or_else(|| CodecError::new("term name is not a valid cell name"))?;
        let tag_id = reader.byte()?;
        let tag = TypeTag::ALL
            .into_iter()
            .find(|tag| tag.id() == tag_id && tag.is_indexable())
            .ok_or_else(|| CodecError::new("term type is not an indexable type id"))?;
        let presence = reader.byte()?;
        let value = reader.padded(TERM_VALUE_MAX)?;
        reader.finish()?;
        let value = match presence {
            PRESENCE_EXISTENCE if value.is_empty() => None,
            PRESENCE_VALUE if Value::from_term_bytes(tag, value).is_some() => {
                Some(Arc::from(value))
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
/// other than `Bytes` and at most [`Value::MAX_INDEXED_LEN`] encoded bytes,
/// as admission guarantees for every stored row.
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
                    || cell.value.encoded_len() > Value::MAX_INDEXED_LEN)
        }) {
            return Err(CodecError::new(
                "a row attribute is not indexable or longer than 128 bytes",
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

    /// Raw keys biased toward the cases where a padded encoding could go
    /// wrong: empty keys, zero bytes, trailing zeros, prefixes of each
    /// other and the full 20 bytes.
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
            assert_eq!(raw(left).len(), 21);
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
        // Hand-written vectors: the empty key, a trailing zero, a full key.
        let mut empty = [0u8; 21];
        empty[20] = 0;
        assert_eq!(raw(b""), empty);
        let mut zero = [0u8; 21];
        zero[20] = 1;
        assert_eq!(raw(b"\x00"), zero);
        let mut full = [0xffu8; 21];
        full[20] = 20;
        assert_eq!(raw(&[0xff; 20]), full);
        assert!(RawKey::decode(&[0u8; 20]).is_err(), "wrong width");
        let mut dirty = zero;
        dirty[5] = 1;
        assert!(RawKey::decode(&dirty).is_err(), "nonzero padding");
        let mut long = [0u8; 21];
        long[20] = 21;
        assert!(RawKey::decode(&long).is_err(), "length above 20");
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
        for left in &models {
            let encoded = term(left).encode();
            assert_eq!(encoded.len(), TermKey::MAX_LEN);
            assert_eq!(TermKey::decode(&encoded).unwrap(), term(left));
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
        // Existence sorts directly before the empty value of its run.
        let name = CellName::new("s").unwrap();
        let existence = TermKey::existence(&name, TypeTag::Str).encode();
        let empty = TermKey::value(&name, &Value::str("")).encode();
        assert_eq!(existence.cmp(&empty), Ordering::Less);
        assert_eq!(
            existence[..CellName::MAX_LEN + 2],
            empty[..CellName::MAX_LEN + 2]
        );
        assert_eq!(
            (
                existence[CellName::MAX_LEN + 2],
                empty[CellName::MAX_LEN + 2]
            ),
            (0, 1)
        );
        assert_eq!(
            existence[CellName::MAX_LEN + 3..],
            empty[CellName::MAX_LEN + 3..]
        );
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
        // Built by hand: "c" padded to 32, length 1, type Str (2), value
        // presence, "red" padded to 128, length 3.
        let mut expected = vec![b'c'];
        expected.extend(vec![0; CellName::MAX_LEN - 1]);
        expected.extend([1, 2, 1]);
        expected.extend(b"red");
        expected.extend(vec![0; Value::MAX_INDEXED_LEN - 3]);
        expected.push(3);
        let name = CellName::new("c").unwrap();
        assert_eq!(TermKey::value(&name, &Value::str("red")).encode(), expected);
        // The existence term of n: I64 (17).
        let mut expected = vec![b'n'];
        expected.extend(vec![0; CellName::MAX_LEN - 1]);
        expected.extend([1, 17, 0]);
        expected.extend(vec![0; Value::MAX_INDEXED_LEN + 1]);
        let name = CellName::new("n").unwrap();
        assert_eq!(TermKey::existence(&name, TypeTag::I64).encode(), expected);
        // Malformed keys are rejected.
        let mut bad_presence = expected.clone();
        bad_presence[CellName::MAX_LEN + 2] = 2;
        assert!(TermKey::decode(&bad_presence).is_err());
        let mut bytes_type = expected.clone();
        bytes_type[CellName::MAX_LEN + 1] = TypeTag::Bytes.id();
        assert!(TermKey::decode(&bytes_type).is_err());
        let mut existence_with_value = expected.clone();
        existence_with_value[CellName::MAX_LEN + 3] = 1;
        existence_with_value[TermKey::MAX_LEN - 1] = 1;
        assert!(TermKey::decode(&existence_with_value).is_err());
        let mut short_i64 = expected;
        short_i64[CellName::MAX_LEN + 2] = 1;
        short_i64[CellName::MAX_LEN + 3] = 1;
        short_i64[TermKey::MAX_LEN - 1] = 1;
        assert!(TermKey::decode(&short_i64).is_err(), "I64 needs 8 bytes");
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
        let long = "x".repeat(Value::MAX_INDEXED_LEN + 1);
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
