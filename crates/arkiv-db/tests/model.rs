use arkiv_db::{Database, Entry, WriteBatch};

const KEYS: &[&[u8]] = &[
    b"",
    b"\0",
    b"\0\0",
    b"\0\xff",
    b"\x01",
    b"a",
    b"a\0",
    b"a/",
    b"a/\0",
    b"a/b",
    b"a\xff",
    b"\x7f",
    b"\x80",
    b"\xfe",
    b"\xff",
    b"\xff\0",
    b"\xff\xff",
    b"\xff\xff\0",
];

// This oracle records intentions independently of WriteBatch's representation.
// It deliberately uses neither an ordered map nor incremental map updates.
#[derive(Debug)]
enum Mutation {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

impl Mutation {
    fn key(&self) -> &[u8] {
        match self {
            Self::Put(key, _) | Self::Delete(key) => key,
        }
    }

    fn value(&self) -> Option<&[u8]> {
        match self {
            Self::Put(_, value) => Some(value),
            Self::Delete(_) => None,
        }
    }
}

// A block is test workload metadata for now: the database receives one batch
// and exposes only the resulting head. It does not yet retain block snapshots.
struct BlockPacket {
    number: usize,
    mutations: Vec<Mutation>,
}

impl BlockPacket {
    fn batch(&self) -> WriteBatch {
        let mut batch = WriteBatch::new();
        for mutation in &self.mutations {
            match mutation {
                Mutation::Put(key, value) => {
                    batch.put(key.clone(), value.clone());
                }
                Mutation::Delete(key) => {
                    batch.delete(key.clone());
                }
            }
        }
        batch
    }
}

struct Generator(u32);

impl Generator {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn index(&mut self, length: usize) -> usize {
        self.next() as usize % length
    }

    fn value(&mut self) -> Vec<u8> {
        let lengths = [0, 1, 2, 7, 32, 257];
        let length = lengths[self.index(lengths.len())];
        (0..length).map(|_| self.next() as u8).collect()
    }

    fn block(&mut self, number: usize) -> BlockPacket {
        // Exercise realistic multi-operation boundaries, plus empty and
        // singleton blocks. Every seed sees every size repeatedly.
        let sizes = [0, 1, 2, 8, 32, 128];
        let size = sizes[(number - 1) % sizes.len()];
        let key_index = self.index(KEYS.len());
        let key = KEYS[key_index];
        let other = KEYS[(key_index + 1) % KEYS.len()];
        let mutations = (0..size)
            .map(|operation| match operation % 8 {
                // Force put/delete/recreate and replacement within a block.
                0 | 3 | 4 => Mutation::Put(key.to_vec(), self.value()),
                1 => Mutation::Put(other.to_vec(), self.value()),
                2 => Mutation::Delete(key.to_vec()),
                5 => Mutation::Delete(other.to_vec()),
                _ => {
                    let key = KEYS[self.index(KEYS.len())];
                    if self.index(3) == 0 {
                        Mutation::Delete(key.to_vec())
                    } else {
                        Mutation::Put(key.to_vec(), self.value())
                    }
                }
            })
            .collect();
        BlockPacket { number, mutations }
    }
}

fn prefix_cases() -> Vec<Vec<u8>> {
    let mut prefixes = vec![b"absent".to_vec(), b"\xff\xff\xff".to_vec()];
    for key in KEYS {
        for length in 0..=key.len() {
            prefixes.push(key[..length].to_vec());
        }
        let mut longer = key.to_vec();
        longer.extend_from_slice(b"\x55\x55");
        prefixes.push(longer);
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

fn assert_model(
    db: &Database,
    history: &[Mutation],
    prefixes: &[Vec<u8>],
    seed: u32,
    block: usize,
    phase: &str,
) {
    let mut expected = Vec::new();
    for key in KEYS.iter().copied().chain([b"absent".as_slice()]) {
        let value = history
            .iter()
            .rev()
            .find(|mutation| mutation.key() == key)
            .and_then(Mutation::value)
            .map(<[u8]>::to_vec);
        assert_eq!(
            db.get(key).unwrap(),
            value,
            "seed={seed:#010x}, block={block}, phase={phase}, mutations={}, key={key:?}",
            history.len(),
        );
        if let Some(value) = value {
            expected.push(Entry {
                key: key.to_vec(),
                value,
            });
        }
    }
    expected.sort_by(|left, right| left.key.cmp(&right.key));

    for prefix in prefixes {
        let matching: Vec<_> = expected
            .iter()
            .filter(|entry| entry.key.starts_with(prefix))
            .cloned()
            .collect();
        assert_eq!(
            db.scan_prefix(prefix).unwrap(),
            matching,
            "seed={seed:#010x}, block={block}, phase={phase}, mutations={}, prefix={prefix:?}",
            history.len(),
        );
    }
}

#[test]
fn block_packets_match_a_log_model_before_and_after_each_block() {
    let prefixes = prefix_cases();
    for seed in [1, 0x9e37_79b9, 0xdead_beef, 0xa5a5_a5a5] {
        let mut generator = Generator(seed);
        let mut db = Database::in_memory();
        let mut history = Vec::new();
        assert_model(&db, &history, &prefixes, seed, 0, "initial");

        for number in 1..=256 {
            let block = generator.block(number);
            let batch = block.batch();
            // Preparing a whole candidate must not publish any of its writes.
            assert_model(&db, &history, &prefixes, seed, block.number, "staged");

            if number % 11 == 0 {
                // An unsubmitted candidate at this boundary leaves the prior
                // block intact; the actual block below is submitted once.
                let abandoned = block.batch();
                drop(abandoned);
                assert_model(&db, &history, &prefixes, seed, block.number, "discarded");
            }

            db.write_batch(batch).unwrap();
            history.extend(block.mutations);
            assert_model(&db, &history, &prefixes, seed, block.number, "applied");
        }
    }
}
