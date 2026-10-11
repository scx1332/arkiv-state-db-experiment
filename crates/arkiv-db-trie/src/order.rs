//! Order-preserving, prefix-free key encodings.
//!
//! A [`TrieKey`](crate::TrieKey) encoding must preserve order and be
//! prefix-free, and the map orders keys by their encoded bytes. These
//! building blocks turn composite keys into such byte strings:
//!
//! - [`encode_bytes`]: a self-delimiting byte string. Every `0x00` byte
//!   becomes `01 01`, every `0x01` byte becomes `01 02`, every other byte is
//!   itself, and `0x00` terminates. The terminator occurs nowhere else, so
//!   no encoding is a strict prefix of another; and since the terminator is
//!   the smallest byte and the escapes keep the order of `00 < 01 < 02..`,
//!   bytewise order of encodings equals bytewise order of the strings (a
//!   proper prefix first). [`escape_bytes`] is the encoding without the
//!   terminator: it is a prefix of the encoding of every string that starts
//!   with the input, and of no other, so `[escape(p), prefix_successor(escape(p)))`
//!   is exactly the run of strings starting with `p`.
//!
//!   (The FoundationDB tuple form, `00 → 00 ff` with a `00` terminator, is
//!   not prefix-free as a whole key: `61 00` encodes `"a"` and is a strict
//!   prefix of `61 00 ff 00`, the encoding of `"a\0"`. Branches here hold
//!   no values, so the variable part of a key must be strictly prefix-free.)
//! - [`encode_padded`]: a byte string of at most `max_len` bytes becomes
//!   `bytes ‖ 0x00 × (max_len − len) ‖ len`, `max_len + 1` bytes wide. For
//!   two strings, the first differing padded byte decides exactly as the
//!   bytewise comparison of the strings does (a string that ends earlier
//!   shows `0x00` where the longer one continues, and a proper prefix is
//!   smaller); when the padded bytes are all equal, one string is the other
//!   plus trailing zero bytes and the length byte puts the shorter first.
//! - Fixed-width fields (single bytes, big-endian integers, fixed arrays)
//!   compare as their values do.
//! - Concatenating prefix-free fields gives lexicographic tuple order and a
//!   prefix-free whole: two tuples first differ inside one field, before
//!   either field's encoding ends.
//!
//! [`KeyWriter`] and [`KeyReader`] compose these; the tests check the order
//! claims against Rust's own slice and tuple ordering on random inputs.

use crate::codec::CodecError;

/// The terminator of [`encode_bytes`].
const TERMINATOR: u8 = 0x00;
/// The escape byte of [`encode_bytes`]: `01 01` is `00`, `01 02` is `01`.
const ESCAPE: u8 = 0x01;

/// Appends the escaped form of `value` without a terminator.
///
/// It is a prefix of [`encode_bytes`] of every string starting with `value`
/// and of no other encoding, and it is the smallest such string.
pub fn escape_bytes(value: &[u8], out: &mut Vec<u8>) {
    for &byte in value {
        match byte {
            0x00 => out.extend_from_slice(&[ESCAPE, 0x01]),
            0x01 => out.extend_from_slice(&[ESCAPE, 0x02]),
            other => out.push(other),
        }
    }
}

/// Appends the prefix-free, order-preserving encoding of `value`: the
/// [`escape_bytes`] form followed by a `00` terminator.
pub fn encode_bytes(value: &[u8], out: &mut Vec<u8>) {
    escape_bytes(value, out);
    out.push(TERMINATOR);
}

/// The longest [`encode_bytes`] output for an input of `max_len` bytes.
pub const fn encoded_bytes_max(max_len: usize) -> usize {
    2 * max_len + 1
}

/// Decodes one [`encode_bytes`] field at the start of `bytes`, returning
/// the value and the bytes after its terminator.
///
/// Rejects a missing terminator, an escape byte followed by anything but
/// `01` or `02`, and a value longer than `max_len`, so every accepted field
/// is the unique encoding of its value. Never allocates more than the
/// decoded value.
pub fn decode_bytes(bytes: &[u8], max_len: usize) -> Result<(Vec<u8>, &[u8]), CodecError> {
    let mut value = Vec::new();
    let mut index = 0;
    loop {
        let byte = *bytes
            .get(index)
            .ok_or_else(|| CodecError::new("byte field has no terminator"))?;
        index += 1;
        let decoded = match byte {
            TERMINATOR => return Ok((value, &bytes[index..])),
            ESCAPE => {
                let escaped = bytes.get(index).copied();
                index += 1;
                match escaped {
                    Some(0x01) => 0x00,
                    Some(0x02) => 0x01,
                    _ => return Err(CodecError::new("invalid escape in a byte field")),
                }
            }
            other => other,
        };
        if value.len() == max_len {
            return Err(CodecError::new("byte field longer than its bound"));
        }
        value.push(decoded);
    }
}

/// The smallest byte string above every string that starts with `prefix`:
/// `prefix` with its last byte below `ff` incremented and everything after
/// it dropped. `None` when no such string exists (`prefix` is empty or all
/// `ff`), meaning the run is unbounded above. Not the successor of one key:
/// an exclusive bound at `prefix_successor(p)` admits every extension of `p`.
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let last = prefix.iter().rposition(|byte| *byte != 0xff)?;
    let mut successor = prefix[..=last].to_vec();
    successor[last] += 1;
    Some(successor)
}

/// The encoded width of a padded field that holds up to `max_len` bytes.
pub const fn padded_width(max_len: u8) -> usize {
    max_len as usize + 1
}

/// Appends `value ‖ 0x00 × (max_len − len) ‖ len`.
///
/// Fails, appending nothing, when `value` is longer than `max_len`.
pub fn encode_padded(value: &[u8], max_len: u8, out: &mut Vec<u8>) -> Result<(), CodecError> {
    let length = u8::try_from(value.len())
        .ok()
        .filter(|length| *length <= max_len)
        .ok_or_else(|| CodecError::new("value longer than the padded field"))?;
    out.extend_from_slice(value);
    out.resize(out.len() + usize::from(max_len - length), 0);
    out.push(length);
    Ok(())
}

/// Decodes a padded field of exactly [`padded_width`]`(max_len)` bytes.
///
/// Rejects a length above `max_len` and nonzero padding, so every accepted
/// field is the unique encoding of the returned bytes.
pub fn decode_padded(field: &[u8], max_len: u8) -> Result<&[u8], CodecError> {
    if field.len() != padded_width(max_len) {
        return Err(CodecError::new("padded field has the wrong width"));
    }
    let (body, length) = field.split_at(usize::from(max_len));
    let length = usize::from(length[0]);
    if length > usize::from(max_len) {
        return Err(CodecError::new("padded length exceeds the field"));
    }
    let (value, padding) = body.split_at(length);
    if padding.iter().any(|byte| *byte != 0) {
        return Err(CodecError::new("nonzero padding in a padded field"));
    }
    Ok(value)
}

/// Builds a fixed-width, order-preserving key from fields in order.
///
/// ```
/// use arkiv_db_trie::order::KeyWriter;
///
/// // (name ≤ 20 bytes, tag, presence, value ≤ 64 bytes) → 88 bytes.
/// let mut key = KeyWriter::new();
/// key.padded(b"color", 20)?.byte(4).byte(1).padded(b"red", 64)?;
/// assert_eq!(key.finish().len(), 88);
/// # Ok::<(), arkiv_db_trie::CodecError>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct KeyWriter {
    bytes: Vec<u8>,
}

impl KeyWriter {
    /// Starts an empty key.
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Appends a padded variable-length field; see [`encode_padded`].
    pub fn padded(&mut self, value: &[u8], max_len: u8) -> Result<&mut Self, CodecError> {
        encode_padded(value, max_len, &mut self.bytes)?;
        Ok(self)
    }

    /// Appends a self-delimiting byte string; see [`encode_bytes`].
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        encode_bytes(value, &mut self.bytes);
        self
    }

    /// Appends one byte.
    pub fn byte(&mut self, value: u8) -> &mut Self {
        self.bytes.push(value);
        self
    }

    /// Appends a big-endian `u64`.
    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends bytes whose width is fixed by the key type.
    pub fn fixed(&mut self, bytes: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(bytes);
        self
    }

    /// Returns the key bytes.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

/// Reads the fields of a key built by [`KeyWriter`], in the same order.
#[derive(Clone, Debug)]
pub struct KeyReader<'a> {
    rest: &'a [u8],
}

impl<'a> KeyReader<'a> {
    /// Starts reading `bytes`.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn take(&mut self, width: usize) -> Result<&'a [u8], CodecError> {
        if self.rest.len() < width {
            return Err(CodecError::new("key is too short"));
        }
        let (field, rest) = self.rest.split_at(width);
        self.rest = rest;
        Ok(field)
    }

    /// Reads a padded field written with the same `max_len`.
    pub fn padded(&mut self, max_len: u8) -> Result<&'a [u8], CodecError> {
        decode_padded(self.take(padded_width(max_len))?, max_len)
    }

    /// Reads a self-delimiting byte string of at most `max_len` bytes.
    pub fn bytes(&mut self, max_len: usize) -> Result<Vec<u8>, CodecError> {
        let (value, rest) = decode_bytes(self.rest, max_len)?;
        self.rest = rest;
        Ok(value)
    }

    /// Reads one byte.
    pub fn byte(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, CodecError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| CodecError::new("key is too short"))?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Reads `width` fixed bytes.
    pub fn fixed(&mut self, width: usize) -> Result<&'a [u8], CodecError> {
        self.take(width)
    }

    /// Succeeds only if every byte was read.
    pub fn finish(self) -> Result<(), CodecError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(CodecError::new("trailing bytes after the last key field"))
        }
    }
}

/// A bounded byte-string key of at most `MAX` bytes, the empty string
/// included, keyed by [`encode_bytes`] (`MAX_LEN = 2·MAX + 1`).
///
/// Prefix-free and ordered as the byte strings are, so any set of these keys
/// can share a map: `""`, `"a"`, `"a\0"` and `"aa"` are four distinct
/// entries in that order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteKey<const MAX: usize>(pub Vec<u8>);

impl<const MAX: usize> crate::TrieKey for ByteKey<MAX> {
    const MAX_LEN: usize = encoded_bytes_max(MAX);

    /// # Panics
    ///
    /// If the key is longer than `MAX` bytes.
    fn encode(&self) -> Vec<u8> {
        assert!(self.0.len() <= MAX, "ByteKey longer than its bound");
        let mut out = Vec::with_capacity(self.0.len() + 1);
        encode_bytes(&self.0, &mut out);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (value, rest) = decode_bytes(bytes, MAX)?;
        if !rest.is_empty() {
            return Err(CodecError::new("trailing bytes after a byte key"));
        }
        Ok(Self(value))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KeyReader, KeyWriter, decode_bytes, decode_padded, encode_bytes, encode_padded,
        escape_bytes, padded_width, prefix_successor,
    };

    fn enc(value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_bytes(value, &mut out);
        out
    }

    #[test]
    fn byte_field_vectors() {
        assert_eq!(enc(b""), [0x00]);
        assert_eq!(enc(b"a"), [0x61, 0x00]);
        assert_eq!(enc(b"a\0b"), [0x61, 0x01, 0x01, 0x62, 0x00]);
        assert_eq!(enc(&[0x01, 0xff]), [0x01, 0x02, 0xff, 0x00]);
        for value in [&b""[..], b"a", b"a\0", b"\x01\x00\xff", &[0xff; 9]] {
            assert_eq!(decode_bytes(&enc(value), 16), Ok((value.to_vec(), &[][..])));
        }
        // Noncanonical or truncated fields.
        assert!(decode_bytes(&[], 4).is_err());
        assert!(decode_bytes(b"ab", 4).is_err());
        assert!(decode_bytes(&[0x01, 0x03, 0x00], 4).is_err());
        assert!(decode_bytes(&[0x01], 4).is_err());
        assert!(decode_bytes(&[0x01, 0x00], 4).is_err());
        assert!(decode_bytes(&enc(b"abcde"), 4).is_err());
        assert_eq!(
            decode_bytes(&[0x61, 0x00, 0x07], 4),
            Ok((b"a".to_vec(), &[0x07][..]))
        );
        // Prefix-related strings stay ordered and prefix-free.
        let ordered = [
            &b""[..],
            b"\0",
            b"\x01",
            b"a",
            b"a\0",
            b"a\0\0",
            b"a\x01",
            b"aa",
            b"a\xff",
        ];
        for pair in ordered.windows(2) {
            assert!(enc(pair[0]) < enc(pair[1]));
            assert!(!enc(pair[1]).starts_with(&enc(pair[0])));
        }
        let mut escaped = Vec::new();
        escape_bytes(b"a\0", &mut escaped);
        assert!(enc(b"a\0b").starts_with(&escaped));
        assert!(!enc(b"a").starts_with(&escaped));
    }

    #[test]
    fn prefix_successor_vectors() {
        assert_eq!(prefix_successor(b""), None);
        assert_eq!(prefix_successor(&[0xff, 0xff]), None);
        assert_eq!(prefix_successor(b"a"), Some(b"b".to_vec()));
        assert_eq!(prefix_successor(&[0x61, 0xff]), Some(vec![0x62]));
        assert_eq!(prefix_successor(&[0x00, 0x00]), Some(vec![0x00, 0x01]));
    }

    #[test]
    fn padded_vectors() {
        let mut out = Vec::new();
        encode_padded(b"ab", 4, &mut out).unwrap();
        assert_eq!(out, [b'a', b'b', 0, 0, 2]);
        out.clear();
        encode_padded(b"", 3, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 0]);
        out.clear();
        encode_padded(b"abc", 3, &mut out).unwrap();
        assert_eq!(out, [b'a', b'b', b'c', 3]);
        assert!(encode_padded(b"abcd", 3, &mut out).is_err());
        assert_eq!(
            out,
            [b'a', b'b', b'c', 3],
            "a failed encode appends nothing"
        );
        assert_eq!(padded_width(64), 65);
        assert_eq!(decode_padded(&[b'a', b'b', 0, 0, 2], 4), Ok(&b"ab"[..]));
        assert!(decode_padded(&[b'a', b'b', 0, 1, 2], 4).is_err());
        assert!(decode_padded(&[b'a', b'b', 0, 0, 5], 4).is_err());
        assert!(decode_padded(&[b'a', b'b', 0, 2], 4).is_err());
    }

    #[test]
    fn writer_and_reader_round_trip() {
        let mut writer = KeyWriter::new();
        writer
            .padded(b"name", 20)
            .unwrap()
            .byte(7)
            .u64(0x0102_0304_0506_0708)
            .fixed(&[9, 9]);
        let key = writer.finish();
        assert_eq!(key.len(), 21 + 1 + 8 + 2);
        let mut reader = KeyReader::new(&key);
        assert_eq!(reader.padded(20).unwrap(), b"name");
        assert_eq!(reader.byte().unwrap(), 7);
        assert_eq!(reader.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(reader.fixed(2).unwrap(), [9, 9]);
        reader.finish().unwrap();
        let mut short = KeyReader::new(&key[..5]);
        assert!(short.padded(20).is_err());
        let mut trailing = KeyReader::new(&key);
        trailing.padded(20).unwrap();
        assert!(trailing.finish().is_err());
    }
}
