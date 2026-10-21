//! Small, unpaged range-proof transport. The text query language is unchanged.
use super::{Node, Shared, fetch, parse, snapshot};
use crate::{
    signed::{Error, Result},
    simulator::{
        light::{VerifiedRequest, resolve as resolve_eq},
        wire::{Identity, REQUEST_CAP, RESPONSE_CAP, hex, number, pack, unhex, unpack},
    },
};
use arkiv_db::proof::{RANGE_PROOF_PROFILE, RangeQuery, RangeRequest, VerifiedRange};
use axum::body::Body;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::{ops::Bound, sync::Arc};
use tokio::sync::OwnedSemaphorePermit;

pub(super) const PROFILE: &str = RANGE_PROOF_PROFILE;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RangeBound {
    value: Json,
    inclusive: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Query {
    height: String,
    namespace: String,
    attribute: String,
    value_type: String,
    lower: Option<RangeBound>,
    upper: Option<RangeBound>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalRequest {
    request: String,
}

fn resolve(identity: &Identity, header: &crate::Header, query: &Query) -> Result<RangeRequest> {
    let anchor = query
        .lower
        .as_ref()
        .or(query.upper.as_ref())
        .ok_or(Error("InvalidRequest"))?;
    let value_request = |bound: &RangeBound| VerifiedRequest {
        height: query.height.clone(),
        namespace: query.namespace.clone(),
        attribute: query.attribute.clone(),
        value_type: query.value_type.clone(),
        value: bound.value.clone(),
        limit: 1,
        cursor: None,
    };
    let selected = resolve_eq(identity, header, &value_request(anchor))?;
    if !selected.query.value.tag().is_ordered() {
        return Err(Error("UnsupportedProof"));
    }
    let bound = |value: &Option<RangeBound>| -> Result<Bound<arkiv_db::Value>> {
        match value {
            None => Ok(Bound::Unbounded),
            Some(b) => {
                let value = resolve_eq(identity, header, &value_request(b))?.query.value;
                Ok(if b.inclusive {
                    Bound::Included(value)
                } else {
                    Bound::Excluded(value)
                })
            }
        }
    };
    let request = RangeRequest {
        snapshot: selected.snapshot,
        query: RangeQuery {
            namespace: selected.query.namespace,
            attribute: selected.query.attribute,
            lower: bound(&query.lower)?,
            upper: bound(&query.upper)?,
        },
    };
    request.query.validate()?;
    Ok(request)
}

/// `query/range` on a full node, a detached read (see
/// [`super::super::reads`]): the proof is generated from the pinned state
/// after the node lock is released.
pub(super) async fn proof(rt: Shared, body: Body, admission: OwnedSemaphorePermit) -> Result<Json> {
    let raw: CanonicalRequest = parse(body, REQUEST_CAP).await?;
    let request: RangeRequest = unpack(&raw.request, 4096)?;
    let admitted = rt.reads.admit()?;
    drop(admission);
    admitted
        .run("query/range", move || {
            let state = {
                let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
                let Node::Full(host) = &*node else {
                    return Err(Error("UnsupportedNodeRole"));
                };
                host.proof_state(&request.snapshot, "InvalidRequest")?
            };
            Ok(json!({"proof":hex(&crate::signed::host::range_proof_bytes(&state, &request)?)}))
        })
        .await
}

/// The light-node range flows: resolve, verify and inspect.
pub(super) async fn handle(
    rt: Shared,
    path: &str,
    body: Body,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<Json> {
    let inspect = path == "query/range/inspect";
    let query: Query = parse(body, REQUEST_CAP).await?;
    let (request, header, identity) = {
        let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let header = node.certificate(number(&query.height)?)?.header;
        if (path == "query/range/verified" || inspect) && !matches!(&*node, Node::Light { .. }) {
            return Err(Error("UnsupportedNodeRole"));
        }
        (
            resolve(node.identity(), &header, &query)?,
            header,
            node.identity().clone(),
        )
    };
    if path == "query/range/resolve" {
        return Ok(json!({"request":pack(&request)}));
    }
    let response: Json = fetch(
        &rt.client,
        rt.peer.as_deref().ok_or(Error("InvalidRequest"))?,
        "/signed/v1/query/range",
        Some(json!({"request":pack(&request)})),
        None,
        RESPONSE_CAP,
    )
    .await?;
    let bytes = unhex(
        response["proof"].as_str().ok_or(Error("InvalidProof"))?,
        if inspect {
            crate::simulator::inspection::INSPECT_PROOF_CAP
        } else {
            (RESPONSE_CAP - 4096) / 2
        },
    )?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let node = rt.node.try_lock().map_err(|_| Error("ServerBusy"))?;
        let Node::Light { light, .. } = &*node else {
            return Err(Error("UnsupportedNodeRole"));
        };
        let VerifiedRange::Complete(verified) = light.verify_range(&request, &bytes)? else {
            return Err(Error("NamespaceNotFound"));
        };
        let inspection = if inspect {
            let mut caps = crate::simulator::wire::range_proof_limits()?;
            caps.max_proof_bytes = crate::simulator::inspection::INSPECT_PROOF_CAP;
            let (_,trace) = arkiv_db::proof::verify_range_inspected(
                &request, &bytes, caps, node.genesis().genesis.limits,
            )?;
            Some(trace)
        } else { None };
        let rows = verified
            .rows
            .into_iter()
            .map(|row| crate::simulator::metadata::record_change(request.query.namespace, row))
            .collect::<Result<Vec<_>>>()?;
        let mut result = json!({"verification":"proof verified against authenticated single-proposer header",
            "verificationStatus":{"status":"verified","profile":PROFILE},
            "identity":identity,"query":query,"snapshot":snapshot(&header),
            "certificateBytes":pack(&node.certificate(header.height)?),
            "rows":rows,"complete":true,"proofProfile":PROFILE,
            "termCount":verified.term_count,"postingCount":verified.posting_count});
        if let Some(inspection) = inspection {
            result["canonicalRequest"] = json!(pack(&request));
            result["canonicalProof"] = json!(hex(&bytes));
            result["inspection"] = crate::simulator::inspection::range_json(&request, &inspection);
        }
        Ok(result)
    })
    .await
    .map_err(|_| Error("StorageUnavailable"))?
}
