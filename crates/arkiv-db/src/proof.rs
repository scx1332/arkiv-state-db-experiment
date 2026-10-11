//! Bounded complete equality pages (`eq-page-v1`) and unpaged numeric ranges
//! ([`RANGE_PROOF_PROFILE`]).
//!
//! The verifier takes only caller-selected block identity, local limits,
//! prior verified continuation and untrusted bytes. It has no engine,
//! loader, clock or network. The caller authenticates the selected header
//! and supplies its exact state root. Completeness is relative to that
//! state's authenticated index and the execution invariant that the index
//! describes the live rows; this is not an execution or consensus proof.
//!
//! Every equality page carries the entire posting set. Read accounting is
//! version 4 for these proof APIs (model-3 write/read rules are unchanged).
//! See `docs/eq-proof-charges.md` for charges and conservative reservations.

use std::fmt;

mod range;
pub(crate) use range::generate_range;
pub use range::{
    InspectedRangeInterval, InspectedRangeLeaf, InspectedRangePosting, RANGE_PROOF_PROFILE,
    RangeInspection, RangeProofLimits, RangeQuery, RangeRequest, VerifiedRange, VerifiedRangeRows,
    verify_range, verify_range_inspected,
};

use arkiv_db_trie::{MemoryOnly, MerkleMap, Probe, TrieKey, TrieValue, walk};

use crate::{
    B256, Budget, Cell, CellKind, CellName, Decode, DecodeError, Decoder, Encode, Encoder, Limits,
    NamespaceId, OperationClass as Op, Outcome, Receipt, ReceiptKind, RecordId, RecordKey,
    Rejection, Row, StateHeader, TypeTag, Value, digest, encode,
    layout::{Catalog, Member, NamespaceState, Posting, PostingMap, StoredRecord, TermKey},
    meter::Meter,
    nodes::{Loader, Nodes},
};

/// Wire/profile version.
pub const PROOF_VERSION: u32 = crate::LAYOUT_VERSION;
/// Read-only counting rules used by this API. Model-3 writes are unchanged.
pub const PROOF_READ_MODEL: u32 = 4;

/// One supported positive equality expression. Ordering and full-row
/// projection are fixed by this profile's two terminal `01` tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EqQuery {
    pub namespace: NamespaceId,
    pub attribute: CellName,
    pub value: Value,
}

impl EqQuery {
    pub fn digest(&self) -> B256 {
        digest("arkiv/eq-query/v1", &encode(self))
    }

    fn validate(&self, limits: Limits) -> Result<(), ProofError> {
        if !self.value.tag().is_indexable() {
            return Err(ProofError::UnsupportedProof);
        }
        if matches!(self.value, Value::Str(_))
            && self.value.encoded_len() > limits.indexed_value_len()
        {
            return Err(ProofError::InvalidRequest);
        }
        Ok(())
    }
}

impl Encode for EqQuery {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(PROOF_VERSION);
        e.put(&self.namespace);
        e.put(&self.attribute);
        e.put(&self.value);
        e.u8(1);
        e.u8(1);
    }
}
impl Decode for EqQuery {
    const MIN_ENCODED_LEN: usize = 4 + 8 + 5 + 2 + 2;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        version(d, "EqQueryV1")?;
        let query = Self {
            namespace: d.get()?,
            attribute: d.get()?,
            value: bounded_value(d, Value::MAX_INDEXED_LEN)?,
        };
        if !query.value.tag().is_indexable() || d.u8()? != 1 || d.u8()? != 1 {
            return Err(invalid_codec("unsupported equality profile"));
        }
        Ok(query)
    }
}

/// Stable block identity. No process-local snapshot id appears on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRef {
    pub genesis_hash: B256,
    pub height: u64,
    pub block_hash: B256,
    pub state_root: B256,
}
impl Encode for SnapshotRef {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(PROOF_VERSION);
        e.put(&self.genesis_hash);
        e.u64(self.height);
        e.put(&self.block_hash);
        e.put(&self.state_root);
    }
}
impl Decode for SnapshotRef {
    const MIN_ENCODED_LEN: usize = 108;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        version(d, "SnapshotRefV1")?;
        Ok(Self {
            genesis_hash: d.get()?,
            height: d.u64()?,
            block_hash: d.get()?,
            state_root: d.get()?,
        })
    }
}

/// Untrusted wire cursor. Possessing one does not authorize skipping pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EqCursor {
    pub snapshot: SnapshotRef,
    pub query_digest: B256,
    pub last_id: RecordId,
}
impl Encode for EqCursor {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(PROOF_VERSION);
        e.put(&self.snapshot);
        e.put(&self.query_digest);
        e.put(&self.last_id);
    }
}
impl Decode for EqCursor {
    const MIN_ENCODED_LEN: usize = 152;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        version(d, "CursorV1")?;
        Ok(Self {
            snapshot: d.get()?,
            query_digest: d.get()?,
            last_id: d.get()?,
        })
    }
}

/// Request identity comes from the caller's accepted header and query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EqRequest {
    pub snapshot: SnapshotRef,
    pub query: EqQuery,
    pub limit: u32,
    pub continuation: Option<EqCursor>,
}
impl Encode for EqRequest {
    fn encode_to(&self, e: &mut Encoder) {
        e.put(&self.snapshot);
        e.put(&self.query);
        e.u32(self.limit);
        e.put(&self.continuation);
    }
}
impl Decode for EqRequest {
    const MIN_ENCODED_LEN: usize = SnapshotRef::MIN_ENCODED_LEN + EqQuery::MIN_ENCODED_LEN + 5;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            snapshot: d.get()?,
            query: d.get()?,
            limit: d.u32()?,
            continuation: d.get()?,
        })
    }
}

/// Local admission/resource policy. Fields can be tightened by a caller.
/// Work and allocation are conservative cumulative upper bounds, not
/// measurements of allocator residency or CPU time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProofLimits {
    pub max_posting_ids: usize,
    pub max_page_rows: usize,
    pub max_proof_bytes: usize,
    pub max_nodes: usize,
    pub max_verification_work: u64,
    pub max_allocated_bytes: usize,
}
impl Default for ProofLimits {
    fn default() -> Self {
        Self {
            max_posting_ids: 4096,
            max_page_rows: 64,
            max_proof_bytes: 8 * 1024 * 1024,
            max_nodes: 8192,
            max_verification_work: 128 * 1024 * 1024,
            max_allocated_bytes: 128 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProofError {
    UnsupportedVersion,
    UnsupportedProof,
    InvalidRequest,
    CursorMismatch,
    ProofLimitExceeded,
    BudgetExceeded,
    StorageUnavailable,
    InvalidProof,
}
impl fmt::Display for ProofError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProofError {}
impl From<DecodeError> for ProofError {
    fn from(error: DecodeError) -> Self {
        if matches!(error, DecodeError::UnsupportedVersion { .. }) {
            Self::UnsupportedVersion
        } else {
            Self::InvalidProof
        }
    }
}
impl From<Rejection> for ProofError {
    fn from(_: Rejection) -> Self {
        Self::BudgetExceeded
    }
}

/// Successful service output. Receipt is advisory local accounting.
#[derive(Clone, Debug)]
pub struct GeneratedProof {
    pub bytes: Vec<u8>,
    pub receipt: Receipt,
    pub posting_count: usize,
    pub supplied_nodes: usize,
}

/// Only a successful nonterminal verified page can construct this token.
/// It deliberately implements neither Decode nor a public constructor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedContinuation(EqCursor);
impl VerifiedContinuation {
    pub fn cursor(&self) -> &EqCursor {
        &self.0
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPage {
    pub rows: Vec<Row>,
    pub next: Option<VerifiedContinuation>,
    pub posting_count: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifiedEq {
    /// Authenticated catalog absence: surface as NamespaceNotFound.
    NamespaceAbsent,
    Page(VerifiedPage),
}

/// A verified point path in one specific engine map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedMapPath {
    pub map: crate::MapKind,
    pub record_id: Option<RecordId>,
    pub root: B256,
    pub key: Vec<u8>,
    pub supplied_nodes: usize,
    pub trace: arkiv_db_trie::VerifiedPath,
}

/// Posting completeness uses reconstruction from ALL ordered ids, not point
/// paths. The authenticated term leaf supplies the root and entry count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedPostingSet {
    pub authenticated_root: Option<B256>,
    pub reconstructed_root: B256,
    pub ids: Vec<RecordId>,
    pub selected_ids: Vec<RecordId>,
}

/// Details from the exact witness accepted by the complete Eq verifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EqInspection {
    pub state_header: StateHeader,
    pub namespace: Option<crate::NamespaceRoots>,
    pub point_paths: Vec<InspectedMapPath>,
    pub posting_set: Option<InspectedPostingSet>,
}

#[derive(Default)]
struct InspectionCollector {
    state_header: Option<StateHeader>,
    namespace: Option<crate::NamespaceRoots>,
    point_paths: Vec<InspectedMapPath>,
    posting_set: Option<InspectedPostingSet>,
}

fn invalid_codec(reason: &'static str) -> DecodeError {
    DecodeError::Invalid {
        kind: "eq-page-v1",
        reason,
    }
}
fn version(d: &mut Decoder<'_>, kind: &'static str) -> Result<(), DecodeError> {
    let version = d.u32()?;
    if version != PROOF_VERSION {
        return Err(DecodeError::UnsupportedVersion { kind, version });
    }
    Ok(())
}
fn bounded_value(d: &mut Decoder<'_>, max: usize) -> Result<Value, DecodeError> {
    let tag = d.u8()?;
    let tag = TypeTag::from_id(tag).ok_or(DecodeError::UnknownTag { kind: "Value", tag })?;
    // Check a variable length before copying any string/bytes.
    if tag.width().is_none() {
        let mut peek = d.clone();
        if usize::try_from(peek.u32()?).map_or(true, |len| len > max) {
            return Err(invalid_codec("value exceeds local limit"));
        }
    }
    crate::codec::decode_payload(tag, d)
}
fn validate_request(
    request: &EqRequest,
    caps: ProofLimits,
    limits: Limits,
) -> Result<(), ProofError> {
    request.query.validate(limits)?;
    if request.limit == 0 || request.limit as usize > caps.max_page_rows.min(limits.max_page) {
        return Err(ProofError::InvalidRequest);
    }
    if let Some(cursor) = &request.continuation
        && (cursor.snapshot != request.snapshot || cursor.query_digest != request.query.digest())
    {
        return Err(ProofError::CursorMismatch);
    }
    Ok(())
}
fn next_cursor(request: &EqRequest, last_id: RecordId) -> EqCursor {
    EqCursor {
        snapshot: request.snapshot.clone(),
        query_digest: request.query.digest(),
        last_id,
    }
}
fn page_slice<'a>(
    ids: &'a [RecordId],
    request: &EqRequest,
) -> Result<(&'a [RecordId], Option<EqCursor>), ProofError> {
    let start = match &request.continuation {
        None => 0,
        Some(cursor) => {
            ids.binary_search(&cursor.last_id)
                .map_err(|_| ProofError::CursorMismatch)?
                + 1
        }
    };
    if request.continuation.is_some() && start == ids.len() {
        return Err(ProofError::CursorMismatch);
    }
    let end = start.saturating_add(request.limit as usize).min(ids.len());
    let selected = &ids[start..end];
    let next = if end < ids.len() {
        Some(next_cursor(
            request,
            *selected.last().ok_or(ProofError::InvalidProof)?,
        ))
    } else {
        None
    };
    Ok((selected, next))
}

// Maximum canonical node encodings for fixed-size metadata maps. A branch
// with sixteen hash children is 532 bytes; 1024 also bounds their leaves.
const META_NODE_BYTES: usize = 1024;
fn row_node_bound(limits: Limits) -> Result<usize, ProofError> {
    limits
        .max_cells
        .min(64)
        .checked_mul(
            limits
                .max_payload_len()
                .max(32)
                .checked_add(CellName::MAX_LEN + 12)
                .ok_or(ProofError::ProofLimitExceeded)?,
        )
        .and_then(|n| n.checked_add(128))
        .map(|n| n.max(META_NODE_BYTES))
        .ok_or(ProofError::ProofLimitExceeded)
}

/// Resource meter used before decoding/reservation, walks and reconstruction.
struct VerifyMeter {
    caps: ProofLimits,
    work: u64,
    allocated: usize,
    nodes: usize,
}
impl VerifyMeter {
    fn new(caps: ProofLimits) -> Self {
        Self {
            caps,
            work: 0,
            allocated: 0,
            nodes: 0,
        }
    }
    fn work(&mut self, units: usize) -> Result<(), ProofError> {
        self.work = self
            .work
            .checked_add(units as u64)
            .ok_or(ProofError::BudgetExceeded)?;
        if self.work > self.caps.max_verification_work {
            return Err(ProofError::BudgetExceeded);
        }
        Ok(())
    }
    fn allocate(&mut self, bytes: usize) -> Result<(), ProofError> {
        self.allocated = self
            .allocated
            .checked_add(bytes)
            .ok_or(ProofError::ProofLimitExceeded)?;
        if self.allocated > self.caps.max_allocated_bytes {
            return Err(ProofError::ProofLimitExceeded);
        }
        Ok(())
    }
}
fn bounded_count(d: &mut Decoder<'_>, cap: usize, min_bytes: usize) -> Result<usize, ProofError> {
    let n = d.length()?;
    if n > cap {
        return Err(ProofError::ProofLimitExceeded);
    }
    if n > d.remaining() / min_bytes {
        return Err(ProofError::InvalidProof);
    }
    Ok(n)
}
fn read_path<'a>(
    d: &mut Decoder<'a>,
    structural: usize,
    max_node: usize,
    meter: &mut VerifyMeter,
) -> Result<Vec<&'a [u8]>, ProofError> {
    let n = bounded_count(d, structural, 4)?;
    meter.nodes = meter
        .nodes
        .checked_add(n)
        .ok_or(ProofError::ProofLimitExceeded)?;
    if meter.nodes > meter.caps.max_nodes {
        return Err(ProofError::ProofLimitExceeded);
    }
    meter.allocate(n * size_of::<&[u8]>())?;
    // Inline nodes are visited too. Fixed key lengths bound the complete
    // traversal, regardless of how many nodes are supplied on the wire.
    meter.work(structural * 1024)?;
    let mut path = Vec::with_capacity(n);
    for _ in 0..n {
        let node = d.bytes()?;
        if node.len() > max_node {
            return Err(ProofError::ProofLimitExceeded);
        }
        meter.work(
            node.len()
                .checked_mul(3)
                .ok_or(ProofError::BudgetExceeded)?,
        )?;
        // walk copies the terminal value; account before entering it.
        meter.allocate(node.len())?;
        path.push(node);
    }
    Ok(path)
}
#[allow(clippy::too_many_arguments)]
fn proven(
    root: B256,
    key: &[u8],
    path: &[&[u8]],
    map: crate::MapKind,
    record_id: Option<RecordId>,
    inspection: &mut Option<&mut InspectionCollector>,
    meter: &mut VerifyMeter,
) -> Result<Option<Vec<u8>>, ProofError> {
    if let Some(inspection) = inspection.as_deref_mut() {
        // Reserve trace copies, child references and inline descendants before
        // entering the walker. Key lengths bound all visited inline nodes.
        let wire_bytes = path
            .iter()
            .try_fold(0usize, |n, bytes| n.checked_add(bytes.len()))
            .ok_or(ProofError::ProofLimitExceeded)?;
        let reserve = wire_bytes
            .checked_mul(4)
            .and_then(|n| n.checked_add((key.len() * 2 + 1) * 2048))
            .ok_or(ProofError::ProofLimitExceeded)?;
        meter.allocate(reserve)?;
        meter.work(reserve)?;
        let (value, trace) =
            arkiv_db_trie::inspect_path(root, key, path).map_err(|_| ProofError::InvalidProof)?;
        inspection.point_paths.push(InspectedMapPath {
            map,
            record_id,
            root,
            key: key.to_vec(),
            supplied_nodes: path.len(),
            trace,
        });
        Ok(value)
    } else {
        walk(root, key, path).map_err(|_| ProofError::InvalidProof)
    }
}
fn decode_row(
    bytes: &[u8],
    id: RecordId,
    query: &EqQuery,
    limits: Limits,
    meter: &mut VerifyMeter,
) -> Result<Row, ProofError> {
    meter.work(bytes.len())?;
    let mut d = Decoder::new(bytes);
    let key: RecordKey = d.get()?;
    let n = bounded_count(&mut d, limits.max_cells.min(64), 8)?;
    meter.allocate(n * size_of::<(CellName, Cell)>() + bytes.len())?;
    let mut cells = Vec::with_capacity(n);
    for _ in 0..n {
        let name: CellName = d.get()?;
        let kind: CellKind = d.get()?;
        let value = bounded_value(&mut d, limits.max_payload_len())?;
        if cells.last().is_some_and(|(previous, _)| previous >= &name) {
            return Err(ProofError::InvalidProof);
        }
        if kind == CellKind::Attribute
            && (!value.tag().is_indexable() || value.encoded_len() > Value::MAX_INDEXED_LEN)
        {
            return Err(ProofError::InvalidProof);
        }
        cells.push((name, Cell { kind, value }));
    }
    d.finish()?;
    if !cells.iter().any(|(name, cell)| {
        name == &query.attribute && cell.is_attribute() && cell.value == query.value
    }) {
        return Err(ProofError::InvalidProof);
    }
    Ok(Row {
        id,
        key: Some(key),
        cells,
    })
}

/// Verify one complete page. The caller must derive `request.snapshot`
/// from its accepted authenticated header, never from this response.
/// Continuation is accepted only when identical to `previous`, a token
/// returned by this verifier from a prior nonterminal page.
pub fn verify_eq(
    request: &EqRequest,
    previous: Option<&VerifiedContinuation>,
    bytes: &[u8],
    caps: ProofLimits,
    limits: Limits,
) -> Result<VerifiedEq, ProofError> {
    verify_eq_inner(request, previous, bytes, caps, limits, None)
}

/// Verify the entire equality witness and return inspection data only after
/// all cryptographic, row-binding, completeness and continuation checks pass.
/// Trace allocations and work also consume the caller's `ProofLimits`.
pub fn verify_eq_inspected(
    request: &EqRequest,
    previous: Option<&VerifiedContinuation>,
    bytes: &[u8],
    caps: ProofLimits,
    limits: Limits,
) -> Result<(VerifiedEq, EqInspection), ProofError> {
    let mut inspection = InspectionCollector::default();
    let verified = verify_eq_inner(
        request,
        previous,
        bytes,
        caps,
        limits,
        Some(&mut inspection),
    )?;
    Ok((
        verified,
        EqInspection {
            state_header: inspection.state_header.ok_or(ProofError::InvalidProof)?,
            namespace: inspection.namespace,
            point_paths: inspection.point_paths,
            posting_set: inspection.posting_set,
        },
    ))
}

fn verify_eq_inner(
    request: &EqRequest,
    previous: Option<&VerifiedContinuation>,
    bytes: &[u8],
    caps: ProofLimits,
    limits: Limits,
    mut inspection: Option<&mut InspectionCollector>,
) -> Result<VerifiedEq, ProofError> {
    validate_request(request, caps, limits)?;
    if request.continuation.as_ref() != previous.map(VerifiedContinuation::cursor) {
        return Err(ProofError::CursorMismatch);
    }
    if bytes.len() > caps.max_proof_bytes {
        return Err(ProofError::ProofLimitExceeded);
    }
    let mut meter = VerifyMeter::new(caps);
    meter.work(bytes.len())?;
    let mut d = Decoder::new(bytes);
    version(&mut d, "ProofResponseV1")?;
    let snapshot: SnapshotRef = d.get()?;
    let query_digest: B256 = d.get()?;
    if snapshot != request.snapshot || query_digest != request.query.digest() {
        return Err(ProofError::InvalidProof);
    }
    let tag = d.u8()?;
    if tag != 1 && tag != 2 {
        return Err(ProofError::InvalidProof);
    }
    let header: StateHeader = d.get()?;
    if header.root() != request.snapshot.state_root {
        return Err(ProofError::InvalidProof);
    }
    if let Some(inspection) = inspection.as_deref_mut() {
        inspection.state_header = Some(header.clone());
    }
    let catalog = read_path(&mut d, 17, META_NODE_BYTES, &mut meter)?;
    let namespace = proven(
        header.catalog_root,
        &request.query.namespace.to_bytes(),
        &catalog,
        crate::MapKind::Catalog,
        None,
        &mut inspection,
        &mut meter,
    )?;
    if tag == 1 {
        d.finish()?;
        if namespace.is_some() || request.continuation.is_some() {
            return Err(ProofError::InvalidProof);
        }
        return Ok(VerifiedEq::NamespaceAbsent);
    }
    let namespace = NamespaceState::decode(&namespace.ok_or(ProofError::InvalidProof)?)
        .map_err(|_| ProofError::InvalidProof)?;
    if namespace.descriptor.id != request.query.namespace {
        return Err(ProofError::InvalidProof);
    }
    if let Some(inspection) = inspection.as_deref_mut() {
        inspection.namespace = Some(crate::NamespaceRoots {
            next_record: namespace.typed.next_record,
            records: (namespace.records.root_hash(), namespace.records.len()),
            keys: (namespace.typed.keys.root_hash(), namespace.typed.keys.len()),
            rows: (namespace.typed.rows.root_hash(), namespace.typed.rows.len()),
            terms: (
                namespace.typed.terms.root_hash(),
                namespace.typed.terms.len(),
            ),
        });
    }
    let term_key = TermKey::value(&request.query.attribute, &request.query.value);
    let term_path = read_path(&mut d, 177, META_NODE_BYTES, &mut meter)?;
    let term = proven(
        namespace.typed.terms.root_hash(),
        &TrieKey::encode(&term_key),
        &term_path,
        crate::MapKind::Terms,
        None,
        &mut inspection,
        &mut meter,
    )?;
    let posting = term
        .as_deref()
        .map(Posting::decode)
        .transpose()
        .map_err(|_| ProofError::InvalidProof)?;
    if posting
        .as_ref()
        .is_some_and(|p| p.0.len() > caps.max_posting_ids as u64)
    {
        return Err(ProofError::ProofLimitExceeded);
    }
    let count = bounded_count(&mut d, caps.max_posting_ids, 8)?;
    if posting.as_ref().map_or(0, |p| p.0.len()) != count as u64 {
        return Err(ProofError::InvalidProof);
    }
    meter.allocate(count * size_of::<RecordId>())?;
    let mut ids = Vec::with_capacity(count);
    let mut rebuilt = PostingMap::new();
    for _ in 0..count {
        let id: RecordId = d.get()?;
        if ids.last().is_some_and(|last| last >= &id) {
            return Err(ProofError::InvalidProof);
        }
        // At most 17 nodes of a posting path, with conservative encoding,
        // hashing and node allocation capacity reserved before insertion.
        meter.work(17 * 1024)?;
        meter.allocate(17 * 1024)?;
        rebuilt = rebuilt
            .insert(&id, Member, &MemoryOnly, &mut Probe::new())
            .map_err(|_| ProofError::InvalidProof)?
            .0;
        ids.push(id);
    }
    if let Some(posting) = &posting
        && rebuilt.root_hash() != posting.0.root_hash()
    {
        return Err(ProofError::InvalidProof);
    }
    let (expected, next) = page_slice(&ids, request)?;
    if let Some(inspection) = inspection.as_deref_mut() {
        meter.allocate((ids.len() + expected.len()) * size_of::<RecordId>())?;
        inspection.posting_set = Some(InspectedPostingSet {
            authenticated_root: posting.as_ref().map(|p| p.0.root_hash()),
            reconstructed_root: rebuilt.root_hash(),
            ids: ids.clone(),
            selected_ids: expected.to_vec(),
        });
    }
    let rows_count = bounded_count(&mut d, caps.max_page_rows.min(limits.max_page), 16)?;
    if rows_count != expected.len() {
        return Err(ProofError::InvalidProof);
    }
    meter.allocate(rows_count * size_of::<Row>())?;
    let mut rows = Vec::with_capacity(rows_count);
    for expected_id in expected {
        let id: RecordId = d.get()?;
        if id != *expected_id {
            return Err(ProofError::InvalidProof);
        }
        let path = read_path(&mut d, 17, row_node_bound(limits)?, &mut meter)?;
        let leaf = proven(
            namespace.typed.rows.root_hash(),
            &id.ordinal().to_be_bytes(),
            &path,
            crate::MapKind::Rows,
            Some(id),
            &mut inspection,
            &mut meter,
        )?
        .ok_or(ProofError::InvalidProof)?;
        let row = decode_row(&leaf, id, &request.query, limits, &mut meter)?;
        let key = row.key.as_ref().ok_or(ProofError::InvalidProof)?;
        let path = read_path(&mut d, 67, META_NODE_BYTES, &mut meter)?;
        let binding = proven(
            namespace.typed.keys.root_hash(),
            &key.encoded(),
            &path,
            crate::MapKind::Keys,
            Some(id),
            &mut inspection,
            &mut meter,
        )?
        .ok_or(ProofError::InvalidProof)?;
        let bound: RecordId = crate::decode_exact(&binding)?;
        if bound != id {
            return Err(ProofError::InvalidProof);
        }
        rows.push(row);
    }
    let supplied_next: Option<EqCursor> = d.get()?;
    d.finish()?;
    if supplied_next != next {
        return Err(ProofError::InvalidProof);
    }
    Ok(VerifiedEq::Page(VerifiedPage {
        rows,
        next: next.map(VerifiedContinuation),
        posting_count: count,
    }))
}

// Generation keeps internal maps private. Snapshot supplies an immutable
// catalog and its loaders; no host can mutate or inspect those maps.
struct Serving<'a> {
    caps: ProofLimits,
    meter: Meter<'a>,
    bytes: usize,
    nodes: usize,
}
impl Serving<'_> {
    fn append_capacity(&self, bytes: usize) -> Result<(), ProofError> {
        if self
            .bytes
            .checked_add(bytes)
            .is_none_or(|n| n > self.caps.max_proof_bytes)
        {
            return Err(ProofError::ProofLimitExceeded);
        }
        self.meter
            .check_capacity(&[(Op::ProofByte, bytes as u64)])?;
        Ok(())
    }
    fn append(&mut self, bytes: usize) -> Result<(), ProofError> {
        self.append_capacity(bytes)?;
        self.meter.charge(Op::ProofByte, bytes as u64)?;
        self.bytes += bytes;
        Ok(())
    }
    fn path<K: TrieKey, V: TrieValue + Send + Sync + 'static>(
        &mut self,
        map: &MerkleMap<K, V>,
        key: &K,
        loader: &Loader<'_>,
        max_node: usize,
    ) -> Result<Vec<Vec<u8>>, ProofError> {
        let structural = 2 * K::MAX_LEN + 1;
        self.append(4)?;
        if map.is_empty() {
            return Ok(Vec::new());
        }
        let mut path = Vec::new();
        let mut iter = map.proof(key, loader);
        loop {
            self.append_capacity(max_node + 4)?;
            self.meter.check_capacity(&[
                (Op::ProofNode, structural as u64),
                (Op::ProofByte, (max_node + 4) as u64),
            ])?;
            if self.nodes >= self.caps.max_nodes {
                return Err(ProofError::ProofLimitExceeded);
            }
            let before = iter.probe().nodes;
            let next = iter.next();
            let visited = iter.probe().nodes - before;
            self.meter.charge(Op::ProofNode, visited)?;
            if iter.probe().nodes > structural as u64 {
                return Err(ProofError::InvalidProof);
            }
            let Some(node) = next else {
                break;
            };
            let node = node.map_err(|_| ProofError::StorageUnavailable)?;
            if node.len() > max_node {
                return Err(ProofError::ProofLimitExceeded);
            }
            self.append(node.len() + 4)?;
            self.nodes += 1;
            path.push(node);
        }
        Ok(path)
    }
}
fn write_path(e: &mut Encoder, path: &[Vec<u8>]) {
    e.length(path.len());
    for node in path {
        e.bytes(node);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn generate(
    catalog: &Catalog,
    nodes: &Nodes,
    header: StateHeader,
    limits: Limits,
    request: &EqRequest,
    caps: ProofLimits,
    budget: Budget,
) -> Result<GeneratedProof, ProofError> {
    validate_request(request, caps, limits)?;
    if header.root() != request.snapshot.state_root {
        return Err(ProofError::InvalidRequest);
    }
    let mut serving = Serving {
        caps,
        meter: Meter::new(&header.pricing, budget, None),
        bytes: 0,
        nodes: 0,
    };
    // Fixed envelope and StateHeader are bounded independently of state.
    let overhead = 4 + SnapshotRef::MIN_ENCODED_LEN + 32 + 1 + StateHeader::MIN_ENCODED_LEN;
    serving.append(overhead)?;
    serving.meter.charge(Op::DirectoryLookup, 1)?;
    let catalog_loader = nodes.catalog();
    let namespace = catalog
        .get(&request.query.namespace, &catalog_loader, &mut Probe::new())
        .map_err(|_| ProofError::StorageUnavailable)?;
    let catalog_path = serving.path(
        catalog,
        &request.query.namespace,
        &catalog_loader,
        META_NODE_BYTES,
    )?;
    let mut e = Encoder::new();
    e.u32(PROOF_VERSION);
    e.put(&request.snapshot);
    e.put(&request.query.digest());
    let mut posting_count = 0;
    if let Some(namespace) = namespace {
        if namespace.descriptor.id != request.query.namespace {
            return Err(ProofError::InvalidProof);
        }
        e.u8(2);
        e.put(&header);
        write_path(&mut e, &catalog_path);
        let loaders = nodes.namespace(request.query.namespace);
        serving
            .meter
            .charge_group(&[(Op::QueryGroup, 1), (Op::TermSeek, 1)])?;
        let term_key = TermKey::value(&request.query.attribute, &request.query.value);
        let posting = namespace
            .typed
            .terms
            .get(&term_key, &loaders.terms, &mut Probe::new())
            .map_err(|_| ProofError::StorageUnavailable)?;
        if posting
            .as_ref()
            .is_some_and(|p| p.0.len() > caps.max_posting_ids as u64)
        {
            return Err(ProofError::ProofLimitExceeded);
        }
        let term_path = serving.path(
            &namespace.typed.terms,
            &term_key,
            &loaders.terms,
            META_NODE_BYTES,
        )?;
        write_path(&mut e, &term_path);
        serving.append(4)?;
        let mut ids = Vec::new();
        if let Some(posting) = posting {
            posting_count =
                usize::try_from(posting.0.len()).map_err(|_| ProofError::ProofLimitExceeded)?;
            let mut iter = posting.0.iter(&loaders.postings);
            for _ in 0..posting_count {
                serving.append(8)?;
                serving.meter.charge(Op::PostingEntry, 1)?;
                let (id, Member) = iter
                    .next()
                    .ok_or(ProofError::InvalidProof)?
                    .map_err(|_| ProofError::StorageUnavailable)?;
                ids.push(id);
            }
        }
        e.put(&ids);
        let (selected, next) = page_slice(&ids, request)?;
        serving.append(4)?;
        e.length(selected.len());
        let row_bound = row_node_bound(limits)?;
        for id in selected {
            // The configured maximum is an allocation guard, not the size of
            // this row. Large payload limits must not reject tiny witnesses.
            if row_bound > caps.max_allocated_bytes {
                return Err(ProofError::ProofLimitExceeded);
            }
            serving
                .meter
                .check_capacity(&[(Op::RecordProbe, 1), (Op::ResultRow, 1)])?;
            serving.meter.charge(Op::RecordProbe, 1)?;
            let row: StoredRecord = namespace
                .typed
                .rows
                .get(id, &loaders.rows, &mut Probe::new())
                .map_err(|_| ProofError::StorageUnavailable)?
                .ok_or(ProofError::InvalidProof)?;
            if row.cells.len() > limits.max_cells.min(64)
                || row
                    .cells
                    .iter()
                    .any(|(_, cell)| cell.value.encoded_len() > limits.max_payload_len().max(32))
            {
                return Err(ProofError::ProofLimitExceeded);
            }
            let read_bytes = row.key.as_bytes().len()
                + row
                    .cells
                    .iter()
                    .map(|(name, cell)| name.len() + cell.value.encoded_len())
                    .sum::<usize>();
            serving
                .meter
                .charge_group(&[(Op::ResultRow, 1), (Op::ReadByte, read_bytes as u64)])?;
            serving.append(8)?;
            e.put(id);
            // Canonical row: key length + key + cell count; each cell has
            // name length/name, kind, tag, and at most four length bytes.
            // A present id's proof terminates at exactly this row. Other
            // path nodes are branches/extensions, bounded by 1024 bytes.
            let actual_row_bound = read_bytes
                .checked_add(128)
                .and_then(|n| n.checked_add(row.cells.len().checked_mul(10)?))
                .ok_or(ProofError::ProofLimitExceeded)?
                .max(META_NODE_BYTES);
            let row_path =
                serving.path(&namespace.typed.rows, id, &loaders.rows, actual_row_bound)?;
            write_path(&mut e, &row_path);
            let key_path = serving.path(
                &namespace.typed.keys,
                &row.key,
                &loaders.keys,
                META_NODE_BYTES,
            )?;
            write_path(&mut e, &key_path);
        }
        serving.append(
            1 + if next.is_some() {
                EqCursor::MIN_ENCODED_LEN
            } else {
                0
            },
        )?;
        e.put(&next);
    } else {
        if request.continuation.is_some() {
            return Err(ProofError::CursorMismatch);
        }
        e.u8(1);
        e.put(&header);
        write_path(&mut e, &catalog_path);
    }
    let bytes = e.into_bytes();
    if bytes.len() != serving.bytes {
        return Err(ProofError::InvalidProof);
    }
    let (mut receipt, _) = serving.meter.finish(ReceiptKind::Read, Outcome::Applied);
    receipt.model_version = PROOF_READ_MODEL;
    Ok(GeneratedProof {
        bytes,
        receipt,
        posting_count,
        supplied_nodes: serving.nodes,
    })
}

/// Fixed 32-byte host-key paths are at most 65 nodes. Bound untrusted evidence
/// before allocating or invoking the trie walker.
pub const HOST_PROOF_CAP: usize = 128 * 1024;
pub fn verify_host(
    root: B256,
    key: &[u8; 32],
    bytes: &[u8],
) -> Result<Option<Vec<u8>>, ProofError> {
    if bytes.len() > HOST_PROOF_CAP {
        return Err(ProofError::ProofLimitExceeded);
    }
    let mut d = Decoder::new(bytes);
    if d.u32()? != 1 {
        return Err(ProofError::InvalidProof);
    }
    let header: StateHeader = d.get()?;
    if header.root() != root {
        return Err(ProofError::InvalidProof);
    }
    let count = bounded_count(&mut d, 65, 4)?;
    let mut path = Vec::with_capacity(count);
    for _ in 0..count {
        let node = d.bytes()?;
        if node.len() > crate::HOST_VALUE_MAX + 1024 {
            return Err(ProofError::ProofLimitExceeded);
        }
        path.push(node);
    }
    if d.remaining() != 0 {
        return Err(ProofError::InvalidProof);
    }
    walk(header.host_root, key, &path)
        .map_err(|_| ProofError::InvalidProof)?
        .map(|bytes| {
            let blob = crate::layout::Blob::decode(&bytes).map_err(|_| ProofError::InvalidProof)?;
            if blob.0.len() > crate::HOST_VALUE_MAX {
                return Err(ProofError::InvalidProof);
            }
            Ok(blob.0.to_vec())
        })
        .transpose()
}
