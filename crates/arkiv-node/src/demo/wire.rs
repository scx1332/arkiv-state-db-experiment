//! JSON is transport only. Canonical objects are bounded lowercase hex.
use super::{DemoError, Result};
use arkiv_db::{Decode, Encode, decode_exact, encode};
use serde::{Deserialize, Serialize};

pub const REQUEST_CAP: usize = 1024 * 1024;
pub const RESPONSE_CAP: usize = 8 * 1024 * 1024;
pub const HEADER_BATCH: u32 = 128;
pub const BLOCK_BATCH: u32 = 16;
pub const QUEUE_COUNT: usize = 1024;
pub const QUEUE_BYTES: usize = 8 * 1024 * 1024;
pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for byte in bytes {
        s.push(DIGITS[usize::from(byte >> 4)] as char);
        s.push(DIGITS[usize::from(byte & 15)] as char);
    }
    s
}
pub fn unhex(s: &str, cap: usize) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").ok_or(DemoError::InvalidRequest)?;
    if s.len() % 2 != 0 || s.len() / 2 > cap {
        return Err(DemoError::ProofLimitExceeded);
    }
    fn nibble(b: u8) -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(DemoError::InvalidRequest),
        }
    }
    s.as_bytes()
        .chunks_exact(2)
        .map(|p| Ok(nibble(p[0])? * 16 + nibble(p[1])?))
        .collect()
}
pub fn packed<T: Encode>(value: &T) -> String {
    hex(&encode(value))
}
pub fn unpack<T: Decode>(text: &str, cap: usize) -> Result<T> {
    decode_exact(&unhex(text, cap)?).map_err(|e| {
        if matches!(e, arkiv_db::DecodeError::UnsupportedVersion { .. }) {
            DemoError::UnsupportedVersion
        } else {
            DemoError::InvalidRequest
        }
    })
}
pub fn number(text: &str) -> Result<u64> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(DemoError::InvalidRequest);
    }
    text.parse().map_err(|_| DemoError::InvalidRequest)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RpcRequest {
    pub rpc_version: u32,
    pub request: Request,
}
impl RpcRequest {
    pub fn new(request: Request) -> Self {
        Self {
            rpc_version: 1,
            request,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "method",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Request {
    Status {},
    Genesis {},
    Headers {
        after_height: String,
        after_hash: String,
        limit: u32,
    },
    Blocks {
        from_height: String,
        limit: u32,
    },
    Submit {
        envelope: String,
    },
    RequestStatus {
        identity: String,
    },
    ProduceBlock {},
    QueryEq {
        request: String,
        proof: String,
    },
    /// User-facing query on the light process, always derived from verified evidence.
    QueryVerified {
        request: String,
        peer: Option<String>,
    },
    /// Local full-node query, evaluated through the ordinary engine read surface.
    QueryLocal {
        request: String,
    },
    /// Resolve once against this process's accepted head/height and construct EqQueryV1.
    ResolveQuery {
        height: String,
        namespace: String,
        attribute: String,
        value_type: String,
        value: String,
        limit: u32,
    },
    Pause {
        paused: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RpcResponse {
    pub rpc_version: u32,
    pub result: Option<serde_json::Value>,
    pub error: Option<DemoError>,
}
impl RpcResponse {
    pub fn from_result(result: Result<serde_json::Value>) -> Self {
        match result {
            Ok(value) => Self {
                rpc_version: 1,
                result: Some(value),
                error: None,
            },
            Err(error) => Self {
                rpc_version: 1,
                result: None,
                error: Some(error),
            },
        }
    }
    pub fn into_result(self) -> Result<serde_json::Value> {
        if self.rpc_version != 1 {
            return Err(DemoError::UnsupportedVersion);
        }
        match (self.result, self.error) {
            (Some(value), None) => Ok(value),
            (None, Some(error)) => Err(error),
            _ => Err(DemoError::InvalidRequest),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Status {
    pub role: String,
    pub genesis_hash: String,
    pub height: String,
    pub hash: String,
    pub root: String,
    pub health: String,
    pub observed_peer_height: String,
    pub lag: String,
    pub retained_from: String,
    pub retained_through: String,
    pub capacity: String,
    pub memory: String,
    pub trust: String,
    pub protocol_version: u32,
    pub authentication_version: u32,
    pub proof_profiles: Vec<String>,
    pub pending: String,
    pub conflict_evidence: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Headers {
    pub headers: Vec<String>,
    pub sampled_height: String,
    pub sampled_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SignedBlock {
    pub block: String,
    pub signed_header: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Blocks {
    pub blocks: Vec<SignedBlock>,
    pub sampled_height: String,
    pub sampled_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Proof {
    pub proof: String,
    pub receipt: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GenesisResponse {
    pub signed_header: String,
}

pub fn json<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(|_| DemoError::InvalidRequest)
}
pub fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| DemoError::InvalidRequest)
}

/// Stream, count and cap actual response bytes, including chunked HTTP.
/// One caller request cannot allocate from an advertised Content-Length.
pub async fn rpc(
    client: &reqwest::Client,
    peer: &str,
    request: Request,
) -> Result<serde_json::Value> {
    let request =
        serde_json::to_vec(&RpcRequest::new(request)).map_err(|_| DemoError::InvalidRequest)?;
    if request.len() > REQUEST_CAP {
        return Err(DemoError::ProofLimitExceeded);
    }
    let mut response = client
        .post(format!("{}/rpc", peer.trim_end_matches('/')))
        .header("content-type", "application/json")
        .body(request)
        .send()
        .await
        .map_err(|_| DemoError::Transport)?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > RESPONSE_CAP as u64)
    {
        return Err(DemoError::Transport);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| DemoError::Transport)? {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|n| n > RESPONSE_CAP)
        {
            return Err(DemoError::ProofLimitExceeded);
        }
        bytes.extend_from_slice(&chunk);
    }
    let decoded: RpcResponse =
        serde_json::from_slice(&bytes).map_err(|_| DemoError::InvalidRequest)?;
    decoded.into_result()
}
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(DEADLINE)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| DemoError::Transport)
}
