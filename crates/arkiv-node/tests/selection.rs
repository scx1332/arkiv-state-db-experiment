//! Block selection: admin inputs first, strict FIFO transactions under the
//! count, operation, byte and budget limits, the replay-side check, and the
//! guarantee that a largest valid transaction fits an otherwise empty block.
//!
//! Expected byte sizes are derived from the layouts: a `Put` operation is
//! `1 + 8 + (4 + key) + (4 + value)` bytes, an envelope `8 + 20 + 32 + 8 +
//! 4` bytes plus its operations, an admin input with a schedule 309 bytes,
//! and block inputs 8 bytes plus their items.

mod support;

use arkiv_db::{PriceSchedule, encode};
use arkiv_node::{
    AdminInput, BlockInputs, ChainParams, ChainSpec, Selection, SelectionFault, TxEnvelope, select,
    select::check, select_inputs, validate::check_tx,
};
use support::*;

/// A transaction of `ops` puts (into namespace 2; namespace 1 is the entity
/// namespace) with values of `value` bytes and 1-byte keys:
/// `72 + ops × (1 + 8 + 5 + 4 + value)` bytes.
fn puts(n: u64, budget: u64, ops: usize, value: usize) -> TxEnvelope {
    let ops = (0..ops).map(|_| put(2, b"k", &vec![0x55; value])).collect();
    tx(ALICE, n, budget, ops)
}

fn put_tx_bytes(ops: u64, value: u64) -> u64 {
    72 + ops * (18 + value)
}

fn admins(count: u64) -> Vec<AdminInput> {
    (0..count)
        .map(|n| {
            set_pricing(
                n,
                PriceSchedule::FIXTURE.with_id(10 + u32::try_from(n).unwrap()),
            )
        })
        .collect()
}

#[test]
fn byte_sizes_follow_the_layouts() {
    let envelope = puts(1, 10, 3, 7);
    assert_eq!(encode(&envelope).len() as u64, put_tx_bytes(3, 7));
    let admin = admins(2);
    let inputs = BlockInputs {
        admin: admin.clone(),
        txs: vec![envelope.clone(), puts(2, 10, 1, 0)],
    };
    let expected = 8 + 2 * 309 + put_tx_bytes(3, 7) + put_tx_bytes(1, 0);
    assert_eq!(encode(&inputs).len() as u64, expected);
    let selection = select(&PARAMS, &admin, &inputs.txs);
    assert_eq!(
        selection,
        Selection {
            admin: 2,
            txs: 2,
            bytes: expected,
            ops: 4,
            budget: 20,
        }
    );
}

#[test]
fn admin_inputs_come_first_up_to_their_limit() {
    let queue = admins(3);
    let selected = select_inputs(&PARAMS, &queue, &[]);
    assert_eq!(selected.admin, queue[..2].to_vec());
    assert!(selected.txs.is_empty());
    assert_eq!(check(&PARAMS, &selected), Ok(()));
    let over = BlockInputs {
        admin: queue,
        txs: Vec::new(),
    };
    assert_eq!(check(&PARAMS, &over), Err(SelectionFault::AdminCount));
}

#[test]
fn selection_stops_at_the_first_transaction_that_does_not_fit() {
    // Budget: 60,000 + 50,000 > 100,000, so the second transaction stops
    // the selection even though the third (1,000) would fit.
    let queue = vec![
        puts(1, 60_000, 1, 0),
        puts(2, 50_000, 1, 0),
        puts(3, 1_000, 1, 0),
    ];
    let selection = select(&PARAMS, &[], &queue);
    assert_eq!((selection.txs, selection.budget), (1, 60_000));
    let over = BlockInputs {
        admin: Vec::new(),
        txs: queue[..2].to_vec(),
    };
    assert_eq!(check(&PARAMS, &over), Err(SelectionFault::Budget));

    // Operations: 8 + 8 + 8 of at most 20 in a block.
    let params = ChainParams {
        max_ops_per_block: 20,
        ..PARAMS
    };
    let queue = vec![
        puts(1, 10, 8, 0),
        puts(2, 10, 8, 0),
        puts(3, 10, 8, 0),
        puts(4, 10, 1, 0),
    ];
    let selection = select(&params, &[], &queue);
    assert_eq!((selection.txs, selection.ops), (2, 16));
    let over = BlockInputs {
        admin: Vec::new(),
        txs: queue[..3].to_vec(),
    };
    assert_eq!(check(&params, &over), Err(SelectionFault::OpCount));

    // Count: at most 2 transactions.
    let params = ChainParams {
        max_txs_per_block: 2,
        ..PARAMS
    };
    assert_eq!(select(&params, &[], &queue).txs, 2);
    assert_eq!(check(&params, &over), Err(SelectionFault::TxCount));
}

#[test]
fn the_byte_limit_is_the_encoded_inputs() {
    // Two transactions of 72 + 18 + 100 = 190 bytes: the inputs are 8 + 190
    // + 190 = 388 bytes. A limit of 388 takes both, 387 only the first.
    let queue = vec![puts(1, 10, 1, 100), puts(2, 10, 1, 100)];
    assert_eq!(put_tx_bytes(1, 100), 190);
    for (limit, taken) in [(388, 2), (387, 1), (198, 1), (197, 0)] {
        let params = ChainParams {
            max_block_input_bytes: limit,
            ..PARAMS
        };
        let selection = select(&params, &[], &queue);
        assert_eq!(selection.txs, taken, "limit {limit}");
        let chosen = select_inputs(&params, &[], &queue);
        assert_eq!(encode(&chosen).len() as u64, selection.bytes);
        assert_eq!(check(&params, &chosen), Ok(()));
        let one_more = BlockInputs {
            admin: Vec::new(),
            txs: queue[..(taken + 1).min(2)].to_vec(),
        };
        if taken < 2 {
            assert_eq!(check(&params, &one_more), Err(SelectionFault::Bytes));
        }
    }
}

/// Under the genesis invariants a transaction of the largest valid shape —
/// `max_tx_bytes` bytes, `max_ops_per_tx` operations, the whole user budget
/// — fits a block that already holds a full admin section, for parameter
/// sets at the invariants' edges.
#[test]
fn a_largest_transaction_always_fits_an_otherwise_empty_block() {
    let edges = [
        PARAMS,
        ChainParams {
            max_ops_per_tx: 8,
            max_ops_per_block: 8,
            max_txs_per_block: 1,
            max_admin_per_block: 3,
            max_tx_bytes: 1_000,
            max_block_input_bytes: 8 + 3 * 309 + 1_000,
            ..PARAMS
        },
        ChainParams {
            max_ops_per_tx: 1,
            max_ops_per_block: 1,
            max_txs_per_block: 1,
            max_admin_per_block: 0,
            max_tx_bytes: 90,
            max_block_input_bytes: 98,
            ..PARAMS
        },
    ];
    for params in edges {
        let spec = ChainSpec::new(genesis_with(params, LIMITS, PriceSchedule::FIXTURE))
            .expect("parameters satisfy the invariants");
        // Fill max_ops_per_tx puts, the last one padded to exactly
        // max_tx_bytes.
        let ops = u64::from(params.max_ops_per_tx);
        let base = put_tx_bytes(ops, 0);
        let pad = u64::from(params.max_tx_bytes) - base;
        let mut envelope = puts(1, params.user_budget, usize::try_from(ops).unwrap(), 0);
        let last = envelope.ops.len() - 1;
        envelope.ops[last] = put(2, b"k", &vec![0x55; usize::try_from(pad).unwrap()]);
        assert_eq!(
            encode(&envelope).len() as u64,
            u64::from(params.max_tx_bytes)
        );
        assert_eq!(check_tx(&spec, &envelope), Ok(()));
        let admin = admins(u64::from(params.max_admin_per_block));
        let selection = select(&params, &admin, std::slice::from_ref(&envelope));
        assert_eq!(
            (selection.admin, selection.txs),
            (admin.len(), 1),
            "{params:?}"
        );
        assert_eq!(
            check(&params, &select_inputs(&params, &admin, &[envelope])),
            Ok(())
        );
    }
}
