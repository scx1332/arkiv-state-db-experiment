//! Closed, payload-free explorer DTOs. Canonical bodies and proofs use separate endpoints.
use super::{Error, Result};
use arkiv_db::{B256, Decode, Encode, decode_exact, encode};
use serde::{Deserialize, Serialize};

pub const REQUEST_CAP: usize = 64 * 1024;
pub const FEED_CAP: usize = 4 * 1024 * 1024;
pub const RESPONSE_CAP: usize = 8 * 1024 * 1024;
pub const ENVELOPE_CAP: usize = RESPONSE_CAP - 1024;
pub const HEADER_BATCH: usize = 64;
pub const READ_CONCURRENCY: usize = 4;

pub fn hex(bytes: &[u8]) -> String {
    crate::demo::wire::hex(bytes)
}
pub fn unhex(s: &str, cap: usize) -> Result<Vec<u8>> {
    crate::demo::wire::unhex(s, cap).map_err(|_| Error("InvalidRequest"))
}
pub fn number(s: &str) -> Result<u64> {
    crate::demo::wire::number(s).map_err(|_| Error("InvalidRequest"))
}
pub fn pack(v: &impl Encode) -> String {
    hex(&encode(v))
}
pub fn unpack<T: Decode>(s: &str, cap: usize) -> Result<T> {
    decode_exact(&unhex(s, cap)?).map_err(|e| {
        Error(
            if matches!(e, arkiv_db::DecodeError::UnsupportedVersion { .. }) {
                "UnsupportedVersion"
            } else {
                "InvalidRequest"
            },
        )
    })
}
pub fn hash(s: &str) -> Result<B256> {
    let b = unhex(s, 32)?;
    if b.len() != 32 {
        return Err(Error("InvalidRequest"));
    }
    Ok(B256::from_slice(&b))
}
pub fn json_bytes(v: &impl Serialize, cap: usize) -> Result<Vec<u8>> {
    // Serialize into a bounded sink, stopping before a candidate can grow beyond policy.
    struct Sink {
        bytes: Vec<u8>,
        cap: usize,
    }
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if b.len() > self.cap.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("bounded JSON"));
            }
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut sink, v).map_err(|_| Error("CapacityReached"))?;
    Ok(sink.bytes)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub source_id: String,
    pub run_id: String,
    pub genesis_hash: String,
    pub chain_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeaderDto {
    pub height: String,
    pub hash: String,
    pub parent_hash: String,
    pub state_root: String,
    pub timestamp_ms: String,
    pub inputs_digest: String,
    pub outcomes_digest: String,
    pub header_bytes: String,
}
impl HeaderDto {
    pub fn new(h: &crate::Header) -> Result<Self> {
        Ok(Self {
            height: h.height.to_string(),
            hash: hex(h.hash().as_slice()),
            parent_hash: hex(h.parent_hash.as_slice()),
            state_root: hex(h.state_root.ok_or(Error("UnsupportedVersion"))?.as_slice()),
            timestamp_ms: h.timestamp_ms.to_string(),
            inputs_digest: hex(h.inputs_digest.as_slice()),
            outcomes_digest: hex(h.outcomes_digest.as_slice()),
            header_bytes: pack(h),
        })
    }
    pub fn validate(&self, chain_id: u64) -> Result<crate::Header> {
        let h: crate::Header = unpack(&self.header_bytes, 156)?;
        if h.chain_id != chain_id || Self::new(&h)? != *self {
            return Err(Error("ChainConflict"));
        }
        Ok(h)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptDto {
    pub model_version: u32,
    pub schedule_id: u32,
    pub spent_units: String,
}
impl From<&arkiv_db::Receipt> for ReceiptDto {
    fn from(r: &arkiv_db::Receipt) -> Self {
        Self {
            model_version: r.model_version,
            schedule_id: r.schedule_id,
            spent_units: r.spent.units().to_string(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Transaction {
    pub position: u32,
    pub digest: String,
    pub actor: String,
    pub request_id: String,
    pub budget_units: String,
    pub status: String,
    pub spent_units: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Operation {
    pub phase: String,
    pub group_position: u32,
    pub operation_position: u32,
    pub kind: String,
    pub namespace_id: Option<String>,
    pub record_id: Option<String>,
    pub record_key: Option<String>,
    pub outcome: String,
    pub receipt: Option<ReceiptDto>,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attribute {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
    pub value: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
    pub byte_length: String,
    pub digest: String,
    pub reference: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Change {
    NamespaceUpsert {
        namespace_id: String,
        name: String,
        owner: String,
        revision: String,
    },
    RecordUpsert {
        namespace_id: String,
        record_id: String,
        record_key: String,
        expires_at_height: String,
        attributes: Vec<Attribute>,
        fields: Vec<Field>,
    },
    RecordDelete {
        namespace_id: String,
        record_id: String,
        record_key: String,
    },
    RawUpsert {
        namespace_id: String,
        key: String,
        byte_length: String,
        digest: String,
    },
    RawDelete {
        namespace_id: String,
        key: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedBlock {
    pub feed_version: u32,
    pub feed_digest: String,
    pub source_id: String,
    pub run_id: String,
    pub genesis_hash: String,
    pub chain_id: String,
    pub header: HeaderDto,
    pub transactions: Vec<Transaction>,
    pub operations: Vec<Operation>,
    pub changes: Vec<Change>,
    pub spent_units: String,
}
impl FeedBlock {
    pub fn new(
        id: &Identity,
        header: &crate::Header,
        metadata: super::metadata::Metadata,
    ) -> Result<Self> {
        let mut block = Self {
            feed_version: 1,
            feed_digest: String::new(),
            source_id: id.source_id.clone(),
            run_id: id.run_id.clone(),
            genesis_hash: id.genesis_hash.clone(),
            chain_id: id.chain_id.clone(),
            header: HeaderDto::new(header)?,
            transactions: metadata.transactions,
            operations: metadata.operations,
            changes: metadata.changes,
            spent_units: metadata.spent.to_string(),
        };
        block.feed_digest = hex(block.digest()?.as_slice());
        Ok(block)
    }
    pub fn digest(&self) -> Result<B256> {
        let mut value = serde_json::to_value(self).map_err(|_| Error("InvalidRequest"))?;
        value
            .as_object_mut()
            .ok_or(Error("InvalidRequest"))?
            .remove("feedDigest");
        metadata_digest(&value)
    }
    pub fn validate_digest(&self) -> Result<()> {
        if hash(&self.feed_digest)? != self.digest()? {
            return Err(Error("InvalidRequest"));
        }
        Ok(())
    }
}

/// Whole-block metadata codec v1. JSON is transport only: explicit type tags,
/// BE lengths/numbers, UTF-8 key ordering, depth64 and8MiB output bounds.
pub fn metadata_bytes(value: &serde_json::Value) -> Result<Vec<u8>> {
    fn put(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
        if bytes.len() > (8 * 1024 * 1024usize).saturating_sub(out.len()) {
            return Err(Error("CapacityReached"));
        }
        out.extend_from_slice(bytes);
        Ok(())
    }
    fn string(out: &mut Vec<u8>, s: &str) -> Result<()> {
        put(out, &[4])?;
        let n = u32::try_from(s.len()).map_err(|_| Error("CapacityReached"))?;
        put(out, &n.to_be_bytes())?;
        put(out, s.as_bytes())
    }
    fn write(
        out: &mut Vec<u8>,
        v: &serde_json::Value,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<()> {
        use serde_json::Value::*;
        *nodes += 1;
        if depth > 64 || *nodes > 1_048_576 {
            return Err(Error("InvalidRequest"));
        }
        match v {
            Null => put(out, &[0]),
            Bool(false) => put(out, &[1]),
            Bool(true) => put(out, &[2]),
            Number(n) => {
                let n = u32::try_from(n.as_u64().ok_or(Error("InvalidRequest"))?)
                    .map_err(|_| Error("InvalidRequest"))?;
                put(out, &[3])?;
                put(out, &n.to_be_bytes())
            }
            String(s) => string(out, s),
            Array(items) => {
                put(out, &[5])?;
                put(
                    out,
                    &u32::try_from(items.len())
                        .map_err(|_| Error("CapacityReached"))?
                        .to_be_bytes(),
                )?;
                for item in items {
                    write(out, item, depth + 1, nodes)?;
                }
                Ok(())
            }
            Object(items) => {
                put(out, &[6])?;
                put(
                    out,
                    &u32::try_from(items.len())
                        .map_err(|_| Error("CapacityReached"))?
                        .to_be_bytes(),
                )?;
                let mut entries = items.iter().collect::<Vec<_>>();
                entries.sort_unstable_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
                for (key, value) in entries {
                    string(out, key)?;
                    write(out, value, depth + 1, nodes)?;
                }
                Ok(())
            }
        }
    }
    let mut out = Vec::new();
    write(&mut out, value, 0, &mut 0)?;
    Ok(out)
}
pub fn metadata_digest(value: &serde_json::Value) -> Result<B256> {
    Ok(arkiv_db::digest(
        "arkiv/simulator-feed-block/v1",
        &metadata_bytes(value)?,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProofRequest {
    pub request: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolveRequest {
    pub height: String,
    pub namespace: String,
    pub attribute: String,
    pub value_type: String,
    pub value: serde_json::Value,
    pub limit: u32,
}

pub fn proof_limits() -> arkiv_db::proof::ProofLimits {
    arkiv_db::proof::ProofLimits {
        max_proof_bytes: (RESPONSE_CAP - 4096) / 2,
        ..Default::default()
    }
}

/// Operator-local admission policy; never changes the authenticated query.
pub fn range_proof_limits() -> Result<arkiv_db::proof::RangeProofLimits> {
    range_limits_with(|key| std::env::var(key).ok())
}

fn range_limits_with(
    get: impl Fn(&str) -> Option<String>,
) -> Result<arkiv_db::proof::RangeProofLimits> {
    let mut caps = arkiv_db::proof::RangeProofLimits {
        max_proof_bytes: (RESPONSE_CAP - 4096) / 2,
        ..Default::default()
    };
    for (key, value) in [
        ("ARKIV_RANGE_MAX_ROWS", &mut caps.max_rows),
        ("ARKIV_RANGE_MAX_TERMS", &mut caps.max_terms),
        ("ARKIV_RANGE_MAX_POSTING_IDS", &mut caps.max_posting_ids),
        ("ARKIV_RANGE_MAX_NODES", &mut caps.max_nodes),
        (
            "ARKIV_RANGE_MAX_ALLOCATED_BYTES",
            &mut caps.max_allocated_bytes,
        ),
    ] {
        if let Some(raw) = get(key) {
            *value = raw
                .parse()
                .ok()
                .filter(|v| *v > 0)
                .ok_or(Error("InvalidRangeLimits"))?;
        }
    }
    if let Some(raw) = get("ARKIV_RANGE_MAX_VERIFICATION_WORK") {
        caps.max_verification_work = raw
            .parse()
            .ok()
            .filter(|v| *v > 0)
            .ok_or(Error("InvalidRangeLimits"))?;
    }
    Ok(caps)
}

#[cfg(test)]
mod range_policy_tests {
    use super::*;
    #[test]
    fn configurable_policy_rejects_invalid_values() {
        assert_eq!(range_limits_with(|_| None).unwrap().max_rows, 1_000);
        let caps = range_limits_with(|key| (key == "ARKIV_RANGE_MAX_ROWS").then(|| "10000".into()))
            .unwrap();
        assert_eq!(caps.max_rows, 10_000);
        assert_eq!(caps.max_proof_bytes, (RESPONSE_CAP - 4096) / 2);
        for raw in ["0", "-1", "oops", "184467440737095516160"] {
            assert!(range_limits_with(|_| Some(raw.into())).is_err());
        }
    }
}
