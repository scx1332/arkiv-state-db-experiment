//! Reproducible twenty-block workload. Inputs depend only on seed and height.
//!
//! Block 1 creates the Arkiv entity namespace (ordinal 1, host-controlled)
//! and the generic namespaces `alpha` (2) and `beta` (3); the generic record
//! traffic uses `alpha` and `beta` only.
use crate::{BlockInputs, ChainParams, Expiry, Genesis, TxEnvelope, TxOp};
use arkiv_db::{
    Cell, CellName, Limits, NamespaceId, NamespaceName, OwnerId, PriceSchedule, RecordKey,
    Revision, Value,
};
pub const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
pub const BOB: OwnerId = OwnerId::new([0xb0; 20]);
pub const DEFAULT_SEED: u64 = 7;
pub fn genesis(chain_id: u64, timestamp_ms: u64) -> Genesis {
    Genesis {
        format_version: crate::block::FORMAT_VERSION,
        protocol_version: crate::block::PROTOCOL_VERSION,
        chain_id,
        timestamp_ms,
        limits: Limits {
            namespace_cap: 1024,
            max_cells: 32,
            max_page: 64,
            max_groups: 8,
            max_literals: 8,
            ..Limits::DEFAULT.with_value_len(128 * 1024)
        },
        pricing: PriceSchedule::FIXTURE,
        params: ChainParams {
            block_budget: 1_000_000_000,
            user_budget: 100_000_000,
            expiry_per_block: 16,
            max_admin_per_block: 2,
            max_txs_per_block: 16,
            max_ops_per_tx: 16,
            max_ops_per_block: 128,
            max_tx_bytes: 1024 * 1024,
            max_block_input_bytes: 2 * 1024 * 1024,
            max_lifetime: 1000,
        },
        admins: vec![ALICE],
    }
}
pub fn inputs(chain_id: u64, height: u64, seed: u64) -> BlockInputs {
    let ns = |n| NamespaceId::from_ordinal(n).expect("namespace");
    let round = height / 20;
    let slot = height % 20;
    let key = |kind: &str| {
        let text = format!("{seed}-{round}-{kind}");
        RecordKey::new(text.as_bytes()).unwrap_or_else(|| {
            let digest = arkiv_db::digest("arkiv/demo-key/v1", text.as_bytes());
            RecordKey::new(digest.as_slice()).expect("a 32-byte digest is a record key")
        })
    };
    let cells = |value| {
        vec![
            (CellName::new("group").unwrap(), Cell::attribute(value)),
            (
                CellName::new("note").unwrap(),
                Cell::field(Value::str("retained in memory")),
            ),
        ]
    };
    let create = |kind, value, expiry| TxOp::CreateRecord {
        namespace: ns(2),
        key: key(kind),
        cells: cells(value),
        expiry,
    };
    let patch = |kind, value| TxOp::PatchRecord {
        namespace: ns(2),
        key: key(kind),
        set: vec![(CellName::new("group").unwrap(), Cell::attribute(value))],
        remove: vec![],
        expiry: None,
    };
    let mut actor = ALICE;
    let ops = if height == 1 {
        vec![
            TxOp::CreateNamespace {
                name: NamespaceName::new("entities").unwrap(),
            },
            TxOp::CreateNamespace {
                name: NamespaceName::new("alpha").unwrap(),
            },
            TxOp::CreateNamespace {
                name: NamespaceName::new("beta").unwrap(),
            },
        ]
    } else {
        match slot {
            2 => vec![create(
                "expires",
                Value::U64(1),
                Some(Expiry {
                    expires_at: height + 12,
                    min_lifetime: 0,
                }),
            )],
            3 => vec![create("recreated", Value::U64(1), None)],
            4 => {
                actor = if round.is_multiple_of(2) { ALICE } else { BOB };
                vec![TxOp::CreateRecord {
                    namespace: ns(3),
                    key: key("other"),
                    cells: cells(Value::U64(1)),
                    expiry: None,
                }]
            }
            5 => vec![patch("expires", Value::U64(2))],
            6 => vec![patch("expires", Value::U64(1))],
            7 => vec![
                create("rollback", Value::U64(1), None),
                TxOp::DeleteRecord {
                    namespace: ns(2),
                    key: key("missing"),
                },
            ],
            8 => vec![TxOp::DeleteRecord {
                namespace: ns(2),
                key: key("recreated"),
            }],
            9 => vec![create("recreated", Value::U64(1), None)],
            10 => vec![patch("recreated", Value::str("1"))],
            11 => vec![patch("recreated", Value::U64(1))],
            12 => {
                actor = if round.is_multiple_of(2) { ALICE } else { BOB };
                vec![TxOp::TransferNamespace {
                    namespace: ns(3),
                    expected_revision: Revision(round),
                    new_owner: if round.is_multiple_of(2) { BOB } else { ALICE },
                }]
            }
            15..=18 => vec![create(&format!("stable-{slot}"), Value::U64(1), None)],
            _ => Vec::new(),
        }
    };
    if ops.is_empty() {
        return BlockInputs::default();
    }
    let mut request_id = [0; 32];
    request_id[..8].copy_from_slice(&seed.to_be_bytes());
    request_id[24..].copy_from_slice(&height.to_be_bytes());
    BlockInputs {
        admin: vec![],
        txs: vec![TxEnvelope {
            chain_id,
            actor,
            request_id,
            tx_budget: 10_000,
            ops,
        }],
    }
}
