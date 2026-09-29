//! Order-preserving fixed-width key encodings.
//!
//! A [`TrieKey`](crate::TrieKey) must encode to a fixed length, and the map
//! orders keys by their encoded bytes. These building blocks turn composite
//! keys with variable-length parts into fixed-width byte strings whose
//! bytewise order equals the order of the original tuple:
//!
//! - [`encode_padded`]: a byte string of at most `max_len` bytes becomes
//!   `bytes ‖ 0x00 × (max_len − len) ‖ len`, `max_len + 1` bytes wide. For
//!   two strings, the first differing padded byte decides exactly as the
//!   bytewise comparison of the strings does (a string that ends earlier
//!   shows `0x00` where the longer one continues, and a proper prefix is
//!   smaller); when the padded bytes are all equal, one string is the other
//!   plus trailing zero bytes and the length byte puts the shorter first.
//! - Fixed-width fields (single bytes, big-endian integers, fixed arrays)
//!   compare as their values do.
//! - Concatenating fixed-width fields gives lexicographic tuple order,
//!   because every field starts at the same offset in every key.
//!
//! [`KeyWriter`] and [`KeyReader`] compose these; the tests check the order
//! claims against Rust's own slice and tuple ordering on random inputs.

use crate::codec::CodecError;

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

#[cfg(test)]
mod tests {
    use super::{KeyReader, KeyWriter, decode_padded, encode_padded, padded_width};

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
