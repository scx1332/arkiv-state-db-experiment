//! Bounded, deterministic height-zero entity builder, protocol 4.
//! Canonical bytes, not authoring syntax or local paths, define launch identity.
use crate::{accounts, arkiv, signed::protocol::SignedGenesis};
use alloy_primitives::U256;
use arkiv_db::{
    B256, Budget, CellName, Decode, DecodeError, Decoder, Encode, Encoder, Engine, NamespaceName,
    OwnerId, Projection, RecordKey, SealedCandidate, StateHeader, Value, WriteOp, decode_exact,
    digest, encode,
};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_ENTITIES: u64 = 256;
pub const MAX_CANONICAL_BYTES: u64 = 1024 * 1024;
pub const MAX_STORAGE_UNITS: u64 = 8 * 1024 * 1024;
pub const STREAM_DOMAIN: &str = "arkiv/prefill-stream/v1";
pub const RECIPE_DOMAIN: &str = "arkiv/genesis-recipe/v1";
type Result<T> = std::result::Result<T, String>;
fn invalid(reason: &'static str) -> DecodeError {
    DecodeError::Invalid {
        kind: "PrefillV1",
        reason,
    }
}
fn err(e: impl std::fmt::Debug) -> String {
    format!("prefill: {e:?}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefillLimits {
    pub max_entities: u64,
    pub max_canonical_bytes: u64,
    pub max_storage_units: u64,
}
impl PrefillLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_entities > MAX_ENTITIES
            || self.max_canonical_bytes < 20
            || self.max_canonical_bytes > MAX_CANONICAL_BYTES
            || self.max_storage_units > MAX_STORAGE_UNITS
        {
            return Err("prefill limits exceed supported envelope: 256 entities, 1 MiB canonical bytes, 8 MiB storage units".into());
        }
        Ok(())
    }
}
impl Encode for PrefillLimits {
    fn encode_to(&self, e: &mut Encoder) {
        e.u64(self.max_entities);
        e.u64(self.max_canonical_bytes);
        e.u64(self.max_storage_units);
    }
}
impl Decode for PrefillLimits {
    const MIN_ENCODED_LEN: usize = 24;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let v = Self {
            max_entities: d.u64()?,
            max_canonical_bytes: d.u64()?,
            max_storage_units: d.u64()?,
        };
        v.validate().map_err(|_| invalid("limits"))?;
        Ok(v)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefillEntity {
    pub creator: [u8; 20],
    pub create_nonce: u64,
    pub salt: u128,
    pub expires_at_height: u64,
    pub flags: u8,
    pub content_type: String,
    pub payload: Vec<u8>,
    pub attributes: Vec<(CellName, Value)>,
}
impl Encode for PrefillEntity {
    fn encode_to(&self, e: &mut Encoder) {
        e.fixed(&self.creator);
        e.u64(self.create_nonce);
        e.fixed(&self.salt.to_be_bytes());
        e.u64(self.expires_at_height);
        e.u8(self.flags);
        e.put(&self.content_type);
        e.bytes(&self.payload);
        e.put(&self.attributes);
    }
}
impl Decode for PrefillEntity {
    const MIN_ENCODED_LEN: usize = 65;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        Self::decode_with_attribute_limit(d, 23)
    }
}
impl PrefillEntity {
    pub(crate) fn decode_with_attribute_limit(
        d: &mut Decoder<'_>,
        max_attributes: u32,
    ) -> std::result::Result<Self, DecodeError> {
        let creator = d.array()?;
        let create_nonce = d.u64()?;
        let salt = u128::from_be_bytes(d.array()?);
        let expires_at_height = d.u64()?;
        let flags = d.u8()?;
        let content_type = d.str()?;
        if content_type.len() > 128 * 1024 {
            return Err(invalid("content type size"));
        }
        let content_type = content_type.to_owned();
        let payload = d.bytes()?;
        if payload.len() > 128 * 1024 {
            return Err(invalid("payload size"));
        }
        let payload = payload.to_vec();
        let count = d.u32()?;
        if count > max_attributes || count as usize > d.remaining() / 7 {
            return Err(invalid("attribute count"));
        }
        let mut attributes = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let name = d.get()?;
            let value: Value = d.get()?;
            if value.encoded_len() > 128 * 1024 {
                return Err(invalid("attribute size"));
            }
            attributes.push((name, value));
        }
        Ok(Self {
            creator,
            create_nonce,
            salt,
            expires_at_height,
            flags,
            content_type,
            payload,
            attributes,
        })
    }
}
fn canonical_entities(entities: &[PrefillEntity]) -> Result<Vec<PrefillEntity>> {
    if entities.len() as u64 > MAX_ENTITIES {
        return Err("too many prefill entities".into());
    }
    // Admission precedes cloning/sorting or encoding; callers may construct a much
    // larger in-memory recipe than the supported canonical stream.
    let mut bytes = 20usize;
    for row in entities {
        if row.attributes.len() > 23 {
            return Err("too many prefill attributes".into());
        }
        bytes = bytes
            .checked_add(4 + 20 + 8 + 16 + 8 + 1 + 4 + 4 + 4)
            .and_then(|n| n.checked_add(row.payload.len()))
            .and_then(|n| n.checked_add(row.content_type.len()))
            .ok_or("prefill byte overflow")?;
        for (name, value) in &row.attributes {
            bytes = bytes
                .checked_add(
                    5 + name.len()
                        + if matches!(value, Value::Str(_) | Value::Bytes(_)) {
                            4
                        } else {
                            0
                        },
                )
                .and_then(|n| n.checked_add(value.encoded_len()))
                .ok_or("prefill byte overflow")?;
        }
        if bytes as u64 > MAX_CANONICAL_BYTES {
            return Err("prefill stream exceeds 1 MiB".into());
        }
    }
    let mut rows = entities.to_vec();
    rows.sort_by_key(|r| (r.creator, r.create_nonce));
    let mut nonces = BTreeMap::<[u8; 20], u64>::new();
    for row in &mut rows {
        let next = nonces.entry(row.creator).or_default();
        if row.create_nonce != *next {
            return Err("creator nonces must be consecutive from zero, with no duplicate".into());
        }
        *next = next.checked_add(1).ok_or("creation nonce overflow")?;
        row.attributes.sort_by(|a, b| a.0.cmp(&b.0));
        if row.attributes.len() > 23
            || row.attributes.windows(2).any(|w| w[0].0 == w[1].0)
            || row
                .attributes
                .iter()
                .any(|(n, _)| n.as_str().starts_with('$'))
        {
            return Err("invalid, duplicate, system or excessive prefill attributes".into());
        }
        if row.payload.len() > 128 * 1024
            || row.content_type.len() > 128 * 1024
            || row
                .attributes
                .iter()
                .any(|(_, v)| v.encoded_len() > 128 * 1024)
        {
            return Err("prefill value exceeds 128 KiB".into());
        }
        if row.flags & !3 != 0 || row.expires_at_height == 0 || row.expires_at_height == u64::MAX {
            return Err("prefill requires valid flags and finite positive expiry".into());
        }
    }
    Ok(rows)
}
pub fn encode_stream(entities: &[PrefillEntity]) -> Result<Vec<u8>> {
    let rows = canonical_entities(entities)?;
    let mut e = Encoder::new();
    e.fixed(b"ARKPFL01");
    e.u32(1);
    e.u64(rows.len() as u64);
    for row in rows {
        let bytes = encode(&row);
        e.bytes(&bytes);
        if e.as_bytes().len() as u64 > MAX_CANONICAL_BYTES {
            return Err("prefill stream exceeds 1 MiB".into());
        }
    }
    Ok(e.into_bytes())
}
pub fn decode_stream(bytes: &[u8]) -> Result<Vec<PrefillEntity>> {
    if bytes.len() as u64 > MAX_CANONICAL_BYTES {
        return Err("prefill stream exceeds 1 MiB".into());
    }
    let parse = || -> std::result::Result<Vec<PrefillEntity>, DecodeError> {
        let mut d = Decoder::new(bytes);
        if d.array::<8>()? != *b"ARKPFL01" || d.u32()? != 1 {
            return Err(invalid("stream version"));
        }
        let count = d.u64()?;
        if count > MAX_ENTITIES {
            return Err(invalid("row count"));
        }
        let mut rows = Vec::new();
        for _ in 0..count {
            rows.push(decode_exact(d.bytes()?)?);
        }
        if d.remaining() != 0 {
            return Err(invalid("trailing stream bytes"));
        }
        Ok(rows)
    };
    let rows = parse().map_err(err)?;
    if encode_stream(&rows)? != bytes {
        return Err("noncanonical entity stream".into());
    }
    Ok(rows)
}

/// Wire field order: version, limits, sponsor, stream hash/count/bytes, recipe hash,
/// initial StateHeader, initial burn as u256, logical storage units. Version 1 fixes
/// namespace=entities/ordinal1, builder=1 and sponsored finite rent policy=1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefillLaunch {
    pub version: u32,
    pub limits: PrefillLimits,
    pub sponsor: [u8; 20],
    pub stream_hash: B256,
    pub row_count: u64,
    pub canonical_bytes: u64,
    pub recipe_hash: B256,
    pub state: StateHeader,
    pub burned_wei: U256,
    pub storage_units: u64,
}
impl PrefillLaunch {
    pub fn validate(&self, g: &SignedGenesis) -> Result<()> {
        match self.version {
            1 => self.limits.validate()?,
            2 => crate::genesis_streaming::validate_limits(self.limits)?,
            _ => return Err("unsupported prefill version".into()),
        }
        if self.row_count > self.limits.max_entities
            || self.canonical_bytes > self.limits.max_canonical_bytes
            || self.canonical_bytes < 20
            || self.storage_units > self.limits.max_storage_units
            || self.state.format != arkiv_db::STATE_FORMAT
            || self.state.pricing != g.genesis.pricing
            || self.state.catalog_len != 1
            || self.state.next_namespace != 2
            || self.recipe_hash != recipe_hash(g, self)
            || self.burned_wei > g.accounts.initial_balance(OwnerId::new(self.sponsor))
        {
            return Err("invalid prefill launch commitment".into());
        }
        Ok(())
    }
}
impl Encode for PrefillLaunch {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(self.version);
        e.put(&self.limits);
        e.fixed(&self.sponsor);
        e.put(&self.stream_hash);
        e.u64(self.row_count);
        e.u64(self.canonical_bytes);
        e.put(&self.recipe_hash);
        e.put(&self.state);
        e.fixed(&self.burned_wei.to_be_bytes::<32>());
        e.u64(self.storage_units);
    }
}
impl Decode for PrefillLaunch {
    const MIN_ENCODED_LEN: usize =
        4 + 24 + 20 + 32 + 8 + 8 + 32 + StateHeader::MIN_ENCODED_LEN + 32 + 8;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let version = d.u32()?;
        if version != 1 && version != 2 {
            return Err(invalid("launch version"));
        }
        let limits = PrefillLimits {
            max_entities: d.u64()?,
            max_canonical_bytes: d.u64()?,
            max_storage_units: d.u64()?,
        };
        if version == 1 {
            limits.validate().map_err(|_| invalid("limits"))?;
        } else {
            crate::genesis_streaming::validate_limits(limits).map_err(|_| invalid("limits"))?;
        }
        Ok(Self {
            version,
            limits,
            sponsor: d.array()?,
            stream_hash: d.get()?,
            row_count: d.u64()?,
            canonical_bytes: d.u64()?,
            recipe_hash: d.get()?,
            state: d.get()?,
            burned_wei: U256::from_be_bytes(d.array::<32>()?),
            storage_units: d.u64()?,
        })
    }
}
pub fn recipe_hash(g: &SignedGenesis, p: &PrefillLaunch) -> B256 {
    let mut e = Encoder::new();
    e.u32(p.version);
    e.put(&g.genesis);
    e.fixed(&g.run_id);
    e.fixed(&g.proposer);
    e.u64(g.block_period_ms);
    e.put(&g.limits);
    e.put(&g.accounts);
    e.u32(p.version);
    e.put(&p.limits);
    e.fixed(&p.sponsor);
    e.put(&p.stream_hash);
    e.u64(p.row_count);
    e.u64(p.canonical_bytes);
    digest(
        if p.version == 1 {
            RECIPE_DOMAIN
        } else {
            "arkiv/genesis-recipe/v2"
        },
        &e.into_bytes(),
    )
}
struct Constructed {
    candidate: SealedCandidate,
    burn: U256,
    units: u64,
    rows: Vec<serde_json::Value>,
}
fn construct(
    g: &SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    stream: &[u8],
) -> Result<Constructed> {
    limits.validate()?;
    let rows = decode_stream(stream)?;
    if rows.len() as u64 > limits.max_entities || stream.len() as u64 > limits.max_canonical_bytes {
        return Err("declared prefill limit exceeded".into());
    }
    let accounts = &g.accounts;
    accounts.validate().map_err(err)?;
    let engine = Engine::with_limits(g.genesis.pricing.clone(), g.genesis.limits);
    let mut branch = engine.begin(engine.head(), None).map_err(err)?;
    branch
        .try_execute(
            OwnerId::new(g.proposer),
            WriteOp::CreateNamespace {
                name: NamespaceName::new("entities").map_err(err)?,
            },
            Budget::unlimited(),
        )
        .map_err(err)?
        .map_err(err)?;
    let mut keys = BTreeSet::new();
    let mut units = 0u64;
    let mut burn = U256::ZERO;
    let mut reports = Vec::new();
    for row in rows {
        let key =
            arkiv::derive_entity_key(g.genesis.chain_id, &row.creator, row.create_nonce, row.salt);
        if !keys.insert(key) {
            return Err("prefill derived entity key collision".into());
        }
        let mut attrs = row.attributes;
        attrs.push((
            CellName::new("$payload").map_err(err)?,
            Value::bytes(&row.payload),
        ));
        attrs.push((
            CellName::new("$contentType").map_err(err)?,
            Value::str(&row.content_type),
        ));
        let op = arkiv::ArkivOperation::Create {
            salt: row.salt,
            flags: row.flags,
            attributes: attrs,
            expiry: crate::Expiry {
                expires_at: row.expires_at_height,
                min_lifetime: 0,
            },
        };
        let context = arkiv::ArkivExecutionContext {
            caller: row.creator,
            block_height: 0,
            chain_id: g.genesis.chain_id,
            max_lifetime: g.genesis.params.max_lifetime,
            expiry_per_block: g.genesis.params.expiry_per_block,
        };
        if !matches!(
            arkiv::execute_operation(
                &mut branch,
                arkiv::entity_namespace(),
                &context,
                &op,
                Budget::unlimited()
            )
            .map_err(err)?,
            crate::OpOutcome::Applied { .. }
        ) {
            return Err("prefill entity rejected by normal entity/expiry validation".into());
        }
        let stored = branch
            .try_get_record(
                arkiv::entity_namespace(),
                &RecordKey::new(&key).ok_or("invalid key")?,
                &Projection::ALL,
                Budget::unlimited(),
            )
            .map_err(err)?
            .map_err(err)?
            .value
            .ok_or("created prefill row missing")?;
        let row_units = crate::gas::storage_units(&stored.cells).map_err(err)?.total;
        units = units
            .checked_add(row_units)
            .ok_or("prefill storage units overflow")?;
        if units > limits.max_storage_units {
            return Err("prefill storage units limit exceeded".into());
        }
        let rent = crate::gas::rent(0, row_units, None, Some(row.expires_at_height), 0)
            .map_err(err)?
            .gas;
        burn = burn
            .checked_add(accounts::fee(rent))
            .ok_or("prefill rent overflow")?;
        use crate::simulator::wire::hex;
        reports.push(serde_json::json!({"creator":hex(&row.creator),"createNonce":row.create_nonce.to_string(),"key":hex(&key),"expiresAtHeight":row.expires_at_height.to_string(),"storageUnits":row_units.to_string(),"rentGas":rent.to_string()}));
    }
    let balance = accounts
        .initial_balance(OwnerId::new(sponsor))
        .checked_sub(burn)
        .ok_or("prefill sponsor is underfunded")?;
    if burn != U256::ZERO {
        accounts::put(
            &mut branch,
            OwnerId::new(sponsor),
            accounts::Account { nonce: 0, balance },
        )
        .map_err(err)?;
        branch
            .try_host_put(accounts::BURN_KEY, &burn.to_be_bytes::<32>())
            .map_err(err)?
            .map_err(err)?;
    }
    Ok(Constructed {
        candidate: branch.seal(),
        burn,
        units,
        rows: reports,
    })
}
pub fn build_genesis(
    mut base: SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    entities: &[PrefillEntity],
) -> Result<SignedGenesis> {
    base.prefill = None;
    if base.genesis.pricing != crate::gas::schedule() {
        return Err("prefill requires pinned GLM schedule".into());
    }
    let stream = encode_stream(entities)?;
    let built = construct(&base, limits, sponsor, &stream)?;
    let mut p = PrefillLaunch {
        version: 1,
        limits,
        sponsor,
        stream_hash: digest(STREAM_DOMAIN, &stream),
        row_count: entities.len() as u64,
        canonical_bytes: stream.len() as u64,
        recipe_hash: B256::ZERO,
        state: built.candidate.state_header(),
        burned_wei: built.burn,
        storage_units: built.units,
    };
    p.recipe_hash = recipe_hash(&base, &p);
    base.prefill = Some(p);
    base.validate().map_err(err)?;
    Ok(base)
}
pub fn verified_candidate(g: &SignedGenesis, stream: &[u8]) -> Result<SealedCandidate> {
    g.validate().map_err(err)?;
    let p = g.prefill.as_ref().ok_or("genesis has no prefill profile")?;
    if p.version != 1 {
        return Err("streaming prefill requires file-backed verification".into());
    }
    if stream.len() as u64 != p.canonical_bytes || digest(STREAM_DOMAIN, stream) != p.stream_hash {
        return Err("prefill stream hash/length mismatch".into());
    }
    let rows = decode_stream(stream)?;
    if rows.len() as u64 != p.row_count {
        return Err("prefill row count mismatch".into());
    }
    let built = construct(g, p.limits, p.sponsor, stream)?;
    if built.candidate.state_header() != p.state
        || built.burn != p.burned_wei
        || built.units != p.storage_units
    {
        return Err("prefill state/rent commitment mismatch".into());
    }
    Ok(built.candidate)
}
pub fn verify_genesis(g: &SignedGenesis, stream: &[u8]) -> Result<()> {
    verified_candidate(g, stream).map(|_| ())
}
pub fn report(g: &SignedGenesis, stream: &[u8]) -> serde_json::Value {
    use crate::simulator::wire::hex;
    let Some(p) = &g.prefill else {
        return serde_json::json!({"error":"not a prefill genesis"});
    };
    let built = match construct(g, p.limits, p.sponsor, stream) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error":e}),
    };
    let a = &g.accounts;
    serde_json::json!({"genesisHash":hex(g.hash().as_slice()),"stateRoot":hex(p.state.root().as_slice()),"recipeHash":hex(p.recipe_hash.as_slice()),"streamHash":hex(p.stream_hash.as_slice()),"rowCount":p.row_count.to_string(),"canonicalBytes":p.canonical_bytes.to_string(),"storageUnits":p.storage_units.to_string(),"burnedWei":p.burned_wei.to_string(),"sponsor":hex(&p.sponsor),"sponsorBalanceWei":(a.initial_balance(OwnerId::new(p.sponsor))-p.burned_wei).to_string(),"initialSupplyWei":a.initial_supply().expect("valid supply").to_string(),"entities":built.rows})
}
