//! Run: cargo run -p arkiv-node --features arkiv-v2 --example gas_calibration
#[cfg(feature = "arkiv-v2")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use arkiv_db::{
        Budget, CellName, Engine, Limits, NamespaceName, OwnerId, Projection, RecordKey, Value,
        WriteOp, encode,
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
    for (label, payload, days, attributes) in [
        ("small", 256, 1, 2),
        ("standard-week", 1024, 7, 4),
        ("standard-month", 1024, 30, 4),
        ("document-week", 16 * 1024, 7, 4),
        ("maximum-payload-month", 128 * 1024, 30, 4),
    ] {
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
            max_lifetime: 6_307_200,
            expiry_per_block: 16,
        };
        let mut attrs = vec![
            (CellName::new("$payload")?, Value::bytes(&vec![0; payload])),
            (
                CellName::new("$contentType")?,
                Value::str("application/json"),
            ),
        ];
        for index in 0..attributes {
            attrs.push((CellName::new(&format!("attr{index}"))?, Value::U64(index)));
        }
        let operation = ArkivOperation::Create {
            salt: 7,
            flags: 0,
            expiry: Expiry {
                expires_at: 1 + days * gas::RENT_PERIOD_BLOCKS,
                min_lifetime: 0,
            },
            attributes: attrs,
        };
        let outcome = execute_operation(
            &mut branch,
            entity_namespace(),
            &ctx,
            &operation,
            Budget::unlimited(),
        )?;
        let OpOutcome::Applied { receipt, .. } = outcome else {
            panic!("{outcome:?}");
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
        let rent = gas::rent(
            0,
            units.total,
            None,
            Some(1 + days * gas::RENT_PERIOD_BLOCKS),
            1,
        )?
        .gas;
        let total = gas::TX_INTRINSIC_GAS + receipt.spent.units();
        results.push(serde_json::json!({"example":label,"payloadBytes":payload,"userAttributes":attributes,"lifetimeBlocks":days*gas::RENT_PERIOD_BLOCKS,"logicalStorageUnits":units.total,"canonicalKeyAndCellsBytes":encode(&(key,row.cells)).len(),"executionGas":total-rent,"rentGas":rent,"totalGas":total,"referenceGlm":gas::reference_fee_glm(total)}));
    }
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}
#[cfg(not(feature = "arkiv-v2"))]
fn main() {
    eprintln!("requires --features arkiv-v2");
}
