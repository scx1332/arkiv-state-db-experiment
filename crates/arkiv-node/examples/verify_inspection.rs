//! Independently verify a saved HTTP inspection with separately selected trust
//! inputs. This deliberately ignores the endpoint's verification label/graph.
//! See docs/simulator-proof-inspection.md for the four JSON input files.
use arkiv_db::{
    decode_exact,
    proof::{EqRequest, VerifiedEq, verify_eq},
};
use arkiv_node::simulator::{
    host::RunConfig,
    light::{VerifiedRequest, resolve},
    metadata::record_change,
    runtime::GenesisResponse,
    wire::{HeaderDto, Identity, RESPONSE_CAP, hex, number, proof_limits, unhex},
};
use serde_json::{Value, json};
use std::{fs::File, io::Read};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn read_json(path: &str, cap: usize) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err("input exceeds local byte cap".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}").into())
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 4 {
        return Err("usage: verify_inspection RESPONSE.json TRUSTED_GENESIS.json TRUSTED_HEADER_OR_FEED.json ORIGINAL_SELECTION.json".into());
    }
    let response = read_json(&args[0], RESPONSE_CAP)?;
    let genesis: GenesisResponse = serde_json::from_value(read_json(&args[1], 256 * 1024)?)?;
    let header = read_json(&args[2], RESPONSE_CAP)?;
    let header: HeaderDto =
        serde_json::from_value(header.get("header").unwrap_or(&header).clone())?;
    let selection: VerifiedRequest = serde_json::from_value(read_json(&args[3], 64 * 1024)?)?;
    println!("{}", verify(response, genesis, header, selection)?);
    Ok(())
}
fn verify(
    response: Value,
    genesis: GenesisResponse,
    header: HeaderDto,
    selection: VerifiedRequest,
) -> Result<Value> {
    if selection.cursor.is_some() {
        return Err(
            "only first pages: later pages require a locally verified continuation token".into(),
        );
    }
    let identity = Identity {
        source_id: genesis.source_id.clone(),
        run_id: genesis.run_id.clone(),
        genesis_hash: genesis.genesis_hash.clone(),
        chain_id: genesis.chain_id.clone(),
    };
    let stored = genesis.validate(&identity)?;
    let run: RunConfig = decode_exact(&stored.metadata)?;
    let trusted = header.validate(number(&identity.chain_id)?)?;
    let expected = resolve(&identity, &trusted, &selection)?;
    let request: EqRequest = decode_exact(&unhex(string(&response, "canonicalRequest")?, 4096)?)?;
    if request != expected {
        return Err("canonical request differs from independently selected header/query".into());
    }
    for (key, expected) in [
        ("sourceId", &identity.source_id),
        ("runId", &identity.run_id),
        ("genesisHash", &identity.genesis_hash),
        ("chainId", &identity.chain_id),
    ] {
        if string(&response, key)? != expected {
            return Err(format!("response {key} differs from selected simulator identity").into());
        }
    }
    if response["query"]
        != json!({"namespace":selection.namespace,"attribute":selection.attribute,"valueType":selection.value_type,"value":selection.value,"limit":selection.limit})
    {
        return Err("displayed query differs from original selection".into());
    }
    let proof = unhex(string(&response, "canonicalProof")?, 1024 * 1024)?;
    let VerifiedEq::Page(page) =
        verify_eq(&expected, None, &proof, proof_limits(), run.genesis.limits)?
    else {
        return Err("namespace absent".into());
    };
    let rows = page
        .rows
        .into_iter()
        .map(|row| record_change(expected.query.namespace, row))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if response["rows"] != serde_json::to_value(&rows)?
        || response["postingCount"] != json!(page.posting_count)
    {
        return Err("displayed rows/count differ from cryptographically verified page".into());
    }
    if response["snapshot"]
        != json!({"height":trusted.height.to_string(),"hash":hex(trusted.hash().as_slice()),"stateRoot":hex(expected.snapshot.state_root.as_slice())})
    {
        return Err("displayed snapshot differs from independently selected header".into());
    }
    Ok(
        json!({"result":"verified","height":trusted.height.to_string(),"stateRoot":hex(expected.snapshot.state_root.as_slice()),"queryDigest":hex(expected.query.digest().as_slice()),"rows":rows.len(),"postingCount":page.posting_count,"proofBytes":proof.len(),"hasNextPage":page.next.is_some(),"trust":"separately supplied unsigned simulator header"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_db::durable::Options;
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_node::simulator::{host::Host, light::Light, wire::pack, workload::Workload};
    use std::sync::Arc;

    #[test]
    fn separate_trust_inputs_reject_tampered_proof_query_and_displayed_rows() {
        let mut full = Host::create(
            Arc::new(MemoryVersionedStore::new()),
            RunConfig {
                genesis: arkiv_node::demo::workload::genesis(9001, 1700000000000),
                workload: Workload::default(),
            },
            [1; 16],
            [7; 16],
            Options::default(),
        )
        .unwrap();
        let mut light = Light::create(
            Arc::new(MemoryVersionedStore::new()),
            full.durable_identity.clone(),
        )
        .unwrap();
        for _ in 0..20 {
            full.produce(None).unwrap();
            light.append(&[full.head], &full.head).unwrap();
        }
        let selection:VerifiedRequest=serde_json::from_value(json!({"height":"20","namespace":"1","attribute":"group","valueType":"u64","value":"1","limit":2,"cursor":null})).unwrap();
        let (request, previous) = light.request(&selection).unwrap();
        let proof = full.proof(&request).unwrap();
        let response = light
            .inspect(&selection, &request, previous.as_ref(), &proof)
            .unwrap();
        let genesis = GenesisResponse {
            source_id: full.identity.source_id.clone(),
            run_id: full.identity.run_id.clone(),
            genesis_hash: full.identity.genesis_hash.clone(),
            chain_id: full.identity.chain_id.clone(),
            genesis: pack(&full.run.genesis),
            header: pack(&full.run.genesis.header()),
            durable_identity: hex(&full.durable_identity.envelope().unwrap()),
        };
        let header = HeaderDto::new(&full.head).unwrap();
        let verify_response =
            |response, selection| verify(response, genesis.clone(), header.clone(), selection);
        assert_eq!(
            verify_response(response.clone(), selection.clone()).unwrap()["postingCount"],
            5
        );
        let mut damaged = response.clone();
        let mut proof = proof;
        proof[200] ^= 1;
        damaged["canonicalProof"] = json!(hex(&proof));
        assert!(verify_response(damaged, selection.clone()).is_err());
        let mut damaged = response.clone();
        damaged["rows"] = json!([]);
        assert!(verify_response(damaged, selection.clone()).is_err());
        let mut damaged = response.clone();
        damaged["query"]["value"] = json!("2");
        assert!(verify_response(damaged, selection.clone()).is_err());
        let mut wrong_selection = selection.clone();
        wrong_selection.value = json!("2");
        assert!(verify_response(response.clone(), wrong_selection).is_err());
        let mut wrong_header = header;
        wrong_header.state_root = hex(&[0; 32]);
        assert!(verify(response, genesis, wrong_header, selection).is_err());
    }
}
