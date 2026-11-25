//! Shared setup of the kit's tests: one genesis opened on golem-db's own
//! engine and on both arkiv backends, and a small deterministic generator.
#![allow(dead_code)]

use std::fmt::Debug;

pub use golemdb_api::*;

pub const B: Budget = Budget::Unlimited;

pub const LIMITS: CellLimits = CellLimits {
    max_cell_name_len: 32,
    max_str_len: 64,
    max_bytes_len: 128,
};

pub const CAPS: RecordLimits = RecordLimits {
    max_cells: 12,
    max_indexed_cells: 8,
};

pub fn genesis() -> Genesis {
    Genesis::new(
        HashAlgorithm::Keccak256,
        LIMITS,
        CAPS,
        None,
        PricingConfig::zero(),
    )
}

/// One genesis on golem-db's own engine (the reference) and on the adapter
/// over the in-memory and the durable engine.
pub struct Backends {
    pub golem: real::Database,
    pub memory: ArkivDb,
    pub durable: ArkivDb,
    _dir: tempfile::TempDir,
}

impl Backends {
    pub fn new() -> Self {
        Self::with(ArkivOptions::default())
    }

    pub fn with(options: ArkivOptions) -> Self {
        let genesis = genesis();
        let dir = tempfile::tempdir().expect("temporary directory");
        Self {
            golem: real::Database::open_memory(&genesis).expect("golem-db"),
            memory: ArkivDb::open_memory(&genesis, options).expect("arkiv memory"),
            durable: ArkivDb::open(dir.path(), &OpenConfig::new(genesis), options)
                .expect("arkiv durable"),
            _dir: dir,
        }
    }

    /// The reference first, then the adapter's two backends.
    pub fn all(&self) -> [(&'static str, &dyn Api); 3] {
        [
            ("golem-db", &self.golem),
            ("arkiv-memory", &self.memory),
            ("arkiv", &self.durable),
        ]
    }

    pub fn adapters(&self) -> [&ArkivDb; 2] {
        [&self.memory, &self.durable]
    }
}

/// What a caller sees of a metered call apart from the price: the result,
/// the pricing commit and the effect counters. The adapter prices nothing
/// (cost 0, empty ledger), so those two are not compared.
pub fn outcome<T: Debug>(metered: Metered<T>) -> String {
    format!(
        "{:?} priced_at={:?} details={:?}",
        metered.result, metered.receipt.priced_at, metered.receipt.details
    )
}

/// SplitMix64: deterministic, no dependency.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// True `percent` times in a hundred.
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

pub fn key(n: u8) -> RecordKey {
    RecordKey([n; 32])
}

pub fn reserved_key(name: &str) -> RecordKey {
    let mut key = [0; 32];
    key[..name.len()].copy_from_slice(name.as_bytes());
    RecordKey(key)
}

pub const STRINGS: [&str; 8] = ["", "a", "ab", "abc", "b", "ba", "zz", "a\u{e9}"];
pub const FLOATS: [f64; 6] = [f64::NEG_INFINITY, -1.5, 0.0, 2.25, 1e300, f64::INFINITY];

/// A value of one of the types the tests use, by type index: small domains,
/// so that equalities, ranges and ties all occur.
pub fn value(rng: &mut Rng, ty: usize) -> CellValue {
    match ty {
        0 => CellValue::from_u64(rng.below(12) as u64),
        1 => CellValue::from_str(rng.pick(&STRINGS)),
        2 => CellValue::from_f64(*rng.pick(&FLOATS)).expect("not NaN"),
        3 => CellValue::from_i32(rng.below(11) as i32 - 5),
        4 => CellValue::from_bool(rng.chance(50)),
        5 => CellValue::from_bytes32([rng.below(4) as u8; 32]),
        6 => CellValue::from_date32(rng.below(7) as i32 - 3),
        7 => CellValue::from_dec64_unscaled(rng.below(9) as i64 - 4),
        8 => CellValue::from_u256_be([rng.below(3) as u8; 32]),
        _ => CellValue::from_bytes20([rng.below(3) as u8; 20]),
    }
}

pub const TYPES: usize = 10;

/// The cell type of [`value`]'s type index.
pub fn cell_type(ty: usize) -> CellType {
    value(&mut Rng(0), ty).cell_type()
}
