//! Namespace-local typed records, their term indexes and query evaluation.
//!
//! Everything here belongs to exactly one namespace: the key bindings, the
//! rows (the all-live-records domain), and the term map that holds one
//! posting per `(name, type, value)` and one existence posting per
//! `(name, type)`. All of them are Merkle Patricia maps (see
//! [`crate::layout`] for their key and leaf encodings), so a namespace state
//! clones in O(1), a transfer shares them unchanged, and their roots enter
//! the namespace's catalog leaf.
//!
//! Every map operation takes the namespace's loaders and may fail with a
//! storage error ([`Fault::Storage`]); a failed read is never an absent key.
//! Queries evaluate over owned trie iterators whose items are `Result`s.
//!
//! Invariant: the term map is exactly derived from the attribute cells of
//! the live rows. Every write maintains it in the same operation, and every
//! charge is taken before the bounded step it covers.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    ops::Bound,
    sync::Arc,
};

use arkiv_db_storage::versioned::StorageError;
use arkiv_db_trie::{Probe, Range};

use crate::{
    layout::{Keys, Member, Posting, PostingMap, Rows, StoredRecord, TermKey, Terms},
    meter::{Meter, OperationClass, Rejection},
    nodes::{Fault, Loader, NamespaceLoaders, corrupt},
    query::{CellSelect, Literal, Projection, Query, QueryFault, Term},
    record::{ArgumentFault, Cell, CellName, Limits, RecordId, RecordKey, Row, TypeTag, Value},
};

/// The typed state of one namespace.
#[derive(Clone, Debug)]
pub(crate) struct TypedState {
    /// The ordinal the next record creation receives.
    pub(crate) next_record: u64,
    pub(crate) keys: Keys,
    pub(crate) rows: Rows,
    pub(crate) terms: Terms,
}

impl TypedState {
    pub(crate) const fn new() -> Self {
        Self {
            next_record: 1,
            keys: Keys::new(),
            rows: Rows::new(),
            terms: Terms::new(),
        }
    }

    /// Number of live records: an unmetered statistic.
    pub(crate) const fn record_count(&self) -> u64 {
        self.rows.len()
    }

    /// Number of index terms (value and existence): an unmetered statistic.
    pub(crate) const fn term_count(&self) -> u64 {
        self.terms.len()
    }

    /// Whether the rows, keys and terms of both states have the same roots,
    /// hence the same contents. Compares stored ids or shared nodes first;
    /// may hash nodes built since their last hashing, never reads storage.
    pub(crate) fn shares_with(&self, other: &Self) -> bool {
        self.rows.same_root(&other.rows)
            && self.keys.same_root(&other.keys)
            && self.terms.same_root(&other.terms)
    }

    /// Test aid: whether the key bindings and the term map are exactly what
    /// the live rows imply. O(rows × cells); unmetered.
    pub(crate) fn is_consistent(&self, loaders: &NamespaceLoaders<'_>) -> Result<bool, Fault> {
        let mut probe = Probe::new();
        let mut expected: BTreeMap<TermKey, BTreeSet<RecordId>> = BTreeMap::new();
        for entry in self.rows.iter(&loaders.rows) {
            let (id, row) = entry?;
            if self.keys.get(&row.key, &loaders.keys, &mut probe)? != Some(id) {
                return Ok(false);
            }
            for (name, cell) in row.cells.iter() {
                for term in terms_of(name, cell) {
                    expected.entry(term).or_default().insert(id);
                }
            }
        }
        if self.keys.len() != self.rows.len() || self.terms.len() != units(expected.len()) {
            return Ok(false);
        }
        let mut wanted = expected.iter();
        for entry in self.terms.iter(&loaders.terms) {
            let (term, posting) = entry?;
            let Some((want_term, want_ids)) = wanted.next() else {
                return Ok(false);
            };
            if term != *want_term || posting.0.len() != units(want_ids.len()) {
                return Ok(false);
            }
            let mut ids = want_ids.iter();
            for member in posting.0.iter(&loaders.postings) {
                let (id, Member) = member?;
                if ids.next() != Some(&id) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

fn units(length: usize) -> u64 {
    u64::try_from(length).expect("lengths fit in 64 bits")
}

fn cell_bytes(name: &CellName, cell: &Cell) -> u64 {
    units(name.len() + cell.value.encoded_len())
}

/// The logical size of a stored row: key bytes plus name and encoded value
/// bytes of every cell, the `RowByte` count of a write that leaves it behind.
fn row_bytes(key: &RecordKey, cells: &[(CellName, Cell)]) -> u64 {
    cells
        .iter()
        .map(|(name, cell)| cell_bytes(name, cell))
        .sum::<u64>()
        + units(key.as_bytes().len())
}

fn find<'c>(cells: &'c [(CellName, Cell)], name: &CellName) -> Option<&'c Cell> {
    cells
        .binary_search_by(|(candidate, _)| candidate.cmp(name))
        .ok()
        .map(|index| &cells[index].1)
}

// ---------------------------------------------------------------------------
// Admission validation (before any charge)
// ---------------------------------------------------------------------------

fn validate_cells(limits: &Limits, cells: &[(CellName, Cell)]) -> Result<(), ArgumentFault> {
    if cells.len() > limits.max_cells {
        return Err(ArgumentFault::TooManyCells);
    }
    let mut names = BTreeSet::new();
    for (name, cell) in cells {
        if !names.insert(name) {
            return Err(ArgumentFault::DuplicateCell);
        }
        if name.len() > limits.max_cell_name_len {
            return Err(ArgumentFault::NameTooLong);
        }
        if !limits.admits_value(&cell.value, cell.is_attribute()) {
            return Err(ArgumentFault::ValueTooLarge);
        }
        if cell.is_attribute() && !cell.value.tag().is_indexable() {
            return Err(ArgumentFault::BytesNotIndexable);
        }
    }
    Ok(())
}

/// Validates a create's cells.
pub(crate) fn validate_create(
    limits: &Limits,
    cells: &[(CellName, Cell)],
) -> Result<(), ArgumentFault> {
    validate_cells(limits, cells)
}

/// Validates a patch's set and remove lists.
pub(crate) fn validate_patch(
    limits: &Limits,
    set: &[(CellName, Cell)],
    remove: &[CellName],
) -> Result<(), ArgumentFault> {
    if set.len() + remove.len() > limits.max_cells {
        return Err(ArgumentFault::TooManyCells);
    }
    validate_cells(limits, set)?;
    let mut removed = BTreeSet::new();
    for name in remove {
        if !removed.insert(name) {
            return Err(ArgumentFault::DuplicateCell);
        }
        if name.len() > limits.max_cell_name_len {
            return Err(ArgumentFault::NameTooLong);
        }
    }
    if set.iter().any(|(name, _)| removed.contains(name)) {
        return Err(ArgumentFault::SetAndRemoveOverlap);
    }
    Ok(())
}

fn validate_value(limits: &Limits, value: &Value) -> Result<(), QueryFault> {
    if !value.tag().is_indexable() {
        return Err(QueryFault::NotIndexable);
    }
    if matches!(value, Value::Str(_)) && value.encoded_len() > limits.indexed_value_len() {
        return Err(QueryFault::ValueTooLarge);
    }
    Ok(())
}

fn bound_value(bound: &Bound<Value>) -> Option<&Value> {
    match bound {
        Bound::Included(value) | Bound::Excluded(value) => Some(value),
        Bound::Unbounded => None,
    }
}

/// The ordered type a validated range predicate reads.
fn range_tag(lower: &Bound<Value>, upper: &Bound<Value>) -> TypeTag {
    bound_value(lower)
        .or(bound_value(upper))
        .expect("validated ranges have a bound")
        .tag()
}

/// Validates a projection: a named selection may not exceed the cell cap.
pub(crate) fn validate_projection(
    limits: &Limits,
    projection: &Projection,
) -> Result<(), QueryFault> {
    match &projection.cells {
        CellSelect::Named(names) if names.len() > limits.max_cells => {
            Err(QueryFault::ProjectionTooLarge)
        }
        _ => Ok(()),
    }
}

/// Validates a query and page limit against the instance limits.
pub(crate) fn validate_query(
    limits: &Limits,
    query: &Query,
    limit: usize,
) -> Result<(), QueryFault> {
    if query.groups.is_empty() {
        return Err(QueryFault::NoGroups);
    }
    if query.groups.len() > limits.max_groups {
        return Err(QueryFault::TooManyGroups);
    }
    if query
        .groups
        .iter()
        .any(|group| group.len() > limits.max_literals)
    {
        return Err(QueryFault::TooManyLiterals);
    }
    if limit == 0 {
        return Err(QueryFault::ZeroLimit);
    }
    if limit > limits.max_page {
        return Err(QueryFault::LimitTooLarge);
    }
    for literal in query.groups.iter().flatten() {
        match &literal.term {
            Term::Eq { value, .. } | Term::NotEq { value, .. } => validate_value(limits, value)?,
            Term::Range { lower, upper, .. } => {
                let (Some(first), second) = (
                    bound_value(lower).or(bound_value(upper)),
                    bound_value(lower).and(bound_value(upper)),
                ) else {
                    return Err(QueryFault::UnboundedRange);
                };
                if second.is_some_and(|other| other.tag() != first.tag()) {
                    return Err(QueryFault::MixedBounds);
                }
                if !first.tag().is_ordered() {
                    return Err(QueryFault::NotOrdered);
                }
                // A `Str` bound obeys the indexed-string limit, as an
                // equality value does: the seek key holds its bytes.
                for bound in [bound_value(lower), bound_value(upper)]
                    .into_iter()
                    .flatten()
                {
                    validate_value(limits, bound)?;
                }
            }
            Term::StartsWith { prefix, .. } => {
                if prefix.len() > limits.indexed_value_len() {
                    return Err(QueryFault::ValueTooLarge);
                }
            }
            Term::Exists { tag, .. } => {
                if !tag.is_indexable() {
                    return Err(QueryFault::NotIndexable);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Index maintenance
// ---------------------------------------------------------------------------

/// The terms an attribute cell holds: its value term, then its existence
/// term. A field holds none.
fn terms_of(name: &CellName, cell: &Cell) -> Vec<TermKey> {
    if cell.is_attribute() {
        vec![
            TermKey::value(name, &cell.value),
            TermKey::existence(name, cell.value.tag()),
        ]
    } else {
        Vec::new()
    }
}

/// Adds `id` to `term`'s posting, creating the term when it did not exist.
/// The lookup precedes the charge because it decides whether creation is
/// charged; the insert follows the charge. `RootUpdate` 2 covers the two
/// logical entry writes: the posting entry and the term leaf.
fn insert_membership(
    terms: &mut Terms,
    loaders: &NamespaceLoaders<'_>,
    term: TermKey,
    id: RecordId,
    meter: &mut Meter<'_>,
) -> Result<(), Fault> {
    let mut probe = Probe::new();
    let existing = terms.get(&term, &loaders.terms, &mut probe);
    meter.note_index_nodes(probe.nodes);
    let existing = existing?;
    let created = existing.is_none();
    meter.charge_group(&[
        (OperationClass::IndexInsert, 1),
        (OperationClass::IndexTermCreate, u64::from(created)),
        (OperationClass::RootUpdate, 2),
    ])?;
    let mut probe = Probe::new();
    let posting = existing.map_or_else(PostingMap::new, |posting| posting.0);
    let inserted = posting
        .insert(&id, Member, &loaders.postings, &mut probe)
        .and_then(|(posting, _)| terms.insert(&term, Posting(posting), &loaders.terms, &mut probe));
    meter.note_index_nodes(probe.nodes);
    let (updated, _) = inserted?;
    *terms = updated;
    Ok(())
}

/// Removes `id` from `term`'s posting, dropping the term when it empties.
/// `RootUpdate` 2 covers the posting entry and the term leaf.
fn remove_membership(
    terms: &mut Terms,
    loaders: &NamespaceLoaders<'_>,
    term: &TermKey,
    id: RecordId,
    meter: &mut Meter<'_>,
) -> Result<(), Fault> {
    meter.charge_group(&[
        (OperationClass::IndexRemove, 1),
        (OperationClass::RootUpdate, 2),
    ])?;
    let mut probe = Probe::new();
    let removed = remove_member(terms, loaders, term, id, &mut probe);
    meter.note_index_nodes(probe.nodes);
    *terms = removed?;
    Ok(())
}

fn remove_member(
    terms: &Terms,
    loaders: &NamespaceLoaders<'_>,
    term: &TermKey,
    id: RecordId,
    probe: &mut Probe,
) -> Result<Terms, Fault> {
    let posting = terms
        .get(term, &loaders.terms, probe)?
        .ok_or_else(|| corrupt(format!("a held term is missing from the index: {term:?}")))?;
    let (posting, removed) = posting.0.remove(&id, &loaders.postings, probe)?;
    if removed.is_none() {
        return Err(corrupt(format!("{id} is missing from the posting of {term:?}")).into());
    }
    let (updated, _) = if posting.is_empty() {
        terms.remove(term, &loaders.terms, probe)?
    } else {
        terms.insert(term, Posting(posting), &loaders.terms, probe)?
    };
    Ok(updated)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Looks up the live binding of `key`, charging the point read.
fn lookup_key(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    key: &RecordKey,
    meter: &mut Meter<'_>,
) -> Result<Option<RecordId>, Fault> {
    meter.charge(OperationClass::PointRead, 1)?;
    let mut probe = Probe::new();
    let found = state.keys.get(key, &loaders.keys, &mut probe);
    meter.note_record_nodes(probe.nodes);
    Ok(found?)
}

/// Loads the row of a live id. A missing row, or one bound to another key
/// than `key` when the caller looked it up by key, is corrupt state.
fn load_row(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    id: RecordId,
    key: Option<&RecordKey>,
    meter: &mut Meter<'_>,
) -> Result<StoredRecord, Fault> {
    let mut probe = Probe::new();
    let row = state.rows.get(&id, &loaders.rows, &mut probe);
    meter.note_record_nodes(probe.nodes);
    let row = row?.ok_or_else(|| corrupt(format!("{id} is live but has no row")))?;
    if key.is_some_and(|key| *key != row.key) {
        return Err(corrupt(format!("the row of {id} holds another key")).into());
    }
    Ok(row)
}

/// Creates a record; the caller has validated the cells.
pub(crate) fn create(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    key: RecordKey,
    mut cells: Vec<(CellName, Cell)>,
    meter: &mut Meter<'_>,
) -> Result<(TypedState, RecordId), Fault> {
    if lookup_key(state, loaders, &key, meter)?.is_some() {
        return Err(Rejection::AlreadyExists.into());
    }
    let id = RecordId::from_ordinal(state.next_record).ok_or(Rejection::CounterExhausted)?;
    let next_record = state
        .next_record
        .checked_add(1)
        .ok_or(Rejection::CounterExhausted)?;
    cells.sort_by(|(left, _), (right, _)| left.cmp(right));
    let bytes: u64 = cells
        .iter()
        .map(|(name, cell)| cell_bytes(name, cell))
        .sum();
    // Three logical entry writes: the key binding, the row and the
    // namespace's catalog leaf.
    meter.charge_group(&[
        (OperationClass::RecordCreate, 1),
        (OperationClass::CellWrite, units(cells.len())),
        (OperationClass::CellByte, bytes),
        (OperationClass::RowByte, row_bytes(&key, &cells)),
        (OperationClass::RootUpdate, 3),
    ])?;
    let mut terms = state.terms.clone();
    for (name, cell) in &cells {
        for term in terms_of(name, cell) {
            insert_membership(&mut terms, loaders, term, id, meter)?;
        }
    }
    let mut probe = Probe::new();
    let row = StoredRecord {
        key: key.clone(),
        cells: Arc::from(cells),
    };
    let inserted = state
        .keys
        .insert(&key, id, &loaders.keys, &mut probe)
        .and_then(|(keys, _)| {
            let (rows, _) = state.rows.insert(&id, row, &loaders.rows, &mut probe)?;
            Ok((keys, rows))
        });
    meter.note_record_nodes(probe.nodes);
    let (keys, rows) = inserted?;
    Ok((
        TypedState {
            next_record,
            keys,
            rows,
            terms,
        },
        id,
    ))
}

/// Patches a record; the caller has validated the lists.
pub(crate) fn patch(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    limits: &Limits,
    key: &RecordKey,
    set: Vec<(CellName, Cell)>,
    remove: Vec<CellName>,
    meter: &mut Meter<'_>,
) -> Result<(TypedState, RecordId), Fault> {
    let id = lookup_key(state, loaders, key, meter)?.ok_or(Rejection::NotFound)?;
    if set.is_empty() && remove.is_empty() {
        // golem-db semantics: an empty patch of an existing record is a
        // successful no-op. It pays the key lookup and the patch base
        // charge, loads and writes nothing, and leaves every root unchanged.
        meter.charge(OperationClass::RecordPatch, 1)?;
        return Ok((state.clone(), id));
    }
    let row = load_row(state, loaders, id, Some(key), meter)?;
    // The resulting cell list: old cells, replaced or extended by `set`,
    // minus `remove`, kept sorted by name. One linear merge of the sorted
    // lists: O(cells + set + remove) moves.
    let mut set = set;
    set.sort_by(|(left, _), (right, _)| left.cmp(right));
    let removed: BTreeSet<&CellName> = remove.iter().collect();
    let mut cells: Vec<(CellName, Cell)> = Vec::with_capacity(row.cells.len() + set.len());
    let mut old = row.cells.iter().peekable();
    let mut new = set.iter().peekable();
    loop {
        let take_new = match (old.peek(), new.peek()) {
            (None, None) => break,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some((left, _)), Some((right, _))) => {
                if left == right {
                    old.next();
                }
                left >= right
            }
        };
        let (name, cell) = if take_new {
            new.next().expect("peeked")
        } else {
            old.next().expect("peeked")
        };
        if !removed.contains(name) {
            cells.push((name.clone(), cell.clone()));
        }
    }
    if cells.len() > limits.max_cells {
        return Err(Rejection::RecordFull.into());
    }
    let bytes: u64 = set.iter().map(|(name, cell)| cell_bytes(name, cell)).sum();
    // Two logical entry writes: the rewritten row and the namespace's
    // catalog leaf; the key binding is unchanged.
    meter.charge_group(&[
        (OperationClass::RecordPatch, 1),
        (OperationClass::CellWrite, units(set.len())),
        (OperationClass::CellByte, bytes),
        (OperationClass::CellRemove, units(remove.len())),
        (OperationClass::RowByte, row_bytes(&row.key, &cells)),
        (OperationClass::RootUpdate, 2),
    ])?;
    let mut changed: BTreeSet<&CellName> = set.iter().map(|(name, _)| name).collect();
    changed.extend(remove.iter());
    let mut terms = state.terms.clone();
    for name in changed {
        let old = find(&row.cells, name).map_or_else(Vec::new, |cell| terms_of(name, cell));
        let new = find(&cells, name).map_or_else(Vec::new, |cell| terms_of(name, cell));
        for term in old.iter().filter(|term| !new.contains(term)) {
            remove_membership(&mut terms, loaders, term, id, meter)?;
        }
        for term in new.into_iter().filter(|term| !old.contains(term)) {
            insert_membership(&mut terms, loaders, term, id, meter)?;
        }
    }
    let mut probe = Probe::new();
    let patched = StoredRecord {
        key: row.key.clone(),
        cells: Arc::from(cells),
    };
    let inserted = state.rows.insert(&id, patched, &loaders.rows, &mut probe);
    meter.note_record_nodes(probe.nodes);
    let (rows, _) = inserted?;
    Ok((
        TypedState {
            next_record: state.next_record,
            keys: state.keys.clone(),
            rows,
            terms,
        },
        id,
    ))
}

/// Deletes a record and every index membership it held.
pub(crate) fn delete(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    key: &RecordKey,
    meter: &mut Meter<'_>,
) -> Result<(TypedState, RecordId), Fault> {
    let id = lookup_key(state, loaders, key, meter)?.ok_or(Rejection::NotFound)?;
    let row = load_row(state, loaders, id, Some(key), meter)?;
    // Three logical entry writes: the key binding, the row and the
    // namespace's catalog leaf.
    meter.charge_group(&[
        (OperationClass::RecordDelete, 1),
        (OperationClass::CellRemove, units(row.cells.len())),
        (OperationClass::RootUpdate, 3),
    ])?;
    let mut terms = state.terms.clone();
    for (name, cell) in row.cells.iter() {
        for term in terms_of(name, cell) {
            remove_membership(&mut terms, loaders, &term, id, meter)?;
        }
    }
    let mut probe = Probe::new();
    let removed = state
        .keys
        .remove(key, &loaders.keys, &mut probe)
        .and_then(|(keys, _)| {
            let (rows, _) = state.rows.remove(&id, &loaders.rows, &mut probe)?;
            Ok((keys, rows))
        });
    meter.note_record_nodes(probe.nodes);
    let (keys, rows) = removed?;
    Ok((
        TypedState {
            next_record: state.next_record,
            keys,
            rows,
            terms,
        },
        id,
    ))
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// The cells a projection selects, walking the record's own (bounded) cell
/// list.
fn projected<'r>(
    row: &'r StoredRecord,
    projection: &'r Projection,
) -> impl Iterator<Item = &'r (CellName, Cell)> {
    row.cells
        .iter()
        .filter(move |(name, _)| match &projection.cells {
            CellSelect::None => false,
            CellSelect::All => true,
            CellSelect::Named(names) => names.contains(name),
        })
}

/// Charges `{ResultRow 1, ReadByte p}` for one row and then materializes it.
fn emit(
    id: RecordId,
    row: &StoredRecord,
    projection: &Projection,
    meter: &mut Meter<'_>,
) -> Result<Row, Rejection> {
    let mut bytes: u64 = projected(row, projection)
        .map(|(name, cell)| cell_bytes(name, cell))
        .sum();
    if projection.key {
        bytes += units(row.key.as_bytes().len());
    }
    meter.charge_group(&[
        (OperationClass::ResultRow, 1),
        (OperationClass::ReadByte, bytes),
    ])?;
    Ok(Row {
        id,
        key: projection.key.then(|| row.key.clone()),
        cells: projected(row, projection).cloned().collect(),
    })
}

/// Validates a `terms` read against the instance limits.
pub(crate) fn validate_terms(
    limits: &Limits,
    tag: TypeTag,
    lower: &Bound<Value>,
    limit: usize,
) -> Result<(), QueryFault> {
    if !tag.is_indexable() {
        return Err(QueryFault::NotIndexable);
    }
    if let Some(bound) = bound_value(lower) {
        if bound.tag() != tag {
            return Err(QueryFault::MixedBounds);
        }
        if matches!(bound, Value::Str(_)) && bound.encoded_len() > limits.indexed_value_len() {
            return Err(QueryFault::ValueTooLarge);
        }
    }
    if limit == 0 {
        return Err(QueryFault::ZeroLimit);
    }
    if limit > limits.max_page {
        return Err(QueryFault::LimitTooLarge);
    }
    Ok(())
}

/// The distinct indexed values of attribute `name` with type `tag`, in
/// value order from `lower`, at most `limit` of them.
///
/// Positional and lazy: the seek is charged once, then each returned term
/// is charged `{TermVisit 1, ReadByte value bytes}` after the step that
/// reveals it and before it is decoded; an `Excluded` lower bound equal to
/// an existing term is stepped over without charge, as is the key that ends
/// the run.
pub(crate) fn terms(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    name: &CellName,
    tag: TypeTag,
    lower: &Bound<Value>,
    limit: usize,
    meter: &mut Meter<'_>,
) -> Result<Vec<Value>, Fault> {
    meter.charge(OperationClass::TermSeek, 1)?;
    let (start, skip_start) = match lower {
        Bound::Unbounded => (TermKey::run_start(name, tag, &[]), false),
        Bound::Included(bound) => (
            TermKey::run_start(name, tag, &bound.term_bytes().expect("indexable")),
            false,
        ),
        Bound::Excluded(bound) => (
            TermKey::run_start(name, tag, &bound.term_bytes().expect("indexable")),
            true,
        ),
    };
    let mut run = state.terms.range_from(&start, &loaders.terms);
    let mut values = Vec::new();
    let outcome = loop {
        if values.len() == limit {
            break Ok(());
        }
        let (term, _) = match run.next() {
            None => break Ok(()),
            Some(Err(error)) => break Err(Fault::from(error)),
            Some(Ok(entry)) => entry,
        };
        if term.name != *name || term.tag != tag {
            break Ok(());
        }
        let Some(bytes) = &term.value else {
            break Ok(());
        };
        if skip_start && term == start {
            continue;
        }
        if let Err(reason) = meter.charge_group(&[
            (OperationClass::TermVisit, 1),
            (OperationClass::ReadByte, units(bytes.len())),
        ]) {
            break Err(reason.into());
        }
        values.push(Value::from_term_bytes(tag, bytes).expect("decoded term keys hold term bytes"));
    };
    meter.note_index_nodes(run.probe().nodes);
    outcome.map(|()| values)
}

/// Reads one record by key.
pub(crate) fn get_record(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    key: &RecordKey,
    projection: &Projection,
    meter: &mut Meter<'_>,
) -> Result<Option<Row>, Fault> {
    let Some(id) = lookup_key(state, loaders, key, meter)? else {
        return Ok(None);
    };
    let row = load_row(state, loaders, id, Some(key), meter)?;
    Ok(emit(id, &row, projection, meter).map(Some)?)
}

/// Whether `term` holds for the cells of one record.
fn term_matches(term: &Term, cells: &[(CellName, Cell)]) -> bool {
    let Some(cell) = find(cells, term.name()) else {
        return false;
    };
    if !cell.is_attribute() {
        return false;
    }
    let held = &cell.value;
    match term {
        Term::Eq { value, .. } => held.tag() == value.tag() && held == value,
        Term::NotEq { value, .. } => held.tag() == value.tag() && held != value,
        Term::Range { lower, upper, .. } => {
            let tag = range_tag(lower, upper);
            held.tag() == tag && within(held, lower, upper)
        }
        Term::StartsWith { prefix, .. } => match held {
            Value::Str(text) => text.as_bytes().starts_with(prefix.as_bytes()),
            _ => false,
        },
        Term::Exists { tag, .. } => held.tag() == *tag,
    }
}

fn within(value: &Value, lower: &Bound<Value>, upper: &Bound<Value>) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let above_lower = match lower {
        Bound::Unbounded => true,
        Bound::Included(bound) => matches!(value.cmp_ordered(bound), Some(Greater | Equal)),
        Bound::Excluded(bound) => matches!(value.cmp_ordered(bound), Some(Greater)),
    };
    let below_upper = match upper {
        Bound::Unbounded => true,
        Bound::Included(bound) => matches!(value.cmp_ordered(bound), Some(Less | Equal)),
        Bound::Excluded(bound) => matches!(value.cmp_ordered(bound), Some(Less)),
    };
    above_lower && below_upper
}

fn literal_matches(literal: &Literal, cells: &[(CellName, Cell)]) -> bool {
    term_matches(&literal.term, cells) != literal.negated
}

/// A candidate yielded by a [`Source`]: its id and, when the source walked
/// the rows, the row itself.
type Candidate = (RecordId, Option<StoredRecord>);

/// An owned, lazy iterator over one posting map.
type PostingRange<'a> = Range<'a, RecordId, Member, Loader<'a>>;

/// A lazy, id-ordered candidate source of one AND-group's first literal,
/// positioned after the cursor. Every entry is charged after the node step
/// that reveals it and before it is used; entries that a merge has looked
/// at but not yet yielded are not charged. The iterators own the nodes they
/// walk and borrow only the namespace's loaders.
enum Source<'a> {
    /// One posting (Eq, Exists, or the existence posting of NotEq).
    Posting(PostingRange<'a>),
    /// The postings of a Range/StartsWith run, merged by id.
    Run {
        heads: BinaryHeap<Reverse<(RecordId, usize)>>,
        postings: Vec<PostingRange<'a>>,
    },
    /// Every live row (match-all, or the domain of a negated literal).
    Rows(Range<'a, RecordId, StoredRecord, Loader<'a>>),
    /// Nothing: the cursor is past every possible id.
    Empty,
}

impl Source<'_> {
    fn next(&mut self, meter: &mut Meter<'_>) -> Result<Option<Candidate>, Fault> {
        match self {
            Self::Posting(range) => {
                let Some(entry) = range.next() else {
                    return Ok(None);
                };
                let (id, Member) = entry?;
                meter.charge(OperationClass::PostingEntry, 1)?;
                Ok(Some((id, None)))
            }
            Self::Run { heads, postings } => {
                let Some(Reverse((id, index))) = heads.pop() else {
                    return Ok(None);
                };
                meter.charge(OperationClass::PostingEntry, 1)?;
                if let Some(entry) = postings[index].next() {
                    let (next, Member) = entry?;
                    heads.push(Reverse((next, index)));
                }
                Ok(Some((id, None)))
            }
            Self::Rows(range) => {
                let Some(entry) = range.next() else {
                    return Ok(None);
                };
                let (id, row) = entry?;
                meter.charge(OperationClass::RowVisit, 1)?;
                Ok(Some((id, Some(row))))
            }
            Self::Empty => Ok(None),
        }
    }

    /// Physical node visits of the source's traversals so far.
    fn note_physical(&self, meter: &mut Meter<'_>) {
        match self {
            Self::Posting(range) => meter.note_index_nodes(range.probe().nodes),
            Self::Run { postings, .. } => {
                meter.note_index_nodes(postings.iter().map(|range| range.probe().nodes).sum());
            }
            Self::Rows(range) => meter.note_record_nodes(range.probe().nodes),
            Self::Empty => {}
        }
    }
}

/// Where a term run stops.
enum RunEnd<'q> {
    Range(&'q Bound<Value>),
    Prefix(&'q [u8]),
}

/// Opens every posting of the contiguous run of value terms of `(name, tag)`
/// that satisfy the predicate, in key order, each positioned at `from` (its
/// seek and first entry are stepped at once, to prime the merge). The seek
/// is charged once; each qualifying term is charged as it is visited; the
/// existence term, a stepped-over `Excluded` endpoint and the key that
/// terminates the run are never charged.
///
/// The term-map traversal is noted here on every path. The postings are
/// noted by the returned source's owner, or here when a `TermVisit` rejects:
/// no group will own the partial run, but the postings it had already
/// positioned were visited.
#[allow(clippy::too_many_arguments)]
fn open_run<'a>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    name: &CellName,
    tag: TypeTag,
    start: TermKey,
    skip_start: bool,
    end: RunEnd<'_>,
    from: RecordId,
    meter: &mut Meter<'_>,
) -> Result<Source<'a>, Fault> {
    meter.charge(OperationClass::TermSeek, 1)?;
    let mut run = state.terms.range_from(&start, &loaders.terms);
    let mut postings = Vec::new();
    let mut heads = BinaryHeap::new();
    let outcome = loop {
        let (term, posting) = match run.next() {
            None => break Ok(()),
            Some(Err(error)) => break Err(Fault::from(error)),
            Some(Ok(entry)) => entry,
        };
        if term.name != *name || term.tag != tag {
            break Ok(());
        }
        let Some(value) = &term.value else {
            break Ok(());
        };
        if skip_start && term == start {
            continue;
        }
        let inside = match end {
            RunEnd::Range(Bound::Unbounded) => true,
            RunEnd::Range(Bound::Included(bound)) => {
                value.as_ref() <= bound.term_bytes().expect("ordered").as_slice()
            }
            RunEnd::Range(Bound::Excluded(bound)) => {
                value.as_ref() < bound.term_bytes().expect("ordered").as_slice()
            }
            RunEnd::Prefix(prefix) => value.starts_with(prefix),
        };
        if !inside {
            break Ok(());
        }
        if let Err(reason) = meter.charge(OperationClass::TermVisit, 1) {
            break Err(reason.into());
        }
        let mut entries = posting.0.range_from(&from, &loaders.postings);
        let first = entries.next();
        let index = postings.len();
        postings.push(entries);
        match first {
            None => {}
            Some(Ok((id, Member))) => heads.push(Reverse((id, index))),
            Some(Err(error)) => break Err(error.into()),
        }
    };
    meter.note_index_nodes(run.probe().nodes);
    let source = Source::Run { heads, postings };
    if outcome.is_err() {
        source.note_physical(meter);
    }
    outcome.map(|()| source)
}

/// Opens one posting positioned at `from`; a missing term is an empty
/// source. The posting's seek happens at its first step.
fn open_posting<'a>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    term: &TermKey,
    from: RecordId,
    meter: &mut Meter<'_>,
) -> Result<Source<'a>, Fault> {
    meter.charge(OperationClass::TermSeek, 1)?;
    let mut probe = Probe::new();
    let posting = state.terms.get(term, &loaders.terms, &mut probe);
    meter.note_index_nodes(probe.nodes);
    Ok(match posting? {
        Some(posting) => Source::Posting(posting.0.range_from(&from, &loaders.postings)),
        None => Source::Empty,
    })
}

/// One AND-group as a lazy stream of its matches in id order.
struct GroupStream<'a, 'q> {
    source: Source<'a>,
    /// Literals probed against every candidate, in order: the first literal
    /// itself when its source is only a domain (NotEq, negation), then the
    /// rest of the group.
    probes: Vec<&'q Literal>,
    head: Option<RecordId>,
    /// The head's row when a probe or a row-map source already loaded it.
    head_row: Option<StoredRecord>,
}

impl<'a, 'q> GroupStream<'a, 'q> {
    /// Opens the group's source after the cursor, charging the group, the
    /// seek and the run terms.
    fn open(
        state: &TypedState,
        loaders: &'a NamespaceLoaders<'a>,
        group: &'q [Literal],
        from: Option<RecordId>,
        meter: &mut Meter<'_>,
    ) -> Result<Self, Fault> {
        meter.charge(OperationClass::QueryGroup, 1)?;
        let mut probes: Vec<&'q Literal> = Vec::new();
        let source = match (group.first(), from) {
            (_, None) => Source::Empty,
            (None, Some(from)) => Source::Rows(state.rows.range_from(&from, &loaders.rows)),
            (Some(first), Some(from)) if first.negated => {
                probes.push(first);
                Source::Rows(state.rows.range_from(&from, &loaders.rows))
            }
            (Some(first), Some(from)) => match &first.term {
                Term::Eq { name, value } => {
                    open_posting(state, loaders, &TermKey::value(name, value), from, meter)?
                }
                Term::Exists { name, tag } => {
                    open_posting(state, loaders, &TermKey::existence(name, *tag), from, meter)?
                }
                Term::NotEq { name, value } => {
                    probes.push(first);
                    let existence = TermKey::existence(name, value.tag());
                    open_posting(state, loaders, &existence, from, meter)?
                }
                Term::Range { name, lower, upper } => {
                    let tag = range_tag(lower, upper);
                    let (start, skip_start) = match lower {
                        Bound::Unbounded => (TermKey::run_start(name, tag, &[]), false),
                        Bound::Included(bound) => (
                            TermKey::run_start(name, tag, &bound.term_bytes().expect("ordered")),
                            false,
                        ),
                        Bound::Excluded(bound) => (
                            TermKey::run_start(name, tag, &bound.term_bytes().expect("ordered")),
                            true,
                        ),
                    };
                    open_run(
                        state,
                        loaders,
                        name,
                        tag,
                        start,
                        skip_start,
                        RunEnd::Range(upper),
                        from,
                        meter,
                    )?
                }
                Term::StartsWith { name, prefix } => open_run(
                    state,
                    loaders,
                    name,
                    TypeTag::Str,
                    TermKey::run_start(name, TypeTag::Str, prefix.as_bytes()),
                    false,
                    RunEnd::Prefix(prefix.as_bytes()),
                    from,
                    meter,
                )?,
            },
        };
        probes.extend(group.iter().skip(1));
        Ok(Self {
            source,
            probes,
            head: None,
            head_row: None,
        })
    }

    /// Advances to the group's next match: candidates come from the source
    /// in id order and each is probed by the group's literals in order until
    /// one fails (candidate-major).
    fn pull(
        &mut self,
        state: &TypedState,
        loaders: &NamespaceLoaders<'_>,
        meter: &mut Meter<'_>,
    ) -> Result<(), Fault> {
        self.head = None;
        self.head_row = None;
        loop {
            let Some((id, row)) = self.source.next(meter)? else {
                return Ok(());
            };
            let mut loaded = row;
            let mut matched = true;
            for literal in &self.probes {
                meter.charge(OperationClass::RecordProbe, 1)?;
                let record = match &mut loaded {
                    Some(record) => record,
                    empty => empty.insert(load_row(state, loaders, id, None, meter)?),
                };
                if !literal_matches(literal, &record.cells) {
                    matched = false;
                    break;
                }
            }
            if matched {
                self.head = Some(id);
                self.head_row = loaded;
                return Ok(());
            }
        }
    }
}

/// The merged, deduplicated stream of every group's matches.
///
/// The evaluator owns every group from the moment its source is open, also
/// when a later charge rejects, so [`Evaluator::note_physical`] sees every
/// opened source's traversal; see [`evaluate`].
struct Evaluator<'a, 'q> {
    groups: Vec<GroupStream<'a, 'q>>,
}

impl<'a, 'q> Evaluator<'a, 'q> {
    /// Opens every group in order, then pulls every group's first head in
    /// order. A group is kept as soon as it is open, so a rejection while
    /// opening a later group or pulling a head leaves the sources opened so
    /// far in the evaluator.
    fn open(
        &mut self,
        state: &TypedState,
        loaders: &'a NamespaceLoaders<'a>,
        query: &'q Query,
        after: Option<RecordId>,
        meter: &mut Meter<'_>,
    ) -> Result<(), Fault> {
        // The first id a source may yield; `None` when the cursor is past
        // every id (the sources are then empty).
        let from = match after {
            None => RecordId::from_ordinal(1),
            Some(after) => after
                .ordinal()
                .checked_add(1)
                .and_then(RecordId::from_ordinal),
        };
        for group in &query.groups {
            self.groups
                .push(GroupStream::open(state, loaders, group, from, meter)?);
        }
        for group in &mut self.groups {
            group.pull(state, loaders, meter)?;
        }
        Ok(())
    }

    /// The smallest head: the next match, if any.
    fn peek(&self) -> Option<RecordId> {
        self.groups.iter().filter_map(|group| group.head).min()
    }

    /// The row of head `id`, when some group already loaded it.
    fn head_row(&self, id: RecordId) -> Option<StoredRecord> {
        self.groups
            .iter()
            .filter(|group| group.head == Some(id))
            .find_map(|group| group.head_row.clone())
    }

    /// Advances every group whose head is `id`, in group order.
    fn advance_past(
        &mut self,
        state: &TypedState,
        loaders: &NamespaceLoaders<'_>,
        id: RecordId,
        meter: &mut Meter<'_>,
    ) -> Result<(), Fault> {
        for group in &mut self.groups {
            if group.head == Some(id) {
                group.pull(state, loaders, meter)?;
            }
        }
        Ok(())
    }

    /// Physical node visits of every opened source's traversal. Called once,
    /// after the last traversal step.
    fn note_physical(&self, meter: &mut Meter<'_>) {
        for group in &self.groups {
            group.source.note_physical(meter);
        }
    }
}

/// Opens the groups of `query` after `after`, runs `walk` over them and then
/// notes the physical visits of every source that was opened.
///
/// The note follows the walk on every path: success, and a rejection at any
/// charge point (a group's opening, a head pull, an advance or a row
/// emission). Each source the evaluator holds is noted here and nowhere
/// else. A group whose opening rejects never joins the evaluator; the only
/// partial source it can leave, a run stopped at a `TermVisit`, is noted by
/// [`open_run`]. Every visited node is therefore noted exactly once. Noting
/// is not metered, so charges are untouched. (A storage failure discards
/// the trace with the rest of the read.)
fn evaluate<'a, 'q, 'm, T>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    query: &'q Query,
    after: Option<RecordId>,
    meter: &mut Meter<'m>,
    walk: impl FnOnce(&mut Evaluator<'a, 'q>, &mut Meter<'m>) -> Result<T, Fault>,
) -> Result<T, Fault> {
    let mut evaluator = Evaluator {
        groups: Vec::with_capacity(query.groups.len()),
    };
    let outcome = evaluator
        .open(state, loaders, query, after, meter)
        .and_then(|()| walk(&mut evaluator, meter));
    evaluator.note_physical(meter);
    outcome
}

/// Evaluates `query` lazily after `after` and emits up to `limit` rows.
///
/// Returns the page rows and the id of the last row when a further match
/// exists. Every group is positioned at its first match beyond the page
/// when the page is full, so the terminal claim (`None`) is exact.
pub(crate) fn query<'a>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    query: &Query,
    projection: &Projection,
    limit: usize,
    after: Option<RecordId>,
    meter: &mut Meter<'_>,
) -> Result<(Vec<Row>, Option<RecordId>), Fault> {
    evaluate(state, loaders, query, after, meter, |evaluator, meter| {
        page(state, loaders, evaluator, projection, limit, meter)
    })
}

fn page(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    evaluator: &mut Evaluator<'_, '_>,
    projection: &Projection,
    limit: usize,
    meter: &mut Meter<'_>,
) -> Result<(Vec<Row>, Option<RecordId>), Fault> {
    let mut rows: Vec<Row> = Vec::new();
    while let Some(id) = evaluator.peek() {
        if rows.len() == limit {
            let last = rows.last().map(|row| row.id);
            return Ok((rows, last));
        }
        let row = match evaluator.head_row(id) {
            Some(row) => row,
            None => load_row(state, loaders, id, None, meter)?,
        };
        rows.push(emit(id, &row, projection, meter)?);
        evaluator.advance_past(state, loaders, id, meter)?;
    }
    Ok((rows, None))
}

/// Counts every match of `query`: the same enumeration as a page walk with
/// no row emitted.
pub(crate) fn count<'a>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    query: &Query,
    meter: &mut Meter<'_>,
) -> Result<u64, Fault> {
    evaluate(state, loaders, query, None, meter, |evaluator, meter| {
        let mut total = 0u64;
        while let Some(id) = evaluator.peek() {
            total += 1;
            evaluator.advance_past(state, loaders, id, meter)?;
        }
        Ok(total)
    })
}

/// A storage failure of a namespace-local walk outside metered execution.
pub(crate) fn storage(fault: Fault) -> StorageError {
    match fault {
        Fault::Storage(error) => error,
        Fault::Reject(reason) => unreachable!("an unmetered walk was rejected: {reason}"),
    }
}
