//! Planned evaluation of query and count reads: each AND-group is driven by
//! its cheapest index source instead of its first literal.
//!
//! A local read opts in with [`Budget::planned`](crate::Budget::planned);
//! every other read, block execution's expiry sweep included, keeps the
//! written evaluation and its canonical charges. A planned read returns
//! exactly what the written evaluation returns: the same rows in the same
//! id order, the same `next` (an exact claim) and the same counts. Every
//! source still yields ids in ascending order after the cursor, and a match
//! is still an id every literal of its group holds for, so only the work,
//! hence the charges and the point where a budget ends the read, differ.
//! The charges are a pure function of the state and the query: sizes come
//! from the posting entry counts stored in the term leaves, never from
//! timing, caches or storage layout.
//!
//! # Planning one group
//!
//! `QueryGroup` 1, then, before any term is read, a group that no record can
//! satisfy by the one-cell-per-name rule (two positive literals that need
//! different types of one name, two different `Eq` values of one name, `Eq`
//! with `NotEq` of the same value, a literal with its own negation, a
//! positive literal with `NOT Exists` of its name and type) is empty and
//! costs nothing more. Then each literal in written order:
//!
//! - positive `Eq`/`Exists`: `TermSeek` 1 reads its term leaf; a missing
//!   term empties the group (planning stops there). Its posting is a
//!   *positive posting* of known size.
//! - positive `NotEq`: `TermSeek` 1 for the existence term of its type (a
//!   positive posting, the literal's *domain*; missing empties the group),
//!   then `TermSeek` 1 for its value term, an *absent posting* when present.
//! - negated `Eq`/`Exists`: `TermSeek` 1; the term, when present, is an
//!   absent posting. One that holds every live record empties the group.
//! - positive `Range`/`StartsWith`: a *run*. `Range` literals on one
//!   `(name, type)` are one run over the intersection of their bounds.
//! - anything else (negated `NotEq`, `Range`, `StartsWith`) is a *row
//!   probe*.
//!
//! Each run is then walked as `open_run` walks it, `TermSeek` 1 and
//! `TermVisit` 1 per qualifying term, summing its postings' sizes (postings
//! of distinct values of one name are disjoint, so the sum is the run's
//! exact match count). A walk stops, abandoning the run to a row probe,
//! once its cost (matches plus terms) reaches the cheapest source found so
//! far, or after [`RUN_WALK_CAP`] terms while another source competes. A
//! page read with no positive posting compares runs with the smallest
//! domain, or the live row count when there is none; a count with no
//! positive posting walks its first run completely. A completed empty run
//! empties the group.
//!
//! The driver is the cheapest of the positive postings (by size) and the
//! completed runs (by matches plus terms, since a run is re-walked on every
//! page); ties go to a posting, then to the earlier literal. A group with
//! neither is driven by its smallest `NotEq` domain, else by the row map.
//! `NotEq` and negated literals therefore never drive while a positive
//! literal can. Every other positive posting (each term once) becomes a
//! *follower*, smallest first, except one holding every live record, which
//! every candidate passes. Absent postings are probed largest first, then
//! the row probes in written order. Other completed runs become row probes.
//!
//! # Evaluation
//!
//! The driver yields candidates in id order as before (`PostingEntry` 1 per
//! posting or run entry, `RowVisit` 1 per row). Each follower is moved to
//! the candidate: a follower at most [`DENSE_RATIO`] times the driver's
//! size first steps one entry forward (`PostingEntry` 1), and otherwise, or
//! when that falls short, it repositions from its root (`RecordProbe` 1, a
//! membership seek, which covers the descent and the first entry at or
//! after the candidate). A follower with no entry left ends the group; one
//! whose next entry lies beyond the candidate moves the driver to that
//! entry (a leapfrog): a driving posting steps [`DRIVER_GALLOP`] entry and
//! then repositions, any other driver steps. An absent posting is probed
//! by id (`RecordProbe` 1). A row probe loads the candidate's row once and
//! charges `RecordProbe` 1 per literal, as written probes do. A candidate
//! passing everything is the group's head; pages and the union over groups
//! are unchanged.
//!
//! # Counts
//!
//! A count whose groups are pairwise disjoint (by the rule above applied to
//! the union of two groups) is the sum of its groups' counts, so a union of
//! distinct values of one name never deduplicates; otherwise the planned
//! groups are merged and every distinct id is counted once, as written.
//! A group whose plan leaves only its driver is answered from sizes: the
//! posting's entry count, the run's summed matches, or the live row count,
//! less the sizes of its absent postings when they are pairwise disjoint and
//! each inside the driver (the row map, or a value term of the driver's
//! existence term). Every other group is enumerated with no row loaded
//! unless a row probe needs it.

use std::{cmp::Reverse, collections::BinaryHeap, ops::Bound, sync::Arc};

use arkiv_db_trie::Probe;

use super::{Evaluation, GroupStream, PostingRange, Source, TypedState, enumerate, range_tag};
use crate::{
    layout::{Member, PostingMap, TermKey},
    meter::{Meter, OperationClass},
    nodes::{Fault, Loader, NamespaceLoaders},
    query::{Literal, Query, Term},
    record::{CellName, RecordId, TypeTag, Value},
};

/// Most value terms one run's estimate visits while another source
/// competes with it. Bounds the planning work of a wide `Range` or a short
/// prefix to a constant per page.
pub(crate) const RUN_WALK_CAP: u64 = 65_536;

/// Entries a driving posting steps forward, when a follower moved the
/// floor, before it repositions from its root instead. A step and a
/// repositioning cost one unit each under the live schedule, so one step
/// first only pays when the floor is the next entry, and is physically
/// cheaper then.
const DRIVER_GALLOP: usize = 1;

/// A follower is dense when it holds at most this many times the driver's
/// matches: it then steps one entry forward before it repositions, since
/// its next entry is often the candidate. A sparser one repositions at
/// once.
const DENSE_RATIO: u64 = 2;

/// Entries a follower of `size` steps before it repositions, under a driver
/// of `matches`.
fn follower_gallop(size: u64, matches: u64) -> usize {
    usize::from(matches != 0 && size <= matches.saturating_mul(DENSE_RATIO))
}

// ---------------------------------------------------------------------------
// Posting cursors and membership
// ---------------------------------------------------------------------------

/// A forward-only position in one posting: a planned group's driver or one
/// of its followers.
pub(crate) struct PostingCursor<'a> {
    posting: PostingMap,
    loader: &'a Loader<'a>,
    /// `None` until a follower's first seek positions it.
    range: Option<PostingRange<'a>>,
    /// The entry the cursor stands on.
    head: Option<RecordId>,
    /// No entry is left at or after the last position.
    ended: bool,
    /// Entries [`Self::seek`] steps before it repositions.
    gallop: usize,
    /// Node visits of the ranges a repositioning replaced.
    retired: u64,
}

impl<'a> PostingCursor<'a> {
    /// A driving posting positioned at `from`; its first step reads the
    /// first entry at or after it.
    fn driving(posting: PostingMap, loader: &'a Loader<'a>, from: RecordId) -> Self {
        let range = posting.range_from(&from, loader);
        Self {
            posting,
            loader,
            range: Some(range),
            head: None,
            ended: false,
            gallop: DRIVER_GALLOP,
            retired: 0,
        }
    }

    /// A follower, positioned by its first seek.
    fn following(posting: PostingMap, loader: &'a Loader<'a>, gallop: usize) -> Self {
        Self {
            posting,
            loader,
            range: None,
            head: None,
            ended: false,
            gallop,
            retired: 0,
        }
    }

    /// Steps to the next entry, charging `PostingEntry` 1 after the node
    /// step that reveals it.
    pub(crate) fn step(&mut self, meter: &mut Meter<'_>) -> Result<Option<RecordId>, Fault> {
        if self.ended {
            return Ok(None);
        }
        let range = self
            .range
            .as_mut()
            .expect("only a positioned cursor steps: a follower is sought first");
        match range.next() {
            None => {
                self.ended = true;
                self.head = None;
                Ok(None)
            }
            Some(entry) => {
                let (id, Member) = entry?;
                meter.charge(OperationClass::PostingEntry, 1)?;
                self.head = Some(id);
                Ok(Some(id))
            }
        }
    }

    /// The first entry at or after `target`, moving forward only: the
    /// current entry when it already is, else up to the gallop's steps,
    /// else one repositioning from the posting's root (`RecordProbe` 1,
    /// covering the descent and the entry it lands on). Targets of one
    /// cursor never decrease.
    pub(crate) fn seek(
        &mut self,
        target: RecordId,
        meter: &mut Meter<'_>,
    ) -> Result<Option<RecordId>, Fault> {
        if self.ended {
            return Ok(None);
        }
        if let Some(head) = self.head
            && head >= target
        {
            return Ok(Some(head));
        }
        if self.range.is_some() {
            for _ in 0..self.gallop {
                match self.step(meter)? {
                    None => return Ok(None),
                    Some(id) if id >= target => return Ok(Some(id)),
                    Some(_) => {}
                }
            }
        }
        self.reposition(target, meter)
    }

    /// Repositions at `target` from the posting's root: `RecordProbe` 1,
    /// charged first, covers the descent and the entry it lands on.
    fn reposition(
        &mut self,
        target: RecordId,
        meter: &mut Meter<'_>,
    ) -> Result<Option<RecordId>, Fault> {
        meter.charge(OperationClass::RecordProbe, 1)?;
        if let Some(range) = self.range.take() {
            self.retired += range.probe().nodes;
        }
        let mut range = self.posting.range_from(&target, self.loader);
        let first = range.next();
        self.range = Some(range);
        match first {
            None => {
                self.ended = true;
                self.head = None;
                Ok(None)
            }
            Some(entry) => {
                let (id, Member) = entry?;
                self.head = Some(id);
                Ok(Some(id))
            }
        }
    }

    /// Physical node visits of the cursor's traversals so far.
    pub(crate) fn note_physical(&self, meter: &mut Meter<'_>) {
        let current = self.range.as_ref().map_or(0, |range| range.probe().nodes);
        meter.note_index_nodes(self.retired + current);
    }
}

/// Whether `id` is in `posting`: one membership lookup, whose charge the
/// caller takes first.
pub(crate) fn holds(
    posting: &PostingMap,
    loaders: &NamespaceLoaders<'_>,
    id: RecordId,
    meter: &mut Meter<'_>,
) -> Result<bool, Fault> {
    let mut probe = Probe::new();
    let found = posting.get(&id, &loaders.postings, &mut probe);
    meter.note_index_nodes(probe.nodes);
    Ok(found?.is_some())
}

// ---------------------------------------------------------------------------
// Static analysis
// ---------------------------------------------------------------------------

/// What a positive literal needs of its name: an attribute of `tag`, with
/// `value` for `Eq`.
struct Need<'q> {
    name: &'q CellName,
    tag: TypeTag,
    value: Option<&'q Value>,
    /// The value a positive `NotEq` excludes.
    excluded: Option<&'q Value>,
}

fn need(literal: &Literal) -> Option<Need<'_>> {
    if literal.negated {
        return None;
    }
    let (name, tag, value, excluded) = match &literal.term {
        Term::Eq { name, value } => (name, value.tag(), Some(value), None),
        Term::NotEq { name, value } => (name, value.tag(), None, Some(value)),
        Term::Exists { name, tag } => (name, *tag, None, None),
        Term::Range { name, lower, upper } => (name, range_tag(lower, upper), None, None),
        Term::StartsWith { name, .. } => (name, TypeTag::Str, None, None),
    };
    Some(Need {
        name,
        tag,
        value,
        excluded,
    })
}

/// Whether no record can hold every literal at once. Sound, not complete:
/// a record has at most one cell per name, so positive literals needing
/// different types of one name, two different `Eq` values of one name, or
/// `Eq` and `NotEq` of one value cannot all hold, and neither can a literal
/// and its negation, nor a positive literal and `NOT Exists` of its name and
/// type.
fn contradicts(literals: &[&Literal]) -> bool {
    let needs: Vec<Need<'_>> = literals
        .iter()
        .filter_map(|literal| need(literal))
        .collect();
    for (index, left) in needs.iter().enumerate() {
        for right in &needs[index + 1..] {
            if left.name != right.name {
                continue;
            }
            if left.tag != right.tag {
                return true;
            }
            if let (Some(a), Some(b)) = (left.value, right.value)
                && a != b
            {
                return true;
            }
            let excludes = |value: Option<&Value>, excluded: Option<&Value>| {
                value.is_some() && value == excluded
            };
            if excludes(left.value, right.excluded) || excludes(right.value, left.excluded) {
                return true;
            }
        }
    }
    literals
        .iter()
        .filter(|literal| literal.negated)
        .any(|negated| {
            literals
                .iter()
                .any(|other| !other.negated && other.term == negated.term)
                || match &negated.term {
                    Term::Exists { name, tag } => needs
                        .iter()
                        .any(|need| need.name == name && need.tag == *tag),
                    _ => false,
                }
        })
}

/// Whether no record matches two of the groups at once.
fn pairwise_disjoint(groups: &[Vec<Literal>]) -> bool {
    groups.iter().enumerate().all(|(index, left)| {
        groups[index + 1..].iter().all(|right| {
            let both: Vec<&Literal> = left.iter().chain(right).collect();
            contradicts(&both)
        })
    })
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// What a plan is for: a run competes with the row map on a page, but a
/// count walks a lone run completely (its matches are the count).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Page,
    Count,
}

/// A posting found by a term lookup, with its written position.
struct Found {
    term: TermKey,
    posting: PostingMap,
    order: usize,
}

impl Found {
    fn len(&self) -> u64 {
        self.posting.len()
    }
}

/// The run of one or more positive `Range`/`StartsWith` literals.
struct RunSpec<'q> {
    name: CellName,
    tag: TypeTag,
    shape: Shape,
    literals: Vec<(usize, &'q Literal)>,
}

enum Shape {
    Range {
        lower: Bound<Value>,
        upper: Bound<Value>,
    },
    Prefix(Arc<str>),
}

/// A completely walked run.
struct Walked {
    postings: Vec<PostingMap>,
    matches: u64,
    terms: u64,
}

enum Driver {
    /// No live record matches.
    Empty,
    Posting(Found),
    Run(Walked),
    Rows,
}

struct Plan<'q> {
    driver: Driver,
    followers: Vec<Found>,
    absent: Vec<Found>,
    probes: Vec<&'q Literal>,
}

impl Plan<'_> {
    const fn empty() -> Self {
        Self {
            driver: Driver::Empty,
            followers: Vec::new(),
            absent: Vec::new(),
            probes: Vec::new(),
        }
    }
}

/// Reads one term leaf (`TermSeek` 1): its posting, if the term exists.
fn lookup(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    term: &TermKey,
    meter: &mut Meter<'_>,
) -> Result<Option<PostingMap>, Fault> {
    meter.charge(OperationClass::TermSeek, 1)?;
    let mut probe = Probe::new();
    let posting = state.terms.get(term, &loaders.terms, &mut probe);
    meter.note_index_nodes(probe.nodes);
    Ok(posting?.map(|posting| posting.0))
}

/// The tighter of two lower bounds of one ordered type.
fn tighter_lower(left: Bound<Value>, right: Bound<Value>) -> Bound<Value> {
    use std::cmp::Ordering::{Greater, Less};
    match (&left, &right) {
        (Bound::Unbounded, _) => right,
        (_, Bound::Unbounded) => left,
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            match a.cmp_ordered(b) {
                Some(Greater) => left,
                Some(Less) => right,
                _ if matches!(left, Bound::Excluded(_)) => left,
                _ => right,
            }
        }
    }
}

/// The tighter of two upper bounds of one ordered type.
fn tighter_upper(left: Bound<Value>, right: Bound<Value>) -> Bound<Value> {
    use std::cmp::Ordering::{Greater, Less};
    match (&left, &right) {
        (Bound::Unbounded, _) => right,
        (_, Bound::Unbounded) => left,
        (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
            match a.cmp_ordered(b) {
                Some(Less) => left,
                Some(Greater) => right,
                _ if matches!(left, Bound::Excluded(_)) => left,
                _ => right,
            }
        }
    }
}

/// Adds a positive `Range` or `StartsWith` literal to the group's runs:
/// a `Range` joins an earlier `Range` run of its `(name, type)`.
fn add_run<'q>(runs: &mut Vec<RunSpec<'q>>, order: usize, literal: &'q Literal) {
    match &literal.term {
        Term::Range { name, lower, upper } => {
            let tag = range_tag(lower, upper);
            let joined = runs.iter_mut().find(|run| {
                run.name == *name && run.tag == tag && matches!(run.shape, Shape::Range { .. })
            });
            match joined {
                Some(run) => {
                    if let Shape::Range {
                        lower: run_lower,
                        upper: run_upper,
                    } = &mut run.shape
                    {
                        *run_lower = tighter_lower(run_lower.clone(), lower.clone());
                        *run_upper = tighter_upper(run_upper.clone(), upper.clone());
                    }
                    run.literals.push((order, literal));
                }
                None => runs.push(RunSpec {
                    name: name.clone(),
                    tag,
                    shape: Shape::Range {
                        lower: lower.clone(),
                        upper: upper.clone(),
                    },
                    literals: vec![(order, literal)],
                }),
            }
        }
        Term::StartsWith { name, prefix } => runs.push(RunSpec {
            name: name.clone(),
            tag: TypeTag::Str,
            shape: Shape::Prefix(Arc::clone(prefix)),
            literals: vec![(order, literal)],
        }),
        _ => unreachable!("only ranges and prefixes are runs"),
    }
}

/// Walks a run's qualifying value terms in key order as `open_run` does
/// (`TermSeek` 1, then `TermVisit` 1 per qualifying term), collecting
/// their postings. `None` when the walk stops early: `cap` terms were
/// visited, or the matches plus terms reached `limit`.
fn walk_run(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    run: &RunSpec<'_>,
    limit: u64,
    cap: u64,
    meter: &mut Meter<'_>,
) -> Result<Option<Walked>, Fault> {
    meter.charge(OperationClass::TermSeek, 1)?;
    let (start, skip_start) = match &run.shape {
        Shape::Range { lower, .. } => match lower {
            Bound::Unbounded => (TermKey::run_start(&run.name, run.tag, &[]), false),
            Bound::Included(bound) => (
                TermKey::run_start(&run.name, run.tag, &bound.term_bytes().expect("ordered")),
                false,
            ),
            Bound::Excluded(bound) => (
                TermKey::run_start(&run.name, run.tag, &bound.term_bytes().expect("ordered")),
                true,
            ),
        },
        Shape::Prefix(prefix) => (
            TermKey::run_start(&run.name, TypeTag::Str, prefix.as_bytes()),
            false,
        ),
    };
    let end = match &run.shape {
        Shape::Range { upper, .. } => match upper {
            Bound::Unbounded => None,
            Bound::Included(bound) => Some((bound.term_bytes().expect("ordered"), true)),
            Bound::Excluded(bound) => Some((bound.term_bytes().expect("ordered"), false)),
        },
        Shape::Prefix(_) => None,
    };
    let mut terms_run = state.terms.range_from(&start, &loaders.terms);
    let mut walked = Walked {
        postings: Vec::new(),
        matches: 0,
        terms: 0,
    };
    let outcome = loop {
        let (term, posting) = match terms_run.next() {
            None => break Ok(true),
            Some(Err(error)) => break Err(Fault::from(error)),
            Some(Ok(entry)) => entry,
        };
        if term.name != run.name || term.tag != run.tag {
            break Ok(true);
        }
        let Some(value) = &term.value else {
            break Ok(true);
        };
        if skip_start && term == start {
            continue;
        }
        let inside = match (&run.shape, &end) {
            (Shape::Prefix(prefix), _) => value.starts_with(prefix.as_bytes()),
            (Shape::Range { .. }, None) => true,
            (Shape::Range { .. }, Some((bound, true))) => value.as_ref() <= bound.as_slice(),
            (Shape::Range { .. }, Some((bound, false))) => value.as_ref() < bound.as_slice(),
        };
        if !inside {
            break Ok(true);
        }
        if walked.terms == cap {
            break Ok(false);
        }
        if let Err(reason) = meter.charge(OperationClass::TermVisit, 1) {
            break Err(reason.into());
        }
        walked.terms += 1;
        walked.matches += posting.0.len();
        walked.postings.push(posting.0);
        if walked.matches.saturating_add(walked.terms) >= limit {
            break Ok(false);
        }
    };
    meter.note_index_nodes(terms_run.probe().nodes);
    Ok(outcome?.then_some(walked))
}

/// Plans one group (after its `QueryGroup` charge); see the module docs.
fn plan<'q>(
    state: &TypedState,
    loaders: &NamespaceLoaders<'_>,
    group: &'q [Literal],
    purpose: Purpose,
    meter: &mut Meter<'_>,
) -> Result<Plan<'q>, Fault> {
    let literals: Vec<&Literal> = group.iter().collect();
    if contradicts(&literals) {
        return Ok(Plan::empty());
    }
    let live = state.rows.len();
    let mut positives: Vec<Found> = Vec::new();
    let mut domains: Vec<Found> = Vec::new();
    let mut absent: Vec<Found> = Vec::new();
    let mut runs: Vec<RunSpec<'q>> = Vec::new();
    let mut probes: Vec<(usize, &'q Literal)> = Vec::new();
    for (order, literal) in group.iter().enumerate() {
        match (&literal.term, literal.negated) {
            (Term::Eq { name, value }, false) => {
                let term = TermKey::value(name, value);
                let Some(posting) = lookup(state, loaders, &term, meter)? else {
                    return Ok(Plan::empty());
                };
                positives.push(Found {
                    term,
                    posting,
                    order,
                });
            }
            (Term::Exists { name, tag }, false) => {
                let term = TermKey::existence(name, *tag);
                let Some(posting) = lookup(state, loaders, &term, meter)? else {
                    return Ok(Plan::empty());
                };
                positives.push(Found {
                    term,
                    posting,
                    order,
                });
            }
            (Term::NotEq { name, value }, false) => {
                let term = TermKey::existence(name, value.tag());
                let Some(posting) = lookup(state, loaders, &term, meter)? else {
                    return Ok(Plan::empty());
                };
                domains.push(Found {
                    term,
                    posting,
                    order,
                });
                let term = TermKey::value(name, value);
                if let Some(posting) = lookup(state, loaders, &term, meter)? {
                    absent.push(Found {
                        term,
                        posting,
                        order,
                    });
                }
            }
            (Term::Eq { name, value }, true) => {
                let term = TermKey::value(name, value);
                if let Some(posting) = lookup(state, loaders, &term, meter)? {
                    absent.push(Found {
                        term,
                        posting,
                        order,
                    });
                }
            }
            (Term::Exists { name, tag }, true) => {
                let term = TermKey::existence(name, *tag);
                if let Some(posting) = lookup(state, loaders, &term, meter)? {
                    absent.push(Found {
                        term,
                        posting,
                        order,
                    });
                }
            }
            (Term::Range { .. } | Term::StartsWith { .. }, false) => {
                add_run(&mut runs, order, literal);
            }
            _ => probes.push((order, literal)),
        }
    }
    // A negated literal whose posting holds every live record excludes all.
    if absent.iter().any(|found| found.len() == live) {
        return Ok(Plan::empty());
    }
    // The cheapest positive posting, then each run against the cheapest
    // source so far: a run completes only below its bound, so the last run
    // that completes is cheaper than every source before it.
    let mut best = positives.iter().map(Found::len).min();
    let mut chosen: Option<(usize, Walked)> = None;
    for (index, run) in runs.iter().enumerate() {
        let (limit, cap) = match (best, purpose) {
            (Some(cost), _) => (cost, RUN_WALK_CAP),
            (None, Purpose::Count) => (u64::MAX, u64::MAX),
            (None, Purpose::Page) => (
                domains
                    .iter()
                    .map(Found::len)
                    .min()
                    .unwrap_or(live)
                    .min(live),
                RUN_WALK_CAP,
            ),
        };
        match walk_run(state, loaders, run, limit, cap, meter)? {
            Some(walked) if walked.terms == 0 => return Ok(Plan::empty()),
            Some(walked) => {
                best = Some(walked.matches + walked.terms);
                chosen = Some((index, walked));
            }
            None => {}
        }
    }
    for (index, run) in runs.iter().enumerate() {
        if chosen.as_ref().is_none_or(|(chosen, _)| *chosen != index) {
            probes.extend(run.literals.iter().copied());
        }
    }
    probes.sort_by_key(|(order, _)| *order);
    let probes = probes.into_iter().map(|(_, literal)| literal).collect();
    // Each positive term once, then the driver.
    let mut seen: Vec<TermKey> = Vec::new();
    positives.retain(|found| {
        let fresh = !seen.contains(&found.term);
        seen.push(found.term.clone());
        fresh
    });
    domains.retain(|found| {
        let fresh = !seen.contains(&found.term);
        seen.push(found.term.clone());
        fresh
    });
    let take_smallest = |found: &mut Vec<Found>| {
        let index = found
            .iter()
            .enumerate()
            .min_by_key(|(_, found)| (found.len(), found.order))
            .map(|(index, _)| index)?;
        Some(found.remove(index))
    };
    let driver = if let Some((_, walked)) = chosen {
        Driver::Run(walked)
    } else if let Some(found) = take_smallest(&mut positives) {
        Driver::Posting(found)
    } else if let Some(found) = take_smallest(&mut domains) {
        Driver::Posting(found)
    } else {
        Driver::Rows
    };
    let mut followers: Vec<Found> = positives
        .into_iter()
        .chain(domains)
        .filter(|found| found.len() != live)
        .collect();
    followers.sort_by_key(|found| (found.len(), found.order));
    let mut seen: Vec<TermKey> = Vec::new();
    absent.retain(|found| {
        let fresh = !seen.contains(&found.term);
        seen.push(found.term.clone());
        fresh
    });
    absent.sort_by_key(|found| (Reverse(found.len()), found.order));
    Ok(Plan {
        driver,
        followers,
        absent,
        probes,
    })
}

/// The number of matches a plan's sizes already decide: an empty plan, or
/// one left with its driver alone, less absent postings that are pairwise
/// disjoint and each inside the driver. `None` when the group must be
/// enumerated.
fn closed_count(plan: &Plan<'_>, live: u64) -> Option<u64> {
    if !plan.followers.is_empty() || !plan.probes.is_empty() {
        return None;
    }
    let (size, inside): (u64, &dyn Fn(&TermKey) -> bool) = match &plan.driver {
        Driver::Empty => return Some(0),
        Driver::Posting(found) => (found.len(), &|term: &TermKey| {
            *term == found.term
                || (found.term.value.is_none()
                    && term.name == found.term.name
                    && term.tag == found.term.tag)
        }),
        Driver::Run(walked) => (walked.matches, &|_: &TermKey| false),
        Driver::Rows => (live, &|_: &TermKey| true),
    };
    if plan.absent.is_empty() {
        return Some(size);
    }
    let disjoint = plan.absent.iter().enumerate().all(|(index, left)| {
        plan.absent[index + 1..].iter().all(|right| {
            left.term.name == right.term.name
                && (left.term.tag != right.term.tag
                    || (left.term.value.is_some()
                        && right.term.value.is_some()
                        && left.term.value != right.term.value))
        })
    });
    if !disjoint || !plan.absent.iter().all(|found| inside(&found.term)) {
        return None;
    }
    plan.absent
        .iter()
        .try_fold(size, |left, found| left.checked_sub(found.len()))
}

// ---------------------------------------------------------------------------
// Opening groups and counting
// ---------------------------------------------------------------------------

/// Positions a completely walked run at `from`, as `open_run` does: each
/// posting's seek and first entry are stepped at once to prime the merge.
/// On a storage failure the postings positioned so far are noted here, since
/// no group will own them.
fn position_run<'a>(
    walked: Walked,
    loaders: &'a NamespaceLoaders<'a>,
    from: RecordId,
    meter: &mut Meter<'_>,
) -> Result<Source<'a>, Fault> {
    let mut postings = Vec::with_capacity(walked.postings.len());
    let mut heads = BinaryHeap::new();
    for posting in walked.postings {
        let mut entries = posting.range_from(&from, &loaders.postings);
        let first = entries.next();
        let index = postings.len();
        postings.push(entries);
        match first {
            None => {}
            Some(Ok((id, Member))) => heads.push(Reverse((id, index))),
            Some(Err(error)) => {
                Source::Run { heads, postings }.note_physical(meter);
                return Err(error.into());
            }
        }
    }
    Ok(Source::Run { heads, postings })
}

/// A planned group's stream positioned at `from`.
fn stream<'a, 'q>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    plan: Plan<'q>,
    from: RecordId,
    meter: &mut Meter<'_>,
) -> Result<GroupStream<'a, 'q>, Fault> {
    let live = state.rows.len();
    let (source, matches) = match plan.driver {
        Driver::Empty => (Source::Empty, 0),
        Driver::Posting(found) => {
            let matches = found.len();
            (
                Source::Cursor(PostingCursor::driving(
                    found.posting,
                    &loaders.postings,
                    from,
                )),
                matches,
            )
        }
        Driver::Run(walked) => {
            let matches = walked.matches;
            (position_run(walked, loaders, from, meter)?, matches)
        }
        Driver::Rows => (
            Source::Rows(state.rows.range_from(&from, &loaders.rows)),
            live,
        ),
    };
    let followers = plan
        .followers
        .into_iter()
        .map(|found| {
            let gallop = follower_gallop(found.len(), matches);
            PostingCursor::following(found.posting, &loaders.postings, gallop)
        })
        .collect();
    Ok(GroupStream {
        source,
        followers,
        absent: plan.absent.into_iter().map(|found| found.posting).collect(),
        probes: plan.probes,
        head: None,
        head_row: None,
    })
}

/// Opens one group for planned evaluation after the cursor: `QueryGroup` 1,
/// then the plan (see the module docs); a cursor past every id plans
/// nothing.
pub(crate) fn open_group<'a, 'q>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    group: &'q [Literal],
    from: Option<RecordId>,
    meter: &mut Meter<'_>,
) -> Result<GroupStream<'a, 'q>, Fault> {
    meter.charge(OperationClass::QueryGroup, 1)?;
    let Some(from) = from else {
        return Ok(GroupStream {
            source: Source::Empty,
            followers: Vec::new(),
            absent: Vec::new(),
            probes: Vec::new(),
            head: None,
            head_row: None,
        });
    };
    let plan = plan(state, loaders, group, Purpose::Page, meter)?;
    stream(state, loaders, plan, from, meter)
}

/// Counts every match of `query` by planned evaluation (see the module
/// docs): pairwise disjoint groups are summed, each from its sizes when its
/// plan decides them and by enumeration otherwise; overlapping groups are
/// merged and deduplicated.
pub(crate) fn count<'a>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    query: &Query,
    meter: &mut Meter<'_>,
) -> Result<u64, Fault> {
    if !pairwise_disjoint(&query.groups) {
        return enumerate(state, loaders, query, Evaluation::Planned, meter);
    }
    let mut total = 0u64;
    for group in &query.groups {
        total = total
            .checked_add(count_group(state, loaders, group, meter)?)
            .expect("disjoint groups count at most the live records");
    }
    Ok(total)
}

/// Counts one group's matches: from sizes when its plan decides them,
/// otherwise by enumerating its stream from the first id.
fn count_group<'a>(
    state: &TypedState,
    loaders: &'a NamespaceLoaders<'a>,
    group: &[Literal],
    meter: &mut Meter<'_>,
) -> Result<u64, Fault> {
    meter.charge(OperationClass::QueryGroup, 1)?;
    let plan = plan(state, loaders, group, Purpose::Count, meter)?;
    if let Some(count) = closed_count(&plan, state.rows.len()) {
        return Ok(count);
    }
    let first = RecordId::from_ordinal(1).expect("ordinals start at 1");
    let mut stream = stream(state, loaders, plan, first, meter)?;
    let mut walk = || {
        let mut total = 0u64;
        stream.pull(state, loaders, meter)?;
        while stream.head.is_some() {
            total += 1;
            stream.pull(state, loaders, meter)?;
        }
        Ok(total)
    };
    let outcome = walk();
    stream.note_physical(meter);
    outcome
}
