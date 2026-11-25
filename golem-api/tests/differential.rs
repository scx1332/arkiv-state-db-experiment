//! The adapter against golem-db's own engine, call by call.
//!
//! Seeded random scripts of every record and branch call run on golem-db's
//! memory `Database` and on the adapter over both arkiv backends. Each call's
//! result (or error, with its message), pricing commit, effect counters and
//! `BranchInfo` must be the same on all three. Roots are not compared: the
//! two engines hash different structures.

mod common;

use common::*;

const NAMES: [&str; 9] = [
    "a",
    "b",
    "n",
    "s",
    "$sys",
    "with.dots-and:colons_",
    "9bad",
    "bad name",
    "a-name-longer-than-thirty-two-bytes",
];

fn cell(rng: &mut Rng) -> CellValue {
    match rng.below(20) {
        // Over the limits: a 65-byte str and a 129-byte bytes value.
        0 => CellValue::from_str(&"x".repeat(65)),
        1 => CellValue::from_bytes(&[7; 129]),
        2 => CellValue::from_bytes(&[1, 2, 3]),
        _ => {
            let ty = rng.below(TYPES);
            value(rng, ty)
        }
    }
}

fn create(rng: &mut Rng, key: RecordKey) -> RecordOp<op::Create> {
    let mut op = RecordOp::create(key, B);
    for _ in 0..rng.below(5) {
        let (name, value) = (*rng.pick(&NAMES[..5]), cell(rng));
        // Mostly valid names, and now and then any name at all.
        let name = if rng.chance(6) {
            *rng.pick(&NAMES)
        } else {
            name
        };
        op = if rng.chance(60) {
            op.attribute(name, value)
        } else {
            op.field(name, value)
        };
    }
    op
}

fn patch(rng: &mut Rng, key: RecordKey) -> RecordOp<op::Patch> {
    let mut op = RecordOp::patch(key, B);
    for _ in 0..rng.below(4) {
        let name = if rng.chance(6) {
            *rng.pick(&NAMES)
        } else {
            *rng.pick(&NAMES[..5])
        };
        op = match rng.below(4) {
            0 => op.attribute(name, cell(rng)),
            1 => op.field(name, cell(rng)),
            2 => op.set(name, cell(rng)),
            _ => op.remove(name),
        };
    }
    op
}

fn projection(rng: &mut Rng, op: RecordOp<op::Get>) -> RecordOp<op::Get> {
    match rng.below(6) {
        0 => op.only(Vec::<&str>::new()),
        1 => op.only(["a", "#key"]),
        2 => op.only(["#meta", "missing"]),
        3 => op.only(["n", "s", "n"]),
        _ => op,
    }
}

/// One script: `steps` calls drawn from `seed`, run in lockstep.
fn script(seed: u64, steps: usize) {
    let backends = Backends::new();
    let all = backends.all();
    let mut rng = Rng(seed);
    // The same logical branches on every backend; ids differ between them.
    let mut branches: Vec<[BranchId; 3]> = Vec::new();
    let mut head = 0u64;
    for step in 0..steps {
        // Mostly live branches: a consumed or stale one is usually replaced,
        // and sometimes kept to exercise the handle errors.
        let live = |branch: &[BranchId; 3]| backends.golem.branch_info(branch[0]).is_ok();
        if rng.chance(85) {
            branches.retain(live);
        }
        if branches.is_empty() || rng.chance(6) {
            let begun = all.map(|(_, api)| api.begin().expect("begin"));
            branches.push(begun);
            continue;
        }
        let at = rng.below(branches.len());
        let branch = branches[at];
        let record = match rng.below(12) {
            0 => reserved_key("#alloc"),
            n => key(1 + (n % 6) as u8),
        };
        let choice = rng.below(100);
        // Each closure builds the same call for backend `i` and renders
        // everything a caller can observe of it.
        let seed_of_call = rng.next();
        let run = |i: usize| -> String {
            let (_, api) = all[i];
            let b = branch[i];
            let mut rng = Rng(seed_of_call);
            match choice {
                0..=24 => outcome(api.create(b, create(&mut rng, record))),
                25..=44 => outcome(api.patch(b, patch(&mut rng, record))),
                45..=52 => outcome(api.delete(b, RecordOp::delete(record, B))),
                53..=62 => {
                    let get = projection(&mut rng, RecordOp::get(record, B));
                    outcome(api.get(ReadTarget::Branch(b), get))
                }
                63..=68 => {
                    let get = projection(&mut rng, RecordOp::get(record, B));
                    let target = match rng.below(4) {
                        0 => ReadTarget::Commit(CommitId::new(head)),
                        1 => ReadTarget::Commit(CommitId::new(head + 3)),
                        2 => ReadTarget::Commit(CommitId::new(head.saturating_sub(1))),
                        _ => ReadTarget::Head,
                    };
                    outcome(api.get(target, get))
                }
                69..=72 => {
                    // The reserved records the adapter derives: the
                    // allocator and the key bindings, staged and committed.
                    let (name, only) = match rng.below(4) {
                        0 => ("#alloc", None),
                        1 => ("#recordKeys", None),
                        2 => ("#recordKeys", Some(record)),
                        _ => ("#params", None),
                    };
                    let mut get = RecordOp::get(reserved_key(name), B);
                    if let Some(only) = only {
                        get = get.only([only.0.as_slice()]);
                    }
                    let target = if rng.chance(50) {
                        ReadTarget::Branch(b)
                    } else {
                        ReadTarget::Head
                    };
                    outcome(api.get(target, get))
                }
                73..=78 => format!("{:?}", api.checkpoint(b)),
                79..=84 => format!("{:?}", api.rollback(b)),
                85..=87 => format!("{:?}", api.seal(b).map(|sealed| sealed.commit_id)),
                88..=93 => format!("{:?}", api.commit(b)),
                94..=95 => format!("{:?}", api.discard(b)),
                _ => format!("{:?}", api.head()),
            }
        };
        let expected = run(0);
        if std::env::var_os("KIT_TRACE").is_some() {
            eprintln!("{choice:3} {}", &expected[..expected.len().min(60)]);
        }
        for (i, (name, _)) in all.iter().enumerate().skip(1) {
            assert_eq!(
                run(i),
                expected,
                "seed {seed}, step {step}, call {choice} on {name}"
            );
        }
        // After every call the branch looks the same: its base, its undo
        // entries (golem-db's `version`) and whether it is sealed.
        let info = |i: usize| {
            all[i]
                .1
                .branch_info(branch[i])
                .map(|info| (info.commit_id, info.version, info.sealed))
        };
        let expected = format!("{:?}", info(0));
        for (i, (name, _)) in all.iter().enumerate().skip(1) {
            assert_eq!(
                format!("{:?}", info(i)),
                expected,
                "seed {seed}, step {step}, branch_info after call {choice} on {name}"
            );
        }
        head = backends.golem.head().expect("head").get();
        for (name, api) in &all[1..] {
            assert_eq!(api.head().expect("head").get(), head, "{name}");
        }
    }
    // The committed state is the same, record by record.
    for n in 1..=6 {
        let read = |api: &dyn Api| outcome(api.get(ReadTarget::Head, RecordOp::get(key(n), B)));
        let expected = read(all[0].1);
        for (name, api) in &all[1..] {
            assert_eq!(read(*api), expected, "seed {seed}, final key {n} on {name}");
        }
    }
}

#[test]
fn random_scripts_agree_with_golem_db() {
    for seed in 0..40 {
        script(seed, 250);
    }
}

/// Every golem-db cell type, as an attribute and as a field, reads back as
/// golem-db returns it, `#meta` included.
#[test]
fn every_cell_type_round_trips() {
    let backends = Backends::new();
    let values = [
        CellValue::from_bool(true),
        CellValue::from_str("text \u{1F600}"),
        CellValue::from_bytes20([0x14; 20]),
        CellValue::from_bytes4([4; 4]),
        CellValue::from_bytes8([8; 8]),
        CellValue::from_bytes16([16; 16]),
        CellValue::from_bytes32([32; 32]),
        CellValue::from_u32(u32::MAX),
        CellValue::from_u64(u64::MAX),
        CellValue::from_u128(u128::MAX),
        CellValue::from_u256_be([0xfe; 32]),
        CellValue::from_i32(i32::MIN),
        CellValue::from_i64(i64::MIN),
        CellValue::from_i128(i128::MIN),
        CellValue::from_i256_be([0x80; 32]),
        CellValue::from_dec32_unscaled(-1),
        CellValue::from_dec64_unscaled(i64::MAX),
        CellValue::from_dec128_unscaled(-7),
        CellValue::from_dec256_unscaled_be([0x7f; 32]),
        CellValue::from_f32(-0.0).unwrap(),
        CellValue::from_f64(f64::NEG_INFINITY).unwrap(),
        CellValue::from_date32(-1),
        CellValue::from_timestamp64(i64::MAX),
    ];
    let caps = RecordLimits {
        max_cells: 64,
        max_indexed_cells: 64,
    };
    let genesis = Genesis::new(
        HashAlgorithm::Keccak256,
        LIMITS,
        caps,
        None,
        PricingConfig::zero(),
    );
    let golem = real::Database::open_memory(&genesis).unwrap();
    let arkiv = ArkivDb::open_memory(&genesis, ArkivOptions::default()).unwrap();
    drop(backends);
    let write = |api: &dyn Api| {
        let branch = api.begin().unwrap();
        let mut op = RecordOp::create(key(1), B).field("blob", CellValue::from_bytes(&[9; 100]));
        for (i, value) in values.iter().enumerate() {
            op = op
                .attribute(&format!("a{i}"), value.clone())
                .field(&format!("f{i}"), value.clone());
        }
        let created = outcome(api.create(branch, op));
        api.commit(branch).unwrap();
        (
            created,
            outcome(api.get(ReadTarget::Head, RecordOp::get(key(1), B))),
        )
    };
    assert_eq!(write(&arkiv), write(&golem));
}
