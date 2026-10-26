//! The node's advertised data and proof capabilities. Never infer an encoding
//! from individual values.
use arkiv_db::Limits;
use serde_json::{Value, json};

/// The proof profile of equality pages ([`arkiv_db::proof`]).
pub const PROOF_PROFILE: &str = "eq-page-v1";

/// The Arkiv SDK value types an entity attribute may carry, in SDK ABI
/// type-id order. Every type may be indexed except `bytes`, which is only
/// stored.
const ENTITY_VALUE_TYPES: [&str; 10] = [
    "bool", "i32", "u64", "u256", "dec", "bytes32", "bytes", "str", "addr", "key",
];

/// Capabilities under the chain's genesis `limits`: every engine value type,
/// the configured size limits and the Arkiv entity namespace rules.
pub fn capabilities(limits: &Limits) -> Value {
    let value_types: Vec<&str> = arkiv_db::TypeTag::ALL
        .iter()
        .map(|tag| tag.name())
        .collect();
    json!({
        "profile":"arkiv-entity-v3",
        "layoutVersion":arkiv_db::LAYOUT_VERSION,
        "codecVersion":arkiv_db::CODEC_VERSION,
        "valueTypes":value_types,
        "limits":{
            "cellsPerRecord":limits.max_cells,
            "cellNameBytes":limits.max_cell_name_len,
            "indexedValueBytes":limits.indexed_value_len(),
            "strBytes":limits.max_str_len,
            "bytesBytes":limits.max_bytes_len,
            "pageRows":limits.max_page,
        },
        "entityNamespace":{
            "namespace":crate::arkiv::entity_namespace().ordinal().to_string(),
            "valueTypes":ENTITY_VALUE_TYPES,
            "unindexedValueTypes":["bytes"],
            "indexedTypeIdFlag":crate::arkiv::INDEXED_FLAG,
            "maxAttributes":crate::arkiv::MAX_ATTRIBUTES,
            "entityBytes":crate::arkiv::MAX_ENTITY_BYTES,
            "systemEquality":["$key","$creator","$owner","$createdAt","$updatedAt","$expiresAt"],
            "systemProjection":["$key","$creator","$owner","$createdAt","$updatedAt","$expiresAt","$creationFlags"],
            "currentSdkQueryExceptions":["$createdAt","$updatedAt"],
            "expiryPolicy":"purge-before-publication",
            "equalExpiryExtension":"updates-updatedAt",
        },
        "signatures":false,
        "productionStateRoots":false,
        "sdkRpcCompatible":false,
        "proofTrust":"configured-unsigned-header-source"
    })
}
