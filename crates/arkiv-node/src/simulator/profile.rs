//! Explicit binary profile. Never infer an encoding from individual values.
use serde_json::{Value, json};

pub const PROOF_PROFILE: &str = if cfg!(feature = "arkiv-v2") {
    "eq-page-v2"
} else {
    "eq-page-v1"
};

/// Every engine scalar type name, in type-id order. Generic namespaces of
/// both profiles admit all of them; the Arkiv entity namespace admits only
/// the SDK subset listed as `scalarTypes` of the v2 profile.
fn generic_scalar_types() -> Vec<&'static str> {
    arkiv_db::TypeTag::ALL
        .iter()
        .map(|tag| tag.name())
        .collect()
}

pub fn capabilities() -> Value {
    if cfg!(feature = "arkiv-v2") {
        json!({
            "genericScalarTypes":generic_scalar_types(),
            "profile":"arkiv-entity-v2", "layoutVersion":arkiv_db::LAYOUT_VERSION,
            "codecVersion":arkiv_db::CODEC_VERSION,
            "attributeNameBytes":arkiv_db::CellName::MAX_LEN,
            "indexedStringBytes":arkiv_db::Value::MAX_INDEXED_LEN, "payloadBytes":131072,
            "scalarTypes":["bool","i32","u64","u256","dec","bytes32","str","addr","key"],
            "systemEquality":["$key","$creator","$owner","$createdAt","$updatedAt","$expiresAt","$contentType"],
            "systemProjection":["$key","$creator","$owner","$createdAt","$updatedAt","$expiresAt","$contentType","$payload","$creationFlags"],
            "currentSdkQueryExceptions":["$createdAt","$updatedAt","$contentType"],
            "queryLanguage":"single-typed-equality", "signatures":false,
            "productionStateRoots":false,"sdkRpcCompatible":false,
            "expiryPolicy":"purge-before-publication", "equalExpiryExtension":"updates-updatedAt",
            "proofTrust":"configured-unsigned-header-source"
        })
    } else {
        json!({"profile":"generic-v1","layoutVersion":arkiv_db::LAYOUT_VERSION,
            "codecVersion":arkiv_db::CODEC_VERSION,
            "attributeNameBytes":arkiv_db::CellName::MAX_LEN,
            "indexedStringBytes":arkiv_db::Value::MAX_INDEXED_LEN,
            "scalarTypes":generic_scalar_types(),
            "queryLanguage":"single-typed-equality","signatures":false,
            "sdkRpcCompatible":false,"productionStateRoots":false})
    }
}
