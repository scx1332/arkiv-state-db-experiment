use alloy_primitives::keccak256;
use arkiv_db_trie::{IntervalLimits, KeyRange, MemoryOnly, MerkleMap, Probe, ProofError};

/// The closed-interval form these tests were written against.
#[allow(dead_code)]
fn walk_interval<N: AsRef<[u8]>>(
    root: arkiv_db_trie::ObjectId,
    lower: &[u8],
    upper: &[u8],
    nodes: &[N],
    max_entries: usize,
) -> Result<arkiv_db_trie::IntervalEntries, ProofError> {
    arkiv_db_trie::walk_interval(
        root,
        &KeyRange::closed(lower, upper),
        nodes,
        IntervalLimits {
            max_entries,
            max_key_len: 256,
        },
    )
}

#[allow(dead_code)]
fn inspect_interval<N: AsRef<[u8]>>(
    root: arkiv_db_trie::ObjectId,
    lower: &[u8],
    upper: &[u8],
    nodes: &[N],
    max_entries: usize,
) -> Result<
    (
        arkiv_db_trie::IntervalEntries,
        arkiv_db_trie::VerifiedInterval,
    ),
    ProofError,
> {
    arkiv_db_trie::inspect_interval(
        root,
        &KeyRange::closed(lower, upper),
        nodes,
        IntervalLimits {
            max_entries,
            max_key_len: 256,
        },
    )
}

#[test]
fn self_consistent_malformed_intersecting_nodes_are_rejected() {
    // Each root is hashed *after* construction: hash mismatch is not what
    // rejects these witnesses. The first extension points to an inline leaf
    // rather than the mandatory branch; the second reaches the key bound.
    let extension_leaf = vec![0xc4, 0x10, 0xc2, 0x31, 0x01];
    let extension_end = vec![0xc6, 0x82, 0x00, 0x01, 0xc2, 0x20, 0x01];
    let short_leaf = vec![0xc2, 0x20, 0x01];
    let long_leaf = vec![0xc4, 0x82, 0x20, 0x00, 0x01];
    let odd_leaf = vec![0xc2, 0x31, 0x01];
    let bounded = |root, lower: &[u8], upper: &[u8], nodes: &[Vec<u8>], max_key_len| {
        arkiv_db_trie::walk_interval(
            root,
            &KeyRange::closed(lower, upper),
            nodes,
            IntervalLimits {
                max_entries: 10,
                max_key_len,
            },
        )
    };
    for malformed in [extension_leaf, extension_end, odd_leaf] {
        assert!(bounded(keccak256(&malformed), &[0], &[255], &[malformed], 1).is_err());
    }
    // A leaf for the empty key is well formed (the key sorts below `00`).
    assert_eq!(
        bounded(
            keccak256(&short_leaf),
            &[0],
            &[255],
            std::slice::from_ref(&short_leaf),
            1
        ),
        Ok(vec![])
    );
    assert_eq!(
        bounded(keccak256(&short_leaf), &[], &[255], &[short_leaf], 1),
        Ok(vec![(vec![], vec![1])])
    );
    // `long_leaf` holds the one-byte key `00`: over a zero-byte bound
    // wherever it lies, and in range for a one-byte bound.
    assert!(
        bounded(
            keccak256(&long_leaf),
            &[0],
            &[255],
            std::slice::from_ref(&long_leaf),
            0
        )
        .is_err()
    );
    assert_eq!(
        bounded(
            keccak256(&long_leaf),
            &[0],
            &[255],
            std::slice::from_ref(&long_leaf),
            1
        ),
        Ok(vec![(vec![0], vec![1])])
    );
    // `00` is a proper prefix of `00 00`, so it sorts below it.
    assert_eq!(
        bounded(keccak256(&long_leaf), &[0, 0], &[255, 255], &[long_leaf], 1),
        Ok(vec![])
    );
    // A nonempty interval under a non-empty root needs the root node.
    assert!(walk_interval(keccak256([]), &[], &[], &[] as &[Vec<u8>], 1).is_err());
    assert!(walk_interval(keccak256([]), &[0], &[0, 1], &[] as &[Vec<u8>], 1).is_err());
}

#[test]
fn random_untrusted_bytes_and_root_rebinding_never_panic() {
    let mut map = MerkleMap::<[u8; 2], Vec<u8>>::new();
    for n in 0u16..80 {
        map = map
            .insert(
                &n.to_be_bytes(),
                vec![n as u8; 40],
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
    }
    let lower = 17u16.to_be_bytes();
    let upper = 61u16.to_be_bytes();
    let proof = map
        .interval_proof(&KeyRange::closed(&lower, &upper), &MemoryOnly)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for index in 0..proof.len() {
        for at in (0..proof[index].len()).step_by(7) {
            let mut mutated = proof.clone();
            mutated[index][at] ^= 255;
            assert!(walk_interval(map.root_hash(), &lower, &upper, &mutated, 80).is_err());
            // Rebind the trusted root as well, exercising canonical decoding even
            // when a malformed root is self-consistent rather than a hash mismatch.
            let _ = walk_interval(keccak256(&mutated[0]), &lower, &upper, &mutated, 80);
        }
    }
    let mut seed = 7u64;
    for len in 0..512 {
        let bytes = (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect::<Vec<_>>();
        let _ = walk_interval(keccak256(&bytes), &lower, &upper, &[bytes], 80);
    }
}
