use alloy_primitives::keccak256;
use arkiv_db_trie::{MemoryOnly, MerkleMap, Probe, walk_interval};

#[test]
fn self_consistent_malformed_intersecting_nodes_are_rejected() {
    // Each root is hashed *after* construction: hash mismatch is not what
    // rejects these witnesses. The first extension points to an inline leaf
    // rather than the mandatory branch; the second reaches the key end.
    let extension_leaf = vec![0xc4, 0x10, 0xc2, 0x31, 0x01];
    let extension_end = vec![0xc6, 0x82, 0x00, 0x01, 0xc2, 0x20, 0x01];
    let short_leaf = vec![0xc2, 0x20, 0x01];
    let long_leaf = vec![0xc4, 0x82, 0x20, 0x00, 0x01];
    for malformed in [extension_leaf, extension_end, short_leaf] {
        assert!(walk_interval(keccak256(&malformed), &[0], &[255], &[malformed], 10).is_err());
    }
    assert!(
        walk_interval(
            keccak256(&long_leaf),
            &[0, 0],
            &[255, 255],
            &[long_leaf],
            10
        )
        .is_err()
    );
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
        .interval_proof(&lower, &upper, &MemoryOnly)
        .unwrap()
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
