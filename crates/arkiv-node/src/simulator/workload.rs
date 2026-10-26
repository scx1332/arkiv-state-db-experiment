//! Reproducible Arkiv entity trace with a configurable size of its unindexed
//! `bytes` column (`payload_bytes`, kept as the workload's knob name).
use super::{Error, Result};
use arkiv_db::{
    Cell, CellName, Decode, DecodeError, Decoder, Encode, Encoder, RecordKey, Value, digest,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workload {
    pub version: u32,
    pub seed: String,
    pub block_period_ms: String,
    pub payload_bytes: u32,
}
impl Default for Workload {
    fn default() -> Self {
        Self {
            version: arkiv_db::LAYOUT_VERSION,
            seed: "7".into(),
            block_period_ms: "1000".into(),
            payload_bytes: 64,
        }
    }
}
impl Workload {
    /// Finite, reproducible Arkiv fixture followed by empty advancing blocks.
    /// Fixed history makes samples repeatable while headers continue to advance.
    pub fn inputs(&self, chain: u64, height: u64) -> Result<crate::BlockInputs> {
        use crate::demo::workload::{ALICE, BOB};
        use crate::{Expiry, TxOp, arkiv::ArkivOperation as A};
        self.validate()?;
        let ns = crate::arkiv::entity_namespace();
        let cell = |name: &str, value| (CellName::new(name).unwrap(), Cell::attribute(value));
        let field = |name: &str, value| (CellName::new(name).unwrap(), Cell::field(value));
        let key = |n| {
            RecordKey::new(&crate::arkiv::derive_entity_key(
                chain,
                ALICE.as_bytes(),
                n,
                n as u128,
            ))
            .unwrap()
        };
        let mut actor = ALICE;
        let operations = match height {
            1 => vec![TxOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("entities").unwrap(),
            }],
            2 => (0..5)
                .map(|slot| TxOp::Arkiv {
                    namespace: ns,
                    operation: A::Create {
                        salt: slot as u128,
                        flags: if slot == 4 { 2 } else { 0 },
                        expiry: if slot == 4 {
                            Expiry {
                                expires_at: 14,
                                min_lifetime: 0,
                            }
                        } else {
                            Expiry::PERMANENT
                        },
                        attributes: vec![
                            cell("group", Value::U64(1)),
                            cell("title", Value::str(&format!("Arkiv entity {}", slot + 1))),
                            cell("counter", Value::I32(i32::MIN)),
                            cell("quantity", Value::U256([0xff; 32])),
                            cell(
                                "price",
                                Value::Dec(
                                    crate::arkiv::parse_decimal("-12.34567890123456789").unwrap(),
                                ),
                            ),
                            cell("checksum", Value::Bytes32([0x42; 32])),
                            cell("reference", Value::Key([0x77; 32])),
                            field("format", Value::str("application/json")),
                            field(
                                "data",
                                Value::bytes(&vec![b'S'; self.payload_bytes as usize]),
                            ),
                        ],
                    },
                })
                .collect(),
            4 => vec![TxOp::Arkiv {
                namespace: ns,
                operation: A::TransferOwnership {
                    key: key(0),
                    owner: *BOB.as_bytes(),
                },
            }],
            5 => {
                actor = BOB;
                vec![TxOp::Arkiv {
                    namespace: ns,
                    operation: A::Patch {
                        key: key(0),
                        set: vec![cell("title", Value::str("Transferred and updated"))],
                        remove: vec![],
                    },
                }]
            }
            6 => {
                actor = BOB;
                vec![TxOp::Arkiv {
                    namespace: ns,
                    operation: A::ExtendExpiry {
                        key: key(4),
                        expiry: Expiry {
                            expires_at: 14,
                            min_lifetime: 0,
                        },
                    },
                }]
            }
            8 => vec![TxOp::Arkiv {
                namespace: ns,
                operation: A::Delete { key: key(3) },
            }],
            _ => vec![],
        };
        let txs = if operations.is_empty() {
            vec![]
        } else {
            let mut e = Encoder::new();
            e.u64(chain);
            e.u64(height);
            e.str(&self.seed);
            vec![crate::TxEnvelope {
                chain_id: chain,
                actor,
                request_id: digest("arkiv/simulator-workload/v1", &e.into_bytes()).0,
                tx_budget: 50_000_000,
                ops: operations,
            }]
        };
        Ok(crate::BlockInputs { admin: vec![], txs })
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != arkiv_db::LAYOUT_VERSION
            || super::wire::number(&self.block_period_ms)? == 0
            || self.payload_bytes > 131072
        {
            return Err(Error("InvalidRequest"));
        }
        super::wire::number(&self.seed)?;
        Ok(())
    }
    pub fn update_allowed(&self, next: &Self) -> Result<()> {
        next.validate()?;
        if (self.version, &self.seed, &self.block_period_ms)
            != (next.version, &next.seed, &next.block_period_ms)
        {
            return Err(Error("RequestConflict"));
        }
        Ok(())
    }
    pub fn timestamp(&self, genesis: u64, height: u64) -> Result<u64> {
        super::wire::number(&self.block_period_ms)?
            .checked_mul(height)
            .and_then(|n| n.checked_add(genesis))
            .ok_or(Error("CapacityReached"))
    }
}
impl Encode for Workload {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(self.version);
        e.str(&self.seed);
        e.str(&self.block_period_ms);
        e.u32(self.payload_bytes);
    }
}
impl Decode for Workload {
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let w = Self {
            version: d.u32()?,
            seed: d.str()?.into(),
            block_period_ms: d.str()?.into(),
            payload_bytes: d.u32()?,
        };
        w.validate().map_err(|_| DecodeError::Invalid {
            kind: "SimulatorWorkload",
            reason: "unsupported workload",
        })?;
        Ok(w)
    }
}
