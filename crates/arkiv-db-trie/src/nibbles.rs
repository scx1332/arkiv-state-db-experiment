//! Nibble paths and the hex-prefix path encoding of the Ethereum MPT.
//!
//! Paths are kept unpacked, one nibble (0..=15) per byte, so they can be as
//! long as a map's keys require. `alloy_trie::Nibbles` is capped at 64
//! nibbles (32-byte keys), which is shorter than several engine key
//! encodings, so this crate uses its own representation; the byte encoding
//! it produces is the same hex-prefix form `alloy_trie::nodes` produces.

/// Splits bytes into nibbles, high nibble first.
pub(crate) fn unpack(bytes: &[u8]) -> Vec<u8> {
    let mut nibbles = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        nibbles.push(byte >> 4);
        nibbles.push(byte & 0x0f);
    }
    nibbles
}

/// Joins nibbles pairwise into bytes. The caller supplies an even count.
pub(crate) fn pack(nibbles: &[u8]) -> Vec<u8> {
    debug_assert!(
        nibbles.len().is_multiple_of(2),
        "packing an odd nibble count"
    );
    nibbles
        .chunks_exact(2)
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect()
}

/// Returns the length of the longest common prefix of two nibble paths.
pub(crate) fn common_prefix(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

/// Returns `prefix ‖ suffix` as a new path.
pub(crate) fn join(prefix: &[u8], suffix: &[u8]) -> Box<[u8]> {
    let mut path = Vec::with_capacity(prefix.len() + suffix.len());
    path.extend_from_slice(prefix);
    path.extend_from_slice(suffix);
    path.into_boxed_slice()
}

/// Flag nibble of an extension path (`0x0`, or `0x1` when odd).
const EXTENSION_FLAG: u8 = 0x00;
/// Flag nibble of a leaf path (`0x2`, or `0x3` when odd).
const LEAF_FLAG: u8 = 0x20;
/// Added to the flag when the path has an odd number of nibbles.
const ODD_FLAG: u8 = 0x10;

/// Appends the hex-prefix (compact) encoding of `path`.
///
/// The first byte carries the node type and parity; an odd path stores its
/// first nibble in the low half of that byte, an even path leaves it zero.
pub(crate) fn encode_path(path: &[u8], leaf: bool, out: &mut Vec<u8>) {
    let flag = if leaf { LEAF_FLAG } else { EXTENSION_FLAG };
    let rest = match path.split_first() {
        Some((first, rest)) if path.len() % 2 == 1 => {
            out.push(flag | ODD_FLAG | first);
            rest
        }
        _ => {
            out.push(flag);
            path
        }
    };
    out.extend(rest.chunks_exact(2).map(|pair| (pair[0] << 4) | pair[1]));
}

/// Length in bytes of the hex-prefix encoding of a path of `nibbles` nibbles.
pub(crate) const fn encoded_path_len(nibbles: usize) -> usize {
    nibbles / 2 + 1
}

/// Decodes a hex-prefix encoding into its nibbles and leaf flag.
///
/// Rejects an empty encoding, an unknown flag and a nonzero low nibble in
/// the first byte of an even path, so every accepted encoding is the only
/// encoding of its path.
pub(crate) fn decode_path(bytes: &[u8]) -> Result<(Vec<u8>, bool), &'static str> {
    let (&first, rest) = bytes.split_first().ok_or("empty node path")?;
    let leaf = match first >> 4 {
        0x0 | 0x1 => false,
        0x2 | 0x3 => true,
        _ => return Err("unknown node path flag"),
    };
    let mut path = Vec::with_capacity(rest.len() * 2 + 1);
    if first & ODD_FLAG != 0 {
        path.push(first & 0x0f);
    } else if first & 0x0f != 0 {
        return Err("nonzero padding nibble in an even node path");
    }
    path.extend(unpack(rest));
    Ok((path, leaf))
}

#[cfg(test)]
mod tests {
    use super::{common_prefix, decode_path, encode_path, encoded_path_len, pack, unpack};

    #[test]
    fn unpack_and_pack_round_trip() {
        assert_eq!(unpack(&[0xab, 0x0f]), vec![0xa, 0xb, 0x0, 0xf]);
        assert_eq!(pack(&[0xa, 0xb, 0x0, 0xf]), vec![0xab, 0x0f]);
        assert!(unpack(&[]).is_empty());
        assert_eq!(common_prefix(&[1, 2, 3], &[1, 2, 4, 5]), 2);
        assert_eq!(common_prefix(&[], &[1]), 0);
    }

    #[test]
    fn hex_prefix_vectors() {
        // The four flag cases of the Ethereum yellow paper, appendix C.
        let cases: [(&[u8], bool, &[u8]); 6] = [
            (&[0xa, 0xb, 0xc, 0xd], false, &[0x00, 0xab, 0xcd]),
            (&[0xa, 0xb, 0xc], false, &[0x1a, 0xbc]),
            (&[0xa, 0xb, 0xc, 0xd], true, &[0x20, 0xab, 0xcd]),
            (&[0xa, 0xb, 0xc], true, &[0x3a, 0xbc]),
            (&[], true, &[0x20]),
            (&[0x7], false, &[0x17]),
        ];
        for (path, leaf, expected) in cases {
            let mut out = Vec::new();
            encode_path(path, leaf, &mut out);
            assert_eq!(out, expected);
            assert_eq!(out.len(), encoded_path_len(path.len()));
            assert_eq!(decode_path(expected), Ok((path.to_vec(), leaf)));
        }
    }

    #[test]
    fn hex_prefix_rejects_noncanonical_forms() {
        assert!(decode_path(&[]).is_err());
        assert!(decode_path(&[0x40]).is_err());
        assert!(decode_path(&[0xf0, 0x12]).is_err());
        // Even paths must leave the low nibble of the flag byte zero.
        assert!(decode_path(&[0x05, 0x12]).is_err());
        assert!(decode_path(&[0x2f]).is_err());
    }
}
