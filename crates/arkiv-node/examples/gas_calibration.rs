//! Run: cargo run -p arkiv-node --example gas_calibration
//!
//! Each example entity is a user-defined-columns create: an unindexed
//! `bytes` column `body` of the given size, an unindexed `str` column
//! `format`, and the given number of indexed `u64` attributes. Execution is
//! gas at the fixed 1 gwei; rent is GLM of its own (glm-rent-v3), priced per
//! second and capped at the default year, at the SDK's 2-second blocks.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use arkiv_db::{
        Budget, Cell, CellName, Engine, Limits, NamespaceName, OwnerId, Projection, RecordKey,
        Value, WriteOp, encode,
    };
    use arkiv_node::{
        Expiry, OpOutcome,
        arkiv::{
            ArkivExecutionContext, ArkivOperation, derive_entity_key, entity_namespace,
            execute_operation,
        },
        gas,
    };
    let mut results = Vec::new();
    const PERIOD_MS: u64 = 2_000;
    let clock = gas::RentClock::new(PERIOD_MS, gas::DEFAULT_RENT_CAP_SECONDS).unwrap();
    let blocks_per_day = 86_400_000 / PERIOD_MS;
    // Lifetimes in days; `None` is permanent, which pays the one-year cap.
    for (label, body, days, attributes) in [
        ("small", 256, Some(1), 2),
        ("standard-week", 1024, Some(7), 4),
        ("standard-month", 1024, Some(30), 4),
        ("standard-permanent", 1024, None, 4),
        ("document-week", 16 * 1024, Some(7), 4),
        ("maximum-bytes-month", 128 * 1024, Some(30), 4),
        ("maximum-bytes-permanent", 128 * 1024, None, 4),
    ] {
        let expires_at = days.map_or(u64::MAX, |d: u64| 1 + d * blocks_per_day);
        let engine = Engine::with_limits(
            gas::schedule(),
            Limits {
                max_str_len: 128 * 1024,
                max_bytes_len: 128 * 1024,
                ..Limits::DEFAULT
            },
        );
        let mut branch = engine.begin(engine.head(), None)?;
        branch
            .try_execute(
                OwnerId::new([1; 20]),
                WriteOp::CreateNamespace {
                    name: NamespaceName::new("entities")?,
                },
                Budget::unlimited(),
            )?
            .unwrap();
        let ctx = ArkivExecutionContext {
            caller: [1; 20],
            block_height: 1,
            chain_id: 1337,
            rent: clock,
            expiry_per_block: 16,
            // Quoted, not debited: the calibration has no balances.
            payer: None,
            allow_permanent_create: false,
        };
        let mut attrs = vec![
            (
                CellName::new("body")?,
                Cell::field(Value::bytes(&vec![0; body])),
            ),
            (
                CellName::new("format")?,
                Cell::field(Value::str("application/json")),
            ),
        ];
        for index in 0..attributes {
            attrs.push((
                CellName::new(&format!("attr{index}"))?,
                Cell::attribute(Value::U64(index)),
            ));
        }
        let operation = ArkivOperation::Create {
            salt: 7,
            flags: 0,
            expiry: Expiry {
                expires_at,
                min_lifetime: 0,
            },
            attributes: attrs,
        };
        let executed = execute_operation(
            &mut branch,
            entity_namespace(),
            &ctx,
            &operation,
            Budget::unlimited(),
        )?;
        let OpOutcome::Applied { receipt, .. } = executed.outcome else {
            panic!("{executed:?}");
        };
        let key = RecordKey::new(&derive_entity_key(ctx.chain_id, &ctx.caller, 0, 7))
            .expect("32-byte key");
        let row = branch
            .try_get_record(
                entity_namespace(),
                &key,
                &Projection::ALL,
                Budget::unlimited(),
            )?
            .unwrap()
            .value
            .unwrap();
        let units = gas::storage_units(&row.cells)?;
        let rent = gas::rent(0, units.total, None, Some(expires_at), 1, clock)?.wei;
        assert_eq!(executed.rent_wei, alloy_primitives::U256::from(rent));
        let charged = gas::rent_horizon(expires_at, 1, clock.cap_blocks()) - 1;
        let execution = gas::TX_INTRINSIC_GAS + receipt.spent.units();
        let fee = gas::reference_fee_wei(execution);
        results.push(serde_json::json!({"example":label,"bodyBytes":body,"indexedAttributes":attributes,"permanent":days.is_none(),"chargedLifetimeSeconds":charged * PERIOD_MS / 1000,"logicalStorageUnits":units.total,"canonicalKeyAndCellsBytes":encode(&(key,row.cells)).len(),"executionGas":execution,"executionFeeGlm":gas::glm(fee),"rentWei":rent.to_string(),"rentGlm":gas::glm(rent),"totalGlm":gas::glm(fee + rent)}));
    }
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}
