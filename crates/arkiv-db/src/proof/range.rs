//! Index-only complete range proofs over any ordered (indexable) type, in
//! term-byte order. This profile has no pagination: every matching term, its
//! entire posting set and every matching row must fit.

use std::{collections::BTreeMap, ops::Bound};

use super::*;
use crate::TypeTag;

pub const RANGE_PROOF_PROFILE: &str = "range-complete-v1";
// Distinct envelope discriminator; neither equality nor state encoding changes.
const RANGE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeQuery {
    pub namespace: NamespaceId,
    pub attribute: CellName,
    pub lower: Bound<Value>,
    pub upper: Bound<Value>,
}

fn bound_value(bound: &Bound<Value>) -> Option<&Value> {
    match bound {
        Bound::Unbounded => None,
        Bound::Included(v) | Bound::Excluded(v) => Some(v),
    }
}

impl RangeQuery {
    pub fn digest(&self) -> B256 {
        digest("arkiv/range-query/v1", &encode(self))
    }

    /// Require at least one bound of an ordered type (every indexable type),
    /// with the same type at both ends, and `Str` bounds no longer than the
    /// hard [`Value::MAX_INDEXED_LEN`]. Reversed bounds and equal excluded
    /// bounds describe an empty interval.
    pub fn validate(&self) -> Result<(), ProofError> {
        self.tag().map(|_| ())
    }

    /// [`Self::validate`] plus the engine's configured indexed-string
    /// limit, as for an equality value.
    fn validate_limits(&self, limits: Limits) -> Result<(), ProofError> {
        self.validate()?;
        if [&self.lower, &self.upper]
            .into_iter()
            .filter_map(bound_value)
            .any(|value| matches!(value, Value::Str(_)) && value.encoded_len() > limits.indexed_value_len())
        {
            return Err(ProofError::InvalidRequest);
        }
        Ok(())
    }

    fn tag(&self) -> Result<TypeTag, ProofError> {
        let first = bound_value(&self.lower)
            .or_else(|| bound_value(&self.upper))
            .ok_or(ProofError::InvalidRequest)?;
        if !first.tag().is_ordered() {
            return Err(ProofError::UnsupportedProof);
        }
        if [&self.lower, &self.upper]
            .into_iter()
            .filter_map(bound_value)
            .any(|value| value.tag() != first.tag() || value.encoded_len() > Value::MAX_INDEXED_LEN)
        {
            return Err(ProofError::InvalidRequest);
        }
        Ok(first.tag())
    }

    fn interval(&self) -> Result<(Vec<u8>, Vec<u8>), ProofError> {
        let tag = self.tag()?;
        let start = TrieKey::encode(&TermKey::run_start(&self.attribute, tag, &[]));
        let mut end = start.clone();
        // The final MAX_INDEXED_LEN + 1 bytes hold the padded value and length.
        end[CellName::MAX_LEN + 3..].fill(255);
        let lower = match &self.lower {
            Bound::Unbounded => start,
            Bound::Included(value) => TrieKey::encode(&TermKey::value(&self.attribute, value)),
            Bound::Excluded(value) => {
                // The fixed-width successor key. The last byte is the term
                // length (at most MAX_INDEXED_LEN < 255), so it never
                // carries into the value, type or name.
                let mut key = TrieKey::encode(&TermKey::value(&self.attribute, value));
                *key.last_mut().ok_or(ProofError::InvalidRequest)? += 1;
                key
            }
        };
        let upper = match &self.upper {
            Bound::Unbounded => end,
            Bound::Included(value) => TrieKey::encode(&TermKey::value(&self.attribute, value)),
            Bound::Excluded(value) => {
                // The fixed-width predecessor key. Only the empty `Str` has
                // a zero length byte; its predecessor borrows through the
                // zero padding into the presence byte, lands below the
                // run's first value key, and the interval is empty.
                let mut key = TrieKey::encode(&TermKey::value(&self.attribute, value));
                for byte in key.iter_mut().rev() {
                    let (next, borrow) = byte.overflowing_sub(1);
                    *byte = next;
                    if !borrow {
                        break;
                    }
                }
                key
            }
        };
        Ok((lower, upper))
    }
}

fn put_bound(e: &mut Encoder, bound: &Bound<Value>) {
    match bound {
        Bound::Unbounded => e.u8(0),
        Bound::Included(value) => {
            e.u8(1);
            e.put(value);
        }
        Bound::Excluded(value) => {
            e.u8(2);
            e.put(value);
        }
    }
}
fn get_bound(d: &mut Decoder<'_>) -> Result<Bound<Value>, DecodeError> {
    let tag = d.u8()?;
    match tag {
        0 => Ok(Bound::Unbounded),
        1 | 2 => {
            let value = bounded_value(d, Value::MAX_INDEXED_LEN)?;
            Ok(if tag == 1 {
                Bound::Included(value)
            } else {
                Bound::Excluded(value)
            })
        }
        _ => Err(invalid_codec("unknown range bound")),
    }
}
impl Encode for RangeQuery {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(RANGE_VERSION);
        e.put(&self.namespace);
        e.put(&self.attribute);
        put_bound(e, &self.lower);
        put_bound(e, &self.upper);
        e.u8(1); // record-id order
        e.u8(1); // full rows
    }
}
impl Decode for RangeQuery {
    const MIN_ENCODED_LEN: usize = 4 + 8 + 5 + 2 + 2;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        if d.u32()? != RANGE_VERSION {
            return Err(invalid_codec("unknown range profile"));
        }
        let query = Self {
            namespace: d.get()?,
            attribute: d.get()?,
            lower: get_bound(d)?,
            upper: get_bound(d)?,
        };
        if d.u8()? != 1 || d.u8()? != 1 {
            return Err(invalid_codec("unknown range projection/order"));
        }
        query
            .validate()
            .map_err(|_| invalid_codec("invalid range bounds"))?;
        Ok(query)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeRequest {
    pub snapshot: SnapshotRef,
    pub query: RangeQuery,
}
impl Encode for RangeRequest {
    fn encode_to(&self, e: &mut Encoder) {
        e.put(&self.snapshot);
        e.put(&self.query);
    }
}
impl Decode for RangeRequest {
    const MIN_ENCODED_LEN: usize = SnapshotRef::MIN_ENCODED_LEN + RangeQuery::MIN_ENCODED_LEN;
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            snapshot: d.get()?,
            query: d.get()?,
        })
    }
}

/// Local resource policy, never a query limit. Exceeding any cap fails the
/// whole request; no partial result or continuation can be returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeProofLimits {
    pub max_terms: usize,
    pub max_rows: usize,
    pub max_posting_ids: usize,
    pub max_proof_bytes: usize,
    pub max_nodes: usize,
    pub max_verification_work: u64,
    pub max_allocated_bytes: usize,
}
impl Default for RangeProofLimits {
    fn default() -> Self {
        let common = ProofLimits::default();
        Self {
            max_terms: 1_000,
            max_rows: 1_000,
            max_posting_ids: common.max_posting_ids,
            max_proof_bytes: common.max_proof_bytes,
            // Separate row/key paths repeat shared branches for each result.
            max_nodes: 32_768,
            max_verification_work: 512 * 1024 * 1024,
            max_allocated_bytes: common.max_allocated_bytes,
        }
    }
}
impl RangeProofLimits {
    fn common(self) -> ProofLimits {
        ProofLimits {
            max_posting_ids: self.max_posting_ids,
            max_page_rows: self.max_rows,
            max_proof_bytes: self.max_proof_bytes,
            max_nodes: self.max_nodes,
            max_verification_work: self.max_verification_work,
            max_allocated_bytes: self.max_allocated_bytes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRangeRows {
    pub rows: Vec<Row>,
    pub term_count: usize,
    pub posting_count: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifiedRange {
    NamespaceAbsent,
    Complete(VerifiedRangeRows),
}

/// Display evidence from the same successful complete-range verifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeInspection {
    pub state_header: StateHeader,
    pub namespace: Option<crate::NamespaceRoots>,
    pub point_paths: Vec<InspectedMapPath>,
    pub interval: Option<InspectedRangeInterval>,
    pub posting_sets: Vec<InspectedRangePosting>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedRangeInterval {
    pub root: B256,
    pub lower: Vec<u8>,
    pub upper: Vec<u8>,
    pub supplied_nodes: usize,
    pub trace: arkiv_db_trie::VerifiedInterval,
    /// Successfully decoded term leaves, aligned with trace nodes. Opaque
    /// siblings have no trace node and must never receive invented values.
    pub leaves: Vec<Option<InspectedRangeLeaf>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedRangeLeaf {
    pub key: Vec<u8>,
    pub attribute: CellName,
    pub tag: TypeTag,
    pub value: Option<Value>,
    pub included: bool,
    pub posting_root: B256,
    pub posting_count: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedRangePosting {
    pub term_node_index: usize,
    pub term_key: Vec<u8>,
    pub value: Value,
    pub posting: InspectedPostingSet,
}
#[derive(Default)]
struct RangeInspectionCollector {
    points: InspectionCollector,
    interval: Option<InspectedRangeInterval>,
    posting_sets: Vec<InspectedRangePosting>,
}

fn interval_reservation(bytes: usize, meter: &mut VerifyMeter) -> Result<(), ProofError> {
    // One shared prefix; at most fifteen pending siblings at each key nibble.
    // Include Vec growth, borrowed/raw child stacks, inline decoding, returned
    // key/value copies and node-path allocations before invoking the walker.
    let depth = 2 * TermKey::LEN + 1;
    meter.allocate(depth * 16 * 256)?;
    meter.allocate(
        bytes
            .checked_mul(16)
            .ok_or(ProofError::ProofLimitExceeded)?,
    )?;
    meter.work(
        bytes
            .checked_mul(depth + 64)
            .ok_or(ProofError::BudgetExceeded)?,
    )
}

fn terms<N: AsRef<[u8]>>(
    root: B256,
    query: &RangeQuery,
    witness: &[N],
    caps: RangeProofLimits,
    meter: &mut VerifyMeter,
) -> Result<Vec<(Value, Posting)>, ProofError> {
    terms_inner(root, query, witness, caps, meter, None)
}

fn terms_inner<N: AsRef<[u8]>>(
    root: B256,
    query: &RangeQuery,
    witness: &[N],
    caps: RangeProofLimits,
    meter: &mut VerifyMeter,
    inspection: Option<&mut RangeInspectionCollector>,
) -> Result<Vec<(Value, Posting)>, ProofError> {
    let wire_bytes = witness
        .iter()
        .try_fold(0usize, |n, node| n.checked_add(node.as_ref().len()))
        .ok_or(ProofError::ProofLimitExceeded)?;
    interval_reservation(wire_bytes, meter)?;
    let (lower, upper) = query.interval()?;
    let map_error = |error| {
        if error == arkiv_db_trie::ProofError::EntryLimitExceeded {
            ProofError::ProofLimitExceeded
        } else {
            ProofError::InvalidProof
        }
    };
    let entries = if let Some(inspection) = inspection {
        // Every decoded node takes at least three RLP bytes. Inline chains
        // remain under 32 bytes; cap all visits separately from supplied nodes.
        // Reserve prefixes, child references, raw copies and display leaves
        // before the trie walker can allocate any trace.
        let visits = (wire_bytes / 3 + witness.len()).min(arkiv_db_trie::MAX_INTERVAL_TRACE_NODES);
        let reserve = wire_bytes
            .checked_mul(16)
            .and_then(|n| n.checked_add(visits * (2 * TermKey::LEN + 2048)))
            .ok_or(ProofError::ProofLimitExceeded)?;
        meter.allocate(reserve)?;
        meter.work(reserve)?;
        let (entries, trace) =
            arkiv_db_trie::inspect_interval(root, &lower, &upper, witness, caps.max_terms)
                .map_err(map_error)?;
        let leaves = trace
            .nodes
            .iter()
            .map(|node| {
                let key = node.leaf_key.as_ref()?;
                let term = TermKey::decode(key).ok()?;
                let posting = Posting::decode(node.leaf_value.as_ref()?).ok()?;
                Some(InspectedRangeLeaf {
                    key: key.clone(),
                    attribute: term.name,
                    tag: term.tag,
                    value: term
                        .value
                        .as_ref()
                        .and_then(|v| Value::from_term_bytes(term.tag, v)),
                    included: node.leaf_included,
                    posting_root: posting.0.root_hash(),
                    posting_count: posting.0.len(),
                })
            })
            .collect();
        inspection.interval = Some(InspectedRangeInterval {
            root,
            lower,
            upper,
            supplied_nodes: witness.len(),
            trace,
            leaves,
        });
        entries
    } else {
        arkiv_db_trie::walk_interval(root, &lower, &upper, witness, caps.max_terms)
            .map_err(map_error)?
    };
    meter.allocate(entries.len() * (size_of::<(Value, Posting)>() + TermKey::LEN + 64))?;
    entries
        .into_iter()
        .map(|(key, bytes)| {
            let term = TermKey::decode(&key).map_err(|_| ProofError::InvalidProof)?;
            if term.name != query.attribute || term.tag != query.tag()? {
                return Err(ProofError::InvalidProof);
            }
            let value = Value::from_term_bytes(
                term.tag,
                term.value.as_deref().ok_or(ProofError::InvalidProof)?,
            )
            .ok_or(ProofError::InvalidProof)?;
            let posting = Posting::decode(&bytes).map_err(|_| ProofError::InvalidProof)?;
            Ok((value, posting))
        })
        .collect()
}

/// Verify completeness against the caller's accepted state root. As with Eq,
/// this trusts the authenticated execution invariant that indices describe
/// the live rows, and does not establish consensus or execution correctness.
pub fn verify_range(
    request: &RangeRequest,
    bytes: &[u8],
    caps: RangeProofLimits,
    limits: Limits,
) -> Result<VerifiedRange, ProofError> {
    verify_range_inner(request, bytes, caps, limits, None)
}

/// Returns a trace only after all root, completeness, row and key checks pass.
/// Trace work and allocations consume the same caller-owned resource budgets.
pub fn verify_range_inspected(
    request: &RangeRequest,
    bytes: &[u8],
    caps: RangeProofLimits,
    limits: Limits,
) -> Result<(VerifiedRange, RangeInspection), ProofError> {
    let mut inspection = RangeInspectionCollector::default();
    let result = verify_range_inner(request, bytes, caps, limits, Some(&mut inspection))?;
    Ok((
        result,
        RangeInspection {
            state_header: inspection
                .points
                .state_header
                .ok_or(ProofError::InvalidProof)?,
            namespace: inspection.points.namespace,
            point_paths: inspection.points.point_paths,
            interval: inspection.interval,
            posting_sets: inspection.posting_sets,
        },
    ))
}

fn verify_range_inner(
    request: &RangeRequest,
    bytes: &[u8],
    caps: RangeProofLimits,
    limits: Limits,
    mut inspection: Option<&mut RangeInspectionCollector>,
) -> Result<VerifiedRange, ProofError> {
    request.query.validate_limits(limits)?;
    if bytes.len() > caps.max_proof_bytes {
        return Err(ProofError::ProofLimitExceeded);
    }
    let mut meter = VerifyMeter::new(caps.common());
    meter.work(bytes.len())?;
    let mut d = Decoder::new(bytes);
    if d.u32()? != RANGE_VERSION || d.u8()? != 3 {
        return Err(ProofError::UnsupportedVersion);
    }
    let snapshot: SnapshotRef = d.get()?;
    let query_digest: B256 = d.get()?;
    if snapshot != request.snapshot || query_digest != request.query.digest() {
        return Err(ProofError::InvalidProof);
    }
    let header: StateHeader = d.get()?;
    if header.root() != request.snapshot.state_root {
        return Err(ProofError::InvalidProof);
    }
    if let Some(i) = inspection.as_deref_mut() {
        i.points.state_header = Some(header.clone());
    }
    let path = read_path(&mut d, 17, META_NODE_BYTES, &mut meter)?;
    let namespace = proven(
        header.catalog_root,
        &request.query.namespace.to_bytes(),
        &path,
        crate::MapKind::Catalog,
        None,
        &mut inspection.as_deref_mut().map(|i| &mut i.points),
        &mut meter,
    )?;
    let Some(namespace) = namespace else {
        d.finish()?;
        return Ok(VerifiedRange::NamespaceAbsent);
    };
    let namespace = NamespaceState::decode(&namespace).map_err(|_| ProofError::InvalidProof)?;
    if namespace.descriptor.id != request.query.namespace {
        return Err(ProofError::InvalidProof);
    }
    if let Some(i) = inspection.as_deref_mut() {
        i.points.namespace = Some(crate::NamespaceRoots {
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
    let witness = read_path(
        &mut d,
        caps.max_nodes.min(bytes.len() / 4),
        META_NODE_BYTES,
        &mut meter,
    )?;
    let terms = terms_inner(
        namespace.typed.terms.root_hash(),
        &request.query,
        &witness,
        caps,
        &mut meter,
        inspection.as_deref_mut(),
    )?;
    let mut selected = BTreeMap::new();
    let mut total = 0usize;
    for (value, posting) in &terms {
        let count = bounded_count(&mut d, caps.max_posting_ids.saturating_sub(total), 8)?;
        if count as u64 != posting.0.len() {
            return Err(ProofError::InvalidProof);
        }
        total = total
            .checked_add(count)
            .ok_or(ProofError::ProofLimitExceeded)?;
        if total > caps.max_rows {
            return Err(ProofError::ProofLimitExceeded);
        }
        meter.allocate(count * (128 + size_of::<Value>()))?;
        let mut ids = Vec::new();
        if inspection.is_some() {
            meter.allocate(count * size_of::<RecordId>() * 2)?;
            ids.reserve(count);
        }
        let mut rebuilt = PostingMap::new();
        let mut last = None;
        for _ in 0..count {
            let id: RecordId = d.get()?;
            if last.is_some_and(|prior| prior >= id) || selected.contains_key(&id) {
                return Err(ProofError::InvalidProof);
            }
            meter.work(17 * 1024)?;
            meter.allocate(17 * 1024)?;
            rebuilt = rebuilt
                .insert(&id, Member, &MemoryOnly, &mut Probe::new())
                .map_err(|_| ProofError::InvalidProof)?
                .0;
            selected.insert(id, value.clone());
            if inspection.is_some() {
                ids.push(id);
            }
            last = Some(id);
        }
        if rebuilt.root_hash() != posting.0.root_hash() {
            return Err(ProofError::InvalidProof);
        }
        if let Some(i) = inspection.as_deref_mut() {
            let term_key = TrieKey::encode(&TermKey::value(&request.query.attribute, value));
            let interval = i.interval.as_ref().ok_or(ProofError::InvalidProof)?;
            let term_node_index = interval
                .trace
                .nodes
                .iter()
                .position(|node| node.leaf_key.as_ref() == Some(&term_key))
                .ok_or(ProofError::InvalidProof)?;
            i.posting_sets.push(InspectedRangePosting {
                term_node_index,
                term_key,
                value: value.clone(),
                posting: InspectedPostingSet {
                    authenticated_root: Some(posting.0.root_hash()),
                    reconstructed_root: rebuilt.root_hash(),
                    selected_ids: ids.clone(),
                    ids,
                },
            });
        }
    }
    let count = bounded_count(&mut d, caps.max_rows, 16)?;
    if count != selected.len() {
        return Err(ProofError::InvalidProof);
    }
    meter.allocate(count * size_of::<Row>())?;
    let mut rows = Vec::with_capacity(count);
    for (expected, value) in selected {
        let id: RecordId = d.get()?;
        if id != expected {
            return Err(ProofError::InvalidProof);
        }
        let path = read_path(&mut d, 17, row_node_bound(limits)?, &mut meter)?;
        let leaf = proven(
            namespace.typed.rows.root_hash(),
            &id.ordinal().to_be_bytes(),
            &path,
            crate::MapKind::Rows,
            Some(id),
            &mut inspection.as_deref_mut().map(|i| &mut i.points),
            &mut meter,
        )?
        .ok_or(ProofError::InvalidProof)?;
        let eq = EqQuery {
            namespace: request.query.namespace,
            attribute: request.query.attribute.clone(),
            value,
        };
        let row = decode_row(&leaf, id, &eq, limits, &mut meter)?;
        let key = row.key.as_ref().ok_or(ProofError::InvalidProof)?;
        let path = read_path(&mut d, 67, META_NODE_BYTES, &mut meter)?;
        let binding = proven(
            namespace.typed.keys.root_hash(),
            &key.encoded(),
            &path,
            crate::MapKind::Keys,
            Some(id),
            &mut inspection.as_deref_mut().map(|i| &mut i.points),
            &mut meter,
        )?
        .ok_or(ProofError::InvalidProof)?;
        if crate::decode_exact::<RecordId>(&binding)? != id {
            return Err(ProofError::InvalidProof);
        }
        rows.push(row);
    }
    d.finish()?;
    Ok(VerifiedRange::Complete(VerifiedRangeRows {
        rows,
        term_count: terms.len(),
        posting_count: total,
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_range(
    catalog: &Catalog,
    nodes: &Nodes,
    header: StateHeader,
    limits: Limits,
    request: &RangeRequest,
    caps: RangeProofLimits,
    budget: Budget,
) -> Result<GeneratedProof, ProofError> {
    request.query.validate_limits(limits)?;
    let (lower, upper) = request.query.interval()?;
    if header.root() != request.snapshot.state_root {
        return Err(ProofError::InvalidRequest);
    }
    let mut guard = VerifyMeter::new(caps.common());
    // Output encoder growth plus point-path scratch. Reserve before any map
    // traversal; the verifier separately meters reconstruction and row copies.
    guard.allocate(
        caps.max_proof_bytes
            .checked_mul(4)
            .ok_or(ProofError::ProofLimitExceeded)?,
    )?;
    let mut serving = Serving {
        caps: caps.common(),
        meter: Meter::new(&header.pricing, budget, None),
        bytes: 0,
        nodes: 0,
    };
    serving.append(4 + 1 + SnapshotRef::MIN_ENCODED_LEN + 32 + StateHeader::MIN_ENCODED_LEN)?;
    serving.meter.charge(Op::DirectoryLookup, 1)?;
    let loader = nodes.catalog();
    let namespace = catalog
        .get(&request.query.namespace, &loader, &mut Probe::new())
        .map_err(|_| ProofError::StorageUnavailable)?;
    let path = serving.path(catalog, &request.query.namespace, &loader, META_NODE_BYTES)?;
    let mut e = Encoder::new();
    e.u32(RANGE_VERSION);
    e.u8(3);
    e.put(&request.snapshot);
    e.put(&request.query.digest());
    e.put(&header);
    write_path(&mut e, &path);
    let mut posting_count = 0usize;
    if let Some(namespace) = namespace {
        let loaders = nodes.namespace(request.query.namespace);
        serving
            .meter
            .charge_group(&[(Op::QueryGroup, 1), (Op::TermSeek, 1)])?;
        interval_reservation(0, &mut guard)?;
        serving.append(4)?;
        let mut iter = namespace
            .typed
            .terms
            .interval_proof(&lower, &upper, &loaders.terms)
            .map_err(|_| ProofError::InvalidRequest)?;
        let mut witness = Vec::new();
        loop {
            serving.append_capacity(META_NODE_BYTES + 4)?;
            // A supplied node contains at most its byte length in inline nodes.
            // Between two yields at most a key-depth stack of such nodes ends.
            let visit_bound = (2 * TermKey::LEN + 1) * META_NODE_BYTES;
            serving
                .meter
                .check_capacity(&[(Op::ProofNode, visit_bound as u64)])?;
            if serving.nodes >= caps.max_nodes {
                return Err(ProofError::ProofLimitExceeded);
            }
            guard.work(visit_bound)?;
            guard.allocate(META_NODE_BYTES * 2)?;
            let before = iter.probe().nodes;
            let node = iter.next();
            serving
                .meter
                .charge(Op::ProofNode, iter.probe().nodes - before)?;
            let Some(node) = node else {
                break;
            };
            let node = node.map_err(|_| ProofError::StorageUnavailable)?;
            if node.len() > META_NODE_BYTES {
                return Err(ProofError::ProofLimitExceeded);
            }
            serving.append(node.len() + 4)?;
            serving.nodes += 1;
            witness.push(node);
        }
        write_path(&mut e, &witness);
        let terms = terms(
            namespace.typed.terms.root_hash(),
            &request.query,
            &witness,
            caps,
            &mut guard,
        )?;
        let mut selected = BTreeMap::new();
        for (value, expected_posting) in terms {
            serving
                .meter
                .charge_group(&[(Op::TermVisit, 1), (Op::TermSeek, 1)])?;
            // Keep the snapshot's in-memory links when present. Decoding the
            // authenticated term value alone creates a stored-root handle.
            let key = TermKey::value(&request.query.attribute, &value);
            let posting = namespace
                .typed
                .terms
                .get(&key, &loaders.terms, &mut Probe::new())
                .map_err(|_| ProofError::StorageUnavailable)?
                .ok_or(ProofError::InvalidProof)?;
            if posting.0.root_hash() != expected_posting.0.root_hash()
                || posting.0.len() != expected_posting.0.len()
            {
                return Err(ProofError::InvalidProof);
            }
            let count =
                usize::try_from(posting.0.len()).map_err(|_| ProofError::ProofLimitExceeded)?;
            posting_count = posting_count
                .checked_add(count)
                .ok_or(ProofError::ProofLimitExceeded)?;
            if posting_count > caps.max_posting_ids || posting_count > caps.max_rows {
                return Err(ProofError::ProofLimitExceeded);
            }
            guard.allocate(count * 128)?;
            serving.append(4)?;
            e.length(count);
            let mut postings = posting.0.iter(&loaders.postings);
            for _ in 0..count {
                serving.append(8)?;
                serving.meter.charge(Op::PostingEntry, 1)?;
                let (id, Member) = postings
                    .next()
                    .ok_or(ProofError::InvalidProof)?
                    .map_err(|_| ProofError::StorageUnavailable)?;
                if selected.insert(id, ()).is_some() {
                    return Err(ProofError::InvalidProof);
                }
                e.put(&id);
            }
        }
        serving.append(4)?;
        e.length(selected.len());
        for id in selected.keys() {
            let row_bound = row_node_bound(limits)?;
            // Admit the maximum temporary row allocation/read before loading
            // it, then account actual bytes after decoding. Reserving it as
            // cumulative actual allocation would reject tiny rows merely
            // because the configured payload maximum is large.
            if guard
                .allocated
                .checked_add(row_bound)
                .is_none_or(|n| n > caps.max_allocated_bytes)
            {
                return Err(ProofError::ProofLimitExceeded);
            }
            if guard
                .work
                .checked_add(row_bound as u64)
                .is_none_or(|n| n > caps.max_verification_work)
            {
                return Err(ProofError::BudgetExceeded);
            }
            serving.meter.check_capacity(&[
                (Op::RecordProbe, 1),
                (Op::ResultRow, 1),
                (Op::ReadByte, row_bound as u64),
            ])?;
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
            guard.allocate(
                read_bytes
                    .checked_mul(4)
                    .ok_or(ProofError::ProofLimitExceeded)?,
            )?;
            guard.work(read_bytes)?;
            serving
                .meter
                .charge_group(&[(Op::ResultRow, 1), (Op::ReadByte, read_bytes as u64)])?;
            serving.append(8)?;
            e.put(id);
            let bound = read_bytes
                .checked_add(128 + row.cells.len() * 10)
                .ok_or(ProofError::ProofLimitExceeded)?
                .max(META_NODE_BYTES);
            let path = serving.path(&namespace.typed.rows, id, &loaders.rows, bound)?;
            write_path(&mut e, &path);
            let path = serving.path(
                &namespace.typed.keys,
                &row.key,
                &loaders.keys,
                META_NODE_BYTES,
            )?;
            write_path(&mut e, &path);
        }
    }
    let bytes = e.into_bytes();
    if bytes.len() != serving.bytes {
        return Err(ProofError::InvalidProof);
    }
    // Generation must never return an artifact that the same local policy
    // cannot verify, including cumulative verification work/allocation limits.
    verify_range(request, &bytes, caps, limits)?;
    let (mut receipt, _) = serving.meter.finish(ReceiptKind::Read, Outcome::Applied);
    receipt.model_version = PROOF_READ_MODEL;
    Ok(GeneratedProof {
        bytes,
        receipt,
        posting_count,
        supplied_nodes: serving.nodes,
    })
}
