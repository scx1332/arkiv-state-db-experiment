//! Explicit binary profile. Never infer an encoding from individual values.
use serde_json::{Value, json};

pub const PROOF_PROFILE: &str = if cfg!(feature = "arkiv-v2") {
    "eq-page-v2"
} else {
    "eq-page-v1"
};

pub fn capabilities() -> Value {
    if cfg!(feature = "arkiv-v2") {
        json!({
            "profile":"arkiv-entity-v2", "layoutVersion":2, "codecVersion":2,
            "attributeNameBytes":32, "indexedStringBytes":128, "payloadBytes":131072,
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
        json!({"profile":"generic-v1","layoutVersion":1,"codecVersion":1,
            "attributeNameBytes":20,"indexedStringBytes":64,
            "scalarTypes":["bool","i64","u64","str"],
            "queryLanguage":"single-typed-equality","signatures":false,
            "sdkRpcCompatible":false,"productionStateRoots":false})
    }
}
