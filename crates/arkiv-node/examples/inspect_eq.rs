//! Reproducible, in-memory inspection fixture; no service or stored run is touched.
//! `cargo run -q -p arkiv-node --example inspect_eq -- 20 u64 1 2`
use arkiv_db::durable::Options;
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::simulator::{
    host::{Host, RunConfig},
    light::{Light, VerifiedRequest},
    workload::Workload,
};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() > 4 {
        return Err("usage: inspect_eq [height [valueType [value [limit]]]]".into());
    }
    let height = args.first().map_or("20", String::as_str).parse::<u64>()?;
    if !(1..=200).contains(&height) {
        return Err("fixture height must be between 1 and 200".into());
    }
    let value_type = args.get(1).map_or("u64", String::as_str);
    let value = args.get(2).map_or("1", String::as_str);
    let selection = VerifiedRequest {
        height: height.to_string(),
        namespace: "1".into(),
        attribute: "group".into(),
        value_type: value_type.into(),
        value: if value_type == "bool" {
            serde_json::json!(value.parse::<bool>()?)
        } else {
            serde_json::json!(value)
        },
        limit: args.get(3).map_or("2", String::as_str).parse()?,
        cursor: None,
    };
    let mut full = Host::create(
        Arc::new(MemoryVersionedStore::new()),
        RunConfig {
            genesis: arkiv_node::demo::workload::genesis(9001, 1700000000000),
            workload: Workload::default(),
        },
        [1; 16],
        [7; 16],
        Options::default(),
    )?;
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        full.durable_identity.clone(),
    )?;
    for _ in 0..height {
        full.produce(None)?;
        light.append(&[full.head], &full.head)?;
    }
    let (request, previous) = light.request(&selection)?;
    let proof = full.proof(&request)?;
    let inspected = light.inspect(&selection, &request, previous.as_ref(), &proof)?;
    println!("{}", serde_json::to_string_pretty(&inspected)?);
    Ok(())
}
