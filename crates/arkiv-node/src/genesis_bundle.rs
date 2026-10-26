//! Bounded, deterministic height-zero entity builder, protocol 4.
//! Canonical bytes, not authoring syntax or local paths, define launch identity.
use crate::{accounts, arkiv, signed::protocol::SignedGenesis};
use alloy_primitives::U256;
use arkiv_db::{
    B256, Budget, Cell, CellName, Decode, DecodeError, Decoder, Encode, Encoder, Engine,
    NamespaceName, OwnerId, Projection, RecordKey, SealedCandidate, StateHeader, Value, WriteOp,
    decode_exact, digest, encode,
};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_ENTITIES: u64 = 256;
pub const MAX_CANONICAL_BYTES: u64 = 1024 * 1024;
pub const MAX_STORAGE_UNITS: u64 = 8 * 1024 * 1024;
pub const STREAM_DOMAIN: &str = "arkiv/prefill-stream/v1";
pub const RECIPE_DOMAIN: &str = "arkiv/genesis-recipe/v1";
/// Row codec of both prefill stream framings (the `u32` after the magic):
/// user-defined columns, each attribute an engine [`Cell`] whose kind is its
/// indexed flag, with no payload or content type. Codecs 1 (`ARKPFL01`) and
/// 2 (`ARKPFL02`) carried a payload and content type and are refused.
pub const ROW_CODEC: u32 = 3;
/// The most user attributes of one prefill row: a create's
/// [`arkiv::MAX_ATTRIBUTES`].
pub const MAX_ROW_ATTRIBUTES: u32 = arkiv::MAX_ATTRIBUTES as u32;
/// The prefill row expiry that never expires, the same `u64::MAX` sentinel the
/// executor stores for a permanent `$expiresAt` ([`crate::Expiry::PERMANENT`]).
/// Only a permanent-prefill launch ([`PrefillLaunch::permanent_prefill`]) admits it.
pub const PERMANENT_EXPIRY: u64 = u64::MAX;
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

/// One height-zero entity of a prefill stream, row codec [`ROW_CODEC`]:
/// `creator[20] ‖ create_nonce u64 ‖ salt u128 ‖ expires_at_height u64 ‖
/// flags u8 ‖ u32 count ‖ count × (CellName ‖ Cell)`, all in the engine codec.
/// A `Cell` is `kind ‖ Value`: kind `01` is an indexed attribute
/// ([`Cell::attribute`]), `02` an unindexed one ([`Cell::field`]); `bytes`
/// values must be unindexed. It is created exactly like a live `Create` of
/// those attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefillEntity {
    pub creator: [u8; 20],
    pub create_nonce: u64,
    pub salt: u128,
    pub expires_at_height: u64,
    pub flags: u8,
    /// User attributes, strictly ascending by name.
    pub attributes: Vec<(CellName, Cell)>,
}
impl Encode for PrefillEntity {
    fn encode_to(&self, e: &mut Encoder) {
        e.fixed(&self.creator);
        e.u64(self.create_nonce);
        e.fixed(&self.salt.to_be_bytes());
        e.u64(self.expires_at_height);
        e.u8(self.flags);
        e.put(&self.attributes);
    }
}
impl Decode for PrefillEntity {
    const MIN_ENCODED_LEN: usize = 57;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        Self::decode_with_attribute_limit(d, MAX_ROW_ATTRIBUTES)
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
        let count = d.u32()?;
        // A name is at least 5 bytes and a cell at least 3.
        if count > max_attributes || count as usize > d.remaining() / 8 {
            return Err(invalid("attribute count"));
        }
        let mut attributes = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let name = d.get()?;
            let cell: Cell = d.get()?;
            if cell.value.encoded_len() > arkiv::MAX_BYTES_LENGTH {
                return Err(invalid("attribute size"));
            }
            attributes.push((name, cell));
        }
        Ok(Self {
            creator,
            create_nonce,
            salt,
            expires_at_height,
            flags,
            attributes,
        })
    }
}
fn canonical_entities(entities: &[PrefillEntity], permanent: bool) -> Result<Vec<PrefillEntity>> {
    if entities.len() as u64 > MAX_ENTITIES {
        return Err("too many prefill entities".into());
    }
    // Admission precedes cloning/sorting or encoding; callers may construct a much
    // larger in-memory recipe than the supported canonical stream.
    let mut bytes = 20usize;
    for row in entities {
        if row.attributes.len() > MAX_ROW_ATTRIBUTES as usize {
            return Err("too many prefill attributes".into());
        }
        bytes = bytes
            .checked_add(4 + 20 + 8 + 16 + 8 + 1 + 4)
            .ok_or("prefill byte overflow")?;
        for (name, cell) in &row.attributes {
            bytes = bytes
                .checked_add(
                    6 + name.len()
                        + if matches!(cell.value, Value::Str(_) | Value::Bytes(_)) {
                            4
                        } else {
                            0
                        },
                )
                .and_then(|n| n.checked_add(cell.value.encoded_len()))
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
        if row.attributes.len() > MAX_ROW_ATTRIBUTES as usize
            || row.attributes.windows(2).any(|w| w[0].0 == w[1].0)
            || row
                .attributes
                .iter()
                .any(|(n, _)| n.as_str().starts_with('$'))
        {
            return Err("invalid, duplicate, system or excessive prefill attributes".into());
        }
        if row
            .attributes
            .iter()
            .any(|(_, c)| c.value.encoded_len() > arkiv::MAX_BYTES_LENGTH)
        {
            return Err("prefill value exceeds 128 KiB".into());
        }
        if row
            .attributes
            .iter()
            .any(|(_, c)| c.is_attribute() && matches!(c.value, Value::Bytes(_)))
        {
            return Err("prefill bytes attributes must be unindexed".into());
        }
        if arkiv::entity_bytes(&row.attributes) > arkiv::MAX_ENTITY_BYTES {
            return Err("prefill entity bytes exceed 128 KiB".into());
        }
        if row.flags & !3 != 0
            || row.expires_at_height == 0
            || (row.expires_at_height == PERMANENT_EXPIRY && !permanent)
        {
            return Err("prefill requires valid flags and finite positive expiry".into());
        }
    }
    Ok(rows)
}
pub fn encode_stream(entities: &[PrefillEntity]) -> Result<Vec<u8>> {
    encode_stream_with(entities, false)
}
/// [`encode_stream`] under a launch policy: `permanent` also admits
/// [`PERMANENT_EXPIRY`] rows. The byte format is the same.
pub fn encode_stream_with(entities: &[PrefillEntity], permanent: bool) -> Result<Vec<u8>> {
    let rows = canonical_entities(entities, permanent)?;
    let mut e = Encoder::new();
    e.fixed(b"ARKPFL01");
    e.u32(ROW_CODEC);
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
    decode_stream_with(bytes, false)
}
/// [`decode_stream`] under a launch policy; see [`encode_stream_with`].
pub fn decode_stream_with(bytes: &[u8], permanent: bool) -> Result<Vec<PrefillEntity>> {
    if bytes.len() as u64 > MAX_CANONICAL_BYTES {
        return Err("prefill stream exceeds 1 MiB".into());
    }
    let parse = || -> std::result::Result<Vec<PrefillEntity>, DecodeError> {
        let mut d = Decoder::new(bytes);
        if d.array::<8>()? != *b"ARKPFL01" || d.u32()? != ROW_CODEC {
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
    if encode_stream_with(&rows, permanent)? != bytes {
        return Err("noncanonical entity stream".into());
    }
    Ok(rows)
}

/// Wire field order: version, limits, sponsor, stream hash/count/bytes, recipe hash,
/// initial StateHeader, initial burn as u256, logical storage units. Version 1 fixes
/// namespace=entities/ordinal1, builder=1 and sponsored finite rent policy=1.
///
/// | version | stream                         | rows admitted                 |
/// |---------|--------------------------------|-------------------------------|
/// | 1       | small `ARKPFL01` (256 rows)    | finite expiry only            |
/// | 2       | file-backed `ARKPFL02`         | finite expiry only            |
/// | 3       | small `ARKPFL01` (256 rows)    | finite or [`PERMANENT_EXPIRY`] |
/// | 4       | file-backed `ARKPFL02`         | finite or [`PERMANENT_EXPIRY`] |
///
/// Versions 3 and 4 are the permanent-prefill contract: finite rows are built,
/// scheduled and charged exactly as in versions 1 and 2; a permanent row is
/// created with `$expiresAt = u64::MAX`, never enters the expiry schedule (so
/// the per-height expiry capacity does not apply to it) and prepays no rent,
/// because rent prices a finite lifetime. Its storage units still count toward
/// `storage_units` and the declared limit. The layout is that of the
/// corresponding finite version; only the version and recipe domain differ, so
/// the chain identity commits to the policy. Live execution is unchanged.
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
/// `(streaming, permanent)` for a supported launch version.
fn launch_layout(version: u32) -> Option<(bool, bool)> {
    match version {
        1 => Some((false, false)),
        2 => Some((true, false)),
        3 => Some((false, true)),
        4 => Some((true, true)),
        _ => None,
    }
}
fn validate_launch_limits(streaming: bool, limits: PrefillLimits) -> Result<()> {
    if streaming {
        crate::genesis_streaming::validate_limits(limits)
    } else {
        limits.validate()
    }
}
impl PrefillLaunch {
    /// The launch version for a stream layout and permanence policy.
    pub const fn version_for(streaming: bool, permanent: bool) -> u32 {
        match (streaming, permanent) {
            (false, false) => 1,
            (true, false) => 2,
            (false, true) => 3,
            (true, true) => 4,
        }
    }
    /// A file-backed `ARKPFL02` launch (versions 2 and 4).
    pub fn is_streaming(&self) -> bool {
        launch_layout(self.version).is_some_and(|(streaming, _)| streaming)
    }
    /// The permanent-prefill contract (versions 3 and 4): rows may carry
    /// [`PERMANENT_EXPIRY`].
    pub fn permanent_prefill(&self) -> bool {
        launch_layout(self.version).is_some_and(|(_, permanent)| permanent)
    }
    pub fn validate(&self, g: &SignedGenesis) -> Result<()> {
        let Some((streaming, _)) = launch_layout(self.version) else {
            return Err("unsupported prefill version".into());
        };
        validate_launch_limits(streaming, self.limits)?;
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
        let Some((streaming, _)) = launch_layout(version) else {
            return Err(invalid("launch version"));
        };
        let limits = PrefillLimits {
            max_entities: d.u64()?,
            max_canonical_bytes: d.u64()?,
            max_storage_units: d.u64()?,
        };
        validate_launch_limits(streaming, limits).map_err(|_| invalid("limits"))?;
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
        match p.version {
            1 => RECIPE_DOMAIN,
            2 => "arkiv/genesis-recipe/v2",
            3 => "arkiv/genesis-recipe/v3",
            4 => "arkiv/genesis-recipe/v4",
            // Never valid: decoding and validation reject other versions first.
            _ => "arkiv/genesis-recipe/unsupported",
        },
        &e.into_bytes(),
    )
}
/// Genesis rent of one prefill row of `units` storage units expiring at
/// `expires_at`, created at height 0 under the chain's rent clock, in wei. A
/// finite row prepays its lifetime with [`crate::gas::rent`], capped like a
/// live create, so a row at or beyond the cap pays the cap. A
/// [`PERMANENT_EXPIRY`] row is admitted only under a permanent-prefill launch,
/// which sponsors it without rent; that is decided before any quote.
pub(crate) fn prefill_rent_wei(
    units: u64,
    expires_at: u64,
    permanent: bool,
    clock: crate::gas::RentClock,
) -> Result<U256> {
    if expires_at == PERMANENT_EXPIRY {
        return if permanent {
            Ok(U256::ZERO)
        } else {
            Err("permanent prefill rows require a permanent-prefill launch".into())
        };
    }
    crate::gas::rent(0, units, None, Some(expires_at), 0, clock)
        .map(|quote| U256::from(quote.wei))
        .map_err(err)
}
/// The rent clock of a genesis; its parameters were validated.
pub(crate) fn rent_clock(g: &SignedGenesis) -> Result<crate::gas::RentClock> {
    crate::gas::RentClock::of(&g.genesis.params).ok_or_else(|| "invalid rent clock".into())
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
    permanent: bool,
) -> Result<Constructed> {
    limits.validate()?;
    let rows = decode_stream_with(stream, permanent)?;
    if rows.len() as u64 > limits.max_entities || stream.len() as u64 > limits.max_canonical_bytes {
        return Err("declared prefill limit exceeded".into());
    }
    let accounts = &g.accounts;
    accounts.validate().map_err(err)?;
    let clock = rent_clock(g)?;
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
        let op = arkiv::ArkivOperation::Create {
            salt: row.salt,
            flags: row.flags,
            attributes: row.attributes,
            expiry: crate::Expiry {
                expires_at: row.expires_at_height,
                min_lifetime: 0,
            },
        };
        let context = arkiv::ArkivExecutionContext {
            caller: row.creator,
            block_height: 0,
            chain_id: g.genesis.chain_id,
            rent: clock,
            expiry_per_block: g.genesis.params.expiry_per_block,
            // The sponsor, not the creator, pays the prefill burn below.
            payer: None,
            allow_permanent_create: permanent,
        };
        if !matches!(
            arkiv::execute_operation(
                &mut branch,
                arkiv::entity_namespace(),
                &context,
                &op,
                Budget::unlimited()
            )
            .map_err(err)?
            .outcome,
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
        let rent = prefill_rent_wei(row_units, row.expires_at_height, permanent, clock)?;
        burn = burn.checked_add(rent).ok_or("prefill rent overflow")?;
        use crate::simulator::wire::hex;
        reports.push(serde_json::json!({"creator":hex(&row.creator),"createNonce":row.create_nonce.to_string(),"key":hex(&key),"expiresAtHeight":row.expires_at_height.to_string(),"storageUnits":row_units.to_string(),"rentWei":rent.to_string()}));
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
    base: SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    entities: &[PrefillEntity],
) -> Result<SignedGenesis> {
    build_genesis_with(base, limits, sponsor, entities, false)
}
/// [`build_genesis`] under a launch policy: `permanent` builds a version-3
/// permanent-prefill launch, which also admits [`PERMANENT_EXPIRY`] rows.
pub fn build_genesis_with(
    mut base: SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    entities: &[PrefillEntity],
    permanent: bool,
) -> Result<SignedGenesis> {
    base.prefill = None;
    if base.genesis.pricing != crate::gas::schedule() {
        return Err("prefill requires pinned GLM schedule".into());
    }
    let stream = encode_stream_with(entities, permanent)?;
    let built = construct(&base, limits, sponsor, &stream, permanent)?;
    let mut p = PrefillLaunch {
        version: PrefillLaunch::version_for(false, permanent),
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
    if p.is_streaming() {
        return Err("streaming prefill requires file-backed verification".into());
    }
    if stream.len() as u64 != p.canonical_bytes || digest(STREAM_DOMAIN, stream) != p.stream_hash {
        return Err("prefill stream hash/length mismatch".into());
    }
    let rows = decode_stream_with(stream, p.permanent_prefill())?;
    if rows.len() as u64 != p.row_count {
        return Err("prefill row count mismatch".into());
    }
    let built = construct(g, p.limits, p.sponsor, stream, p.permanent_prefill())?;
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
    let built = match construct(g, p.limits, p.sponsor, stream, p.permanent_prefill()) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error":e}),
    };
    let a = &g.accounts;
    let mut report = serde_json::json!({"genesisHash":hex(g.hash().as_slice()),"stateRoot":hex(p.state.root().as_slice()),"recipeHash":hex(p.recipe_hash.as_slice()),"streamHash":hex(p.stream_hash.as_slice()),"rowCount":p.row_count.to_string(),"canonicalBytes":p.canonical_bytes.to_string(),"storageUnits":p.storage_units.to_string(),"burnedWei":p.burned_wei.to_string(),"sponsor":hex(&p.sponsor),"sponsorBalanceWei":(a.initial_balance(OwnerId::new(p.sponsor))-p.burned_wei).to_string(),"initialSupplyWei":a.initial_supply().expect("valid supply").to_string(),"entities":built.rows});
    // Present only for the permanent-prefill contract, so finite reports and
    // the locks that record them are unchanged.
    if p.permanent_prefill() {
        report["permanentPrefill"] = serde_json::json!(true);
        report["prefillVersion"] = serde_json::json!(p.version);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permanent_rows_pay_no_time_rent_only_under_the_permanent_policy() {
        let clock =
            crate::gas::RentClock::new(1_000, crate::gas::DEFAULT_RENT_CAP_SECONDS).unwrap();
        let cap = clock.cap_blocks();
        for units in [1, 512, 9_000_000, u64::MAX] {
            // Waived before quoting, whatever the size.
            assert_eq!(
                prefill_rent_wei(units, PERMANENT_EXPIRY, true, clock).unwrap(),
                U256::ZERO
            );
            assert!(prefill_rent_wei(units, PERMANENT_EXPIRY, false, clock).is_err());
            for expiry in [1, 6_307_200, 1_000_000_000_000] {
                let finite = crate::gas::rent(0, units, None, Some(expiry), 0, clock)
                    .unwrap()
                    .wei;
                // A finite row is charged identically under either policy.
                for permanent in [false, true] {
                    assert_eq!(
                        prefill_rent_wei(units, expiry, permanent, clock).unwrap(),
                        U256::from(finite)
                    );
                }
            }
            // Finite rows at or beyond the cap all pay the cap: a year.
            assert_eq!(
                prefill_rent_wei(units, 1_000_000_000_000, false, clock).unwrap(),
                prefill_rent_wei(units, cap, false, clock).unwrap()
            );
        }
        assert_eq!(
            prefill_rent_wei(1, cap, false, clock).unwrap(),
            U256::from(7_300_000_000_000u64)
        );
        assert!(prefill_rent_wei(1, 0, true, clock).is_err());
    }
    /// The converter contract of `docs/genesis-bundle-v1.md`, byte for byte.
    #[test]
    fn row_codec_layout_is_pinned() {
        let row = PrefillEntity {
            creator: [0x22; 20],
            create_nonce: 0,
            salt: 2,
            expires_at_height: 100,
            flags: 2,
            attributes: vec![
                (
                    CellName::new("arkiv.schema").unwrap(),
                    Cell::attribute(Value::str("posts")),
                ),
                (
                    CellName::new("body").unwrap(),
                    Cell::field(Value::bytes(&[1, 2])),
                ),
                (
                    CellName::new("score").unwrap(),
                    Cell::attribute(Value::I32(-1)),
                ),
            ],
        };
        let expected = [
            "2222222222222222222222222222222222222222",
            "0000000000000000",
            "00000000000000000000000000000002",
            "0000000000000064",
            "02",
            "00000003",
            "0000000c61726b69762e736368656d61",
            "0102",
            "00000005706f737473",
            "00000004626f6479",
            "0203",
            "000000020102",
            "0000000573636f7265",
            "0110",
            "7fffffff",
        ]
        .concat();
        assert_eq!(alloy_primitives::hex::encode(encode(&row)), expected);
        let stream = encode_stream(std::slice::from_ref(&row)).unwrap();
        assert_eq!(&stream[..12], b"ARKPFL01\0\0\0\x03");
        assert_eq!(decode_stream(&stream).unwrap(), vec![row]);
    }
    #[test]
    fn launch_versions_select_layout_and_permanence() {
        for (version, streaming, permanent) in [
            (1, false, false),
            (2, true, false),
            (3, false, true),
            (4, true, true),
        ] {
            assert_eq!(PrefillLaunch::version_for(streaming, permanent), version);
            assert_eq!(launch_layout(version), Some((streaming, permanent)));
        }
        assert_eq!(launch_layout(0), None);
        assert_eq!(launch_layout(5), None);
    }
}
