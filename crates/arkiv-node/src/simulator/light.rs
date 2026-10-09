//! Header-only durable catalog and local stateless Eq verification. No engine or bodies.
use super::{
    Error, Result,
    host::{RunConfig, identity},
    wire::*,
};
use arkiv_db::durable::simulator::SimulatorIdentity;
use arkiv_db::proof::{
    EqQuery, EqRequest, SnapshotRef, VerifiedContinuation, VerifiedEq, verify_eq,
    verify_eq_inspected,
};
use arkiv_db::{CellName, NamespaceId, Value, decode_exact, digest, encode};
use arkiv_db_storage::versioned::{CommitBatch, CommitOutcome, Head, Table, VersionedStore};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc};

const PROFILE: &[u8] = if cfg!(feature = "arkiv-v2") {
    b"ARKSIMLIGHT2"
} else {
    b"ARKSIMLIGHT1"
};
const PROFILE_KEY: &[u8] = b"sim-light/profile";
const ID_KEY: &[u8] = b"sim-light/identity";
const CONFLICT_KEY: &[u8] = b"sim-light/conflict";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifiedRequest {
    pub height: String,
    pub namespace: String,
    pub attribute: String,
    pub value_type: String,
    pub value: serde_json::Value,
    pub limit: u32,
    pub cursor: Option<String>,
}
pub fn resolve(
    identity: &Identity,
    header: &crate::Header,
    r: &VerifiedRequest,
) -> Result<EqRequest> {
    if number(&r.height)? != header.height || !(1..=64).contains(&r.limit) {
        return Err(Error("InvalidRequest"));
    }
    let namespace =
        NamespaceId::from_ordinal(number(&r.namespace)?).ok_or(Error("InvalidRequest"))?;
    let attribute = CellName::new(&r.attribute).map_err(|_| Error("InvalidRequest"))?;
    let value = match r.value_type.as_str() {
        "bool" => Value::Bool(r.value.as_bool().ok_or(Error("InvalidRequest"))?),
        "u64" => Value::U64(number(r.value.as_str().ok_or(Error("InvalidRequest"))?)?),
        "i64" if !cfg!(feature = "arkiv-v2") => {
            let text = r.value.as_str().ok_or(Error("InvalidRequest"))?;
            let n: i64 = text.parse().map_err(|_| Error("InvalidRequest"))?;
            if n.to_string() != text {
                return Err(Error("InvalidRequest"));
            }
            Value::I64(n)
        }
        "str" => {
            let s = r.value.as_str().ok_or(Error("InvalidRequest"))?;
            if s.len() > Value::MAX_INDEXED_LEN {
                return Err(Error("InvalidRequest"));
            }
            Value::str(s)
        }
        "i32" => {
            let text = r.value.as_str().ok_or(Error("InvalidRequest"))?;
            let n: i32 = text.parse().map_err(|_| Error("InvalidRequest"))?;
            if n.to_string() != text {
                return Err(Error("InvalidRequest"));
            }
            Value::I32(n)
        }
        "u256" => {
            let text = r.value.as_str().ok_or(Error("InvalidRequest"))?;
            let n: alloy_primitives::U256 = text.parse().map_err(|_| Error("InvalidRequest"))?;
            if n.to_string() != text {
                return Err(Error("InvalidRequest"));
            }
            Value::U256(n.to_be_bytes())
        }
        #[cfg(feature = "arkiv-v2")]
        "dec" if cfg!(feature = "arkiv-v2") => Value::Dec(
            crate::arkiv::parse_decimal(r.value.as_str().ok_or(Error("InvalidRequest"))?)
                .map_err(|_| Error("InvalidRequest"))?,
        ),
        "bytes32" | "key" => {
            let bytes: [u8; 32] = unhex(r.value.as_str().ok_or(Error("InvalidRequest"))?, 32)?
                .try_into()
                .map_err(|_| Error("InvalidRequest"))?;
            if r.value_type == "key" {
                Value::Key(bytes)
            } else {
                Value::Bytes32(bytes)
            }
        }
        "addr" => Value::Addr(
            unhex(r.value.as_str().ok_or(Error("InvalidRequest"))?, 20)?
                .try_into()
                .map_err(|_| Error("InvalidRequest"))?,
        ),
        // The golem-grid scalars (and dec in the generic profile) use the
        // shared JSON forms of value_json.
        "dec" | "bytes4" | "bytes8" | "bytes16" | "u32" | "u128" | "i128" | "i256" | "dec32"
        | "dec64" | "dec128" | "f32" | "f64" | "date32" | "timestamp64" => {
            crate::value_json::from_json(&r.value_type, &r.value).ok_or(Error("InvalidRequest"))?
        }
        _ => return Err(Error("UnsupportedProof")),
    };
    #[cfg(feature = "arkiv-v2")]
    if namespace.ordinal() == 1 {
        use arkiv_db::TypeTag;
        let valid = match attribute.as_str() {
            "$key" => value.tag() == TypeTag::Key,
            "$owner" | "$creator" => value.tag() == TypeTag::Addr,
            "$createdAt" | "$updatedAt" | "$expiresAt" => value.tag() == TypeTag::U64,
            "$contentType" => value.tag() == TypeTag::Str,
            name if name.starts_with('$') => false,
            name => crate::arkiv::sdk_safe_name(name),
        };
        if !valid || crate::arkiv::validate_value(&value).is_err() {
            return Err(Error("InvalidRequest"));
        }
    }
    Ok(EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: hash(&identity.genesis_hash)?,
            height: header.height,
            block_hash: header.hash(),
            state_root: header.state_root.ok_or(Error("UnsupportedVersion"))?,
        },
        query: EqQuery {
            namespace,
            attribute,
            value,
        },
        limit: r.limit,
        continuation: None,
    })
}
pub struct Light {
    store: Arc<dyn VersionedStore>,
    pub identity: Identity,
    pub run: RunConfig,
    pub durable_identity: SimulatorIdentity,
    pub head: crate::Header,
    pub paused: bool,
    pub health: &'static str,
    pub observed: u64,
    continuations: VecDeque<(String, EqRequest, VerifiedContinuation)>,
}
impl Light {
    pub fn create(store: Arc<dyn VersionedStore>, id: SimulatorIdentity) -> Result<Self> {
        let run: RunConfig = decode_exact(&id.metadata).map_err(|_| Error("InvalidRequest"))?;
        run.validate()?;
        let head = run.genesis.header();
        if head.hash() != id.node_genesis_hash {
            return Err(Error("IdentityMismatch"));
        }
        let mut batch = CommitBatch::new(
            None,
            Head {
                height: 0,
                hash: head.hash().0,
            },
        );
        batch
            .put_record(Table::Meta, PROFILE_KEY, PROFILE)
            .put_record(Table::Meta, ID_KEY, id.envelope()?)
            .put_record(Table::Blocks, 0u64.to_be_bytes(), encode(&head))
            .put_record(
                Table::BlockIndex,
                head.hash().as_slice(),
                0u64.to_be_bytes(),
            );
        match store.commit(batch) {
            CommitOutcome::Committed => {}
            CommitOutcome::NotCommitted(_) => return Err(Error("StorageUnavailable")),
            CommitOutcome::Indeterminate(_) => return Err(Error("CommitUncertain")),
        }
        Ok(Self {
            identity: identity(&id, &run.genesis),
            run,
            durable_identity: id,
            store,
            head,
            paused: false,
            health: "following",
            observed: 0,
            continuations: VecDeque::new(),
        })
    }
    pub fn open(store: Arc<dyn VersionedStore>, expected: &Identity) -> Result<Self> {
        if store
            .read_record_bounded(Table::Meta, PROFILE_KEY, 32)?
            .as_deref()
            != Some(PROFILE)
        {
            return Err(Error("IdentityMismatch"));
        }
        if store
            .read_record_bounded(Table::Meta, CONFLICT_KEY, 1024)?
            .is_some()
        {
            return Err(Error("ChainConflict"));
        }
        let bytes = store
            .read_record_bounded(Table::Meta, ID_KEY, 65536 + 256)?
            .ok_or(Error("StorageCorrupt"))?;
        let id = SimulatorIdentity::from_envelope(&bytes)?;
        let run: RunConfig = decode_exact(&id.metadata).map_err(|_| Error("StorageCorrupt"))?;
        run.validate()?;
        let identity = identity(&id, &run.genesis);
        if identity != *expected || id.node_genesis_hash != run.genesis.header().hash() {
            return Err(Error("IdentityMismatch"));
        }
        let h = store.head()?.ok_or(Error("StorageCorrupt"))?;
        let mut light = Self {
            head: run.genesis.header(),
            identity,
            run,
            durable_identity: id,
            store,
            paused: false,
            health: "following",
            observed: h.height,
            continuations: VecDeque::new(),
        };
        light.head = light.read_header(h.height)?;
        if light.head.hash().0 != h.hash {
            return Err(Error("StorageCorrupt"));
        }
        Ok(light)
    }
    fn read_header(&self, height: u64) -> Result<crate::Header> {
        let bytes = self
            .store
            .read_record_bounded(Table::Blocks, &height.to_be_bytes(), 156)?
            .ok_or(Error("StorageCorrupt"))?;
        let header: crate::Header = decode_exact(&bytes).map_err(|_| Error("StorageCorrupt"))?;
        if header.height != height
            || header.chain_id != self.run.genesis.chain_id
            || header.state_root.is_none()
            || self
                .store
                .read_record_bounded(Table::BlockIndex, header.hash().as_slice(), 8)?
                .as_deref()
                != Some(height.to_be_bytes().as_slice())
        {
            return Err(Error("StorageCorrupt"));
        }
        Ok(header)
    }
    pub fn header(&self, height: u64) -> Result<crate::Header> {
        if self.health == "chain-conflict" || self.health == "storage-fenced" {
            return Err(Error("ChainConflict"));
        }
        if height > self.head.height {
            return Err(Error("BlockNotSynced"));
        }
        self.read_header(height)
    }
    fn freeze(&mut self, header: &crate::Header) -> Result<()> {
        self.health = "chain-conflict";
        self.continuations.clear();
        let h = Head {
            height: self.head.height,
            hash: self.head.hash().0,
        };
        let mut batch = CommitBatch::new(
            Some(h),
            Head {
                height: h.height.checked_add(1).ok_or(Error("CapacityReached"))?,
                hash: h.hash,
            },
        );
        batch.put_record(Table::Meta, CONFLICT_KEY, encode(header));
        match self.store.commit(batch) {
            CommitOutcome::Committed => Err(Error("ChainConflict")),
            _ => {
                self.health = "storage-fenced";
                Err(Error("CommitUncertain"))
            }
        }
    }
    pub fn append(&mut self, headers: &[crate::Header], sampled: &crate::Header) -> Result<()> {
        if self.health == "chain-conflict" || self.health == "storage-fenced" {
            return Err(Error("ChainConflict"));
        }
        if headers.len() > HEADER_BATCH {
            return Err(Error("InvalidRequest"));
        }
        if sampled.chain_id != self.run.genesis.chain_id || sampled.state_root.is_none() {
            return Err(Error("ChainConflict"));
        }
        self.observed = self.observed.max(sampled.height);
        if sampled.height <= self.head.height
            && self.read_header(sampled.height)?.hash() != sampled.hash()
        {
            return self.freeze(sampled);
        }
        let mut current = self.head;
        let mut accepted = Vec::new();
        for h in headers {
            if h.height <= self.head.height {
                if self.read_header(h.height)? != *h {
                    return self.freeze(h);
                }
                continue;
            }
            if h.height
                != current
                    .height
                    .checked_add(1)
                    .ok_or(Error("CapacityReached"))?
            {
                return Err(Error("BlockNotSynced"));
            }
            crate::validate::check_successor(
                &self.run.validate()?,
                &current,
                h.height,
                h.timestamp_ms,
            )
            .map_err(|_| Error("ChainConflict"))?;
            if h.parent_hash != current.hash()
                || h.chain_id != self.run.genesis.chain_id
                || h.state_root.is_none()
                || h.timestamp_ms
                    != self
                        .run
                        .workload
                        .timestamp(self.run.genesis.timestamp_ms, h.height)?
            {
                return Err(Error("ChainConflict"));
            }
            current = *h;
            accepted.push(*h);
        }
        if current.height > sampled.height {
            if accepted.is_empty() {
                self.health = "stalled";
                return Ok(());
            }
            return Err(Error("BlockNotSynced"));
        }
        if current.height == sampled.height && current.hash() != sampled.hash() {
            return Err(Error("ChainConflict"));
        }
        if !accepted.is_empty() {
            let mut batch = CommitBatch::new(
                Some(Head {
                    height: self.head.height,
                    hash: self.head.hash().0,
                }),
                Head {
                    height: current.height,
                    hash: current.hash().0,
                },
            );
            for h in accepted {
                batch
                    .put_record(Table::Blocks, h.height.to_be_bytes(), encode(&h))
                    .put_record(
                        Table::BlockIndex,
                        h.hash().as_slice(),
                        h.height.to_be_bytes(),
                    );
            }
            match self.store.commit(batch) {
                CommitOutcome::Committed => self.head = current,
                CommitOutcome::NotCommitted(_) => {
                    self.health = "storage-fenced";
                    return Err(Error("StorageUnavailable"));
                }
                CommitOutcome::Indeterminate(_) => {
                    self.health = "storage-fenced";
                    return Err(Error("CommitUncertain"));
                }
            }
        }
        self.health = if self.head.height == sampled.height {
            "following"
        } else {
            "stalled"
        };
        Ok(())
    }
    pub fn request(
        &self,
        r: &VerifiedRequest,
    ) -> Result<(EqRequest, Option<VerifiedContinuation>)> {
        let mut request = resolve(&self.identity, &self.header(number(&r.height)?)?, r)?;
        let previous = if let Some(cursor) = &r.cursor {
            let (_, prior, token) = self
                .continuations
                .iter()
                .find(|(c, _, _)| c == cursor)
                .ok_or(Error("CursorMismatch"))?;
            if prior.snapshot != request.snapshot
                || prior.query != request.query
                || prior.limit != request.limit
            {
                return Err(Error("CursorMismatch"));
            }
            request.continuation = Some(token.cursor().clone());
            Some(token.clone())
        } else {
            None
        };
        Ok((request, previous))
    }
    pub fn verify(
        &mut self,
        r: &VerifiedRequest,
        request: &EqRequest,
        previous: Option<&VerifiedContinuation>,
        bytes: &[u8],
    ) -> Result<serde_json::Value> {
        self.verify_page(r, request, previous, bytes, false)
    }
    pub fn inspect(
        &mut self,
        r: &VerifiedRequest,
        request: &EqRequest,
        previous: Option<&VerifiedContinuation>,
        bytes: &[u8],
    ) -> Result<serde_json::Value> {
        self.verify_page(r, request, previous, bytes, true)
    }
    fn verify_page(
        &mut self,
        r: &VerifiedRequest,
        request: &EqRequest,
        previous: Option<&VerifiedContinuation>,
        bytes: &[u8],
        inspect: bool,
    ) -> Result<serde_json::Value> {
        // Re-check the local catalog after network I/O; conflicts cannot race a badge.
        let h = self.header(request.snapshot.height)?;
        if h.hash() != request.snapshot.block_hash {
            return Err(Error("ChainConflict"));
        }
        let (verified, inspection) = if inspect {
            let mut caps = proof_limits();
            caps.max_proof_bytes = super::inspection::INSPECT_PROOF_CAP;
            let (verified, inspection) =
                verify_eq_inspected(request, previous, bytes, caps, self.run.genesis.limits)?;
            (verified, Some(inspection))
        } else {
            (
                verify_eq(
                    request,
                    previous,
                    bytes,
                    proof_limits(),
                    self.run.genesis.limits,
                )?,
                None,
            )
        };
        let page = match verified {
            VerifiedEq::NamespaceAbsent => return Err(Error("NamespaceNotFound")),
            VerifiedEq::Page(p) => p,
        };
        #[cfg(feature = "arkiv-v2")]
        if request.query.namespace.ordinal() == 1 {
            for row in &page.rows {
                crate::arkiv::validate_proven_row(row).map_err(|_| Error("InvalidProof"))?;
            }
        }
        #[cfg(feature = "arkiv-v2")]
        let entities = if request.query.namespace.ordinal() == 1 {
            page.rows.iter().map(|row| {
                let p=crate::arkiv::project(row).map_err(|_| Error("InvalidProof"))?;
                if p.created_at > h.height || p.updated_at > h.height || (p.expires_at != u64::MAX && p.expires_at <= h.height) { return Err(Error("InvalidProof")); }
                Ok(serde_json::json!({"key":hex(&p.key),"creator":hex(&p.creator),"owner":hex(&p.owner),
                    "createdAt":p.created_at.to_string(),"updatedAt":p.updated_at.to_string(),"expiresAt":p.expires_at.to_string(),
                    "creationFlags":{"raw":p.creation_flags,"readonly":p.creation_flags & 1 != 0,"permissionlessExtension":p.creation_flags & 2 != 0},
                    "contentType":p.content_type,"payload":hex(&p.payload)}))
            }).collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };
        let rows = page
            .rows
            .into_iter()
            .map(|row| super::metadata::record_change(request.query.namespace, row))
            .collect::<Result<Vec<_>>>()?;
        let cursor = page.next.map(|token| {
            let mut bytes = self.identity.run_id.as_bytes().to_vec();
            bytes.extend_from_slice(&encode(token.cursor()));
            bytes.extend_from_slice(&request.limit.to_be_bytes());
            let cursor = hex(digest("arkiv/simulator-continuation/v1", &bytes).as_slice());
            if self.continuations.len() >= 64 {
                self.continuations.pop_front();
            }
            self.continuations
                .push_back((cursor.clone(), request.clone(), token));
            cursor
        });
        let mut response = serde_json::json!({"verification":"proof verified against trusted simulator root; unsigned source","sourceId":self.identity.source_id,"runId":self.identity.run_id,"genesisHash":self.identity.genesis_hash,"chainId":self.identity.chain_id,"snapshot":{"height":h.height.to_string(),"hash":hex(h.hash().as_slice()),"stateRoot":hex(h.state_root.unwrap().as_slice())},"query":{"namespace":r.namespace,"attribute":r.attribute,"valueType":r.value_type,"value":r.value,"limit":r.limit},"rows":rows,"continuation":cursor,"postingCount":page.posting_count});
        #[cfg(feature = "arkiv-v2")]
        {
            response["profile"] = serde_json::json!("arkiv-entity-v2");
            response["entities"] = serde_json::json!(entities);
        }
        if let Some(inspection) = inspection {
            let object = response.as_object_mut().ok_or(Error("InvalidProof"))?;
            object.insert("canonicalRequest".into(), serde_json::json!(pack(request)));
            object.insert("canonicalProof".into(), serde_json::json!(hex(bytes)));
            object.insert(
                "inspection".into(),
                super::inspection::json(request, &inspection),
            );
        }
        Ok(response)
    }
    pub fn continuation_count(&self) -> usize {
        self.continuations.len()
    }
}
