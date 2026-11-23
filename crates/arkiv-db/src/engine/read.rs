use std::{ops::Bound, sync::Arc};

use arkiv_db_storage::{Entry, versioned::StorageError};
use arkiv_db_trie::Probe;

use crate::{
    layout::NamespaceState,
    meter::{Budget, Meter, OperationClass, Outcome, PriceSchedule, ReceiptKind, Rejection},
    namespace::{NamespaceDescriptor, NamespaceId},
    nodes::{Fault, NamespaceLoaders, Nodes},
    query::{Cursor, CursorContext, Projection, Query, QueryPage},
    record::{CellName, Limits, RecordId, RecordKey, Row, TypeTag, Value},
    typed::{self, Evaluation},
};

use super::{
    WorkingState,
    ops::{Metered, ReadResult, Rejected},
    write::{raw_key, resolve, units, zero_cost},
};

/// Closes a read's meter: a value or a rejection with its receipt, or the
/// storage error that stopped it (no receipt).
fn finish_read<T>(
    meter: Meter<'_>,
    outcome: Result<T, Fault>,
) -> Result<ReadResult<T>, StorageError> {
    match outcome {
        Ok(value) => {
            let (receipt, physical) = meter.finish(ReceiptKind::Read, Outcome::Applied);
            Ok(Ok(Metered {
                value,
                receipt,
                physical,
            }))
        }
        Err(Fault::Reject(reason)) => {
            let (receipt, physical) = meter.finish(ReceiptKind::Read, Outcome::Rejected(reason));
            Ok(Err(Rejected {
                reason,
                receipt,
                physical,
            }))
        }
        Err(Fault::Storage(error)) => Err(error),
    }
}

/// One state as a metered read sees it: a snapshot's, or a branch's working
/// state with its pinned pricing.
#[derive(Clone, Copy)]
pub(super) struct View<'a> {
    pub(super) state: &'a WorkingState,
    pub(super) nodes: &'a Nodes,
    pub(super) pricing: &'a PriceSchedule,
    pub(super) limits: &'a Limits,
}

impl View<'_> {
    /// A rejection at admission, before any charge.
    fn refuse<T>(&self, budget: Budget, reason: Rejection) -> Result<ReadResult<T>, StorageError> {
        Ok(Err(zero_cost(
            self.pricing,
            budget,
            ReceiptKind::Read,
            reason,
        )))
    }

    /// Meters one read of `namespace`: the directory lookup, then `body` on
    /// the resolved namespace.
    fn read<T>(
        &self,
        namespace: NamespaceId,
        budget: Budget,
        body: impl FnOnce(NamespaceState, &NamespaceLoaders<'_>, &mut Meter<'_>) -> Result<T, Fault>,
    ) -> Result<ReadResult<T>, StorageError> {
        let mut meter = Meter::new(self.pricing, budget, None);
        let loaders = self.nodes.namespace(namespace);
        let outcome = meter
            .charge(OperationClass::DirectoryLookup, 1)
            .map_err(Fault::from)
            .and_then(|()| resolve(self.state, self.nodes, namespace, &mut meter))
            .and_then(|existing| body(existing, &loaders, &mut meter));
        finish_read(meter, outcome)
    }

    pub(super) fn get(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> Result<ReadResult<Option<Vec<u8>>>, StorageError> {
        if key.len() > Limits::MAX_RAW_KEY_LEN {
            let reason = Rejection::InvalidArgument(crate::ArgumentFault::KeyTooLong);
            return self.refuse(budget, reason);
        }
        self.read(namespace, budget, |existing, loaders, meter| {
            meter.charge(OperationClass::PointRead, 1)?;
            let mut probe = Probe::new();
            let found = existing
                .records
                .get(&raw_key(key), &loaders.records, &mut probe);
            meter.note_record_nodes(probe.nodes);
            let Some(value) = found? else {
                return Ok(None);
            };
            // The value's length is known only after the lookup; the bytes
            // are charged before they are copied out.
            meter.charge(OperationClass::ReadByte, units(value.0.len()))?;
            Ok(Some(value.0.to_vec()))
        })
    }

    pub(super) fn scan(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> Result<ReadResult<Vec<Entry>>, StorageError> {
        if prefix.len() > Limits::MAX_RAW_KEY_LEN {
            let reason = Rejection::InvalidArgument(crate::ArgumentFault::KeyTooLong);
            return self.refuse(budget, reason);
        }
        self.read(namespace, budget, |existing, loaders, meter| {
            meter.charge(OperationClass::ScanSeek, 1)?;
            // The encoding of the prefix sorts at or before every key that
            // extends it and after every smaller key, so it is the seek
            // position.
            let mut range = existing
                .records
                .range_from(&raw_key(prefix), &loaders.records);
            let mut entries = Vec::new();
            let outcome = loop {
                // Stepping to the next entry is the bounded step that reveals
                // its size; the entry is charged before it is materialized.
                // An abort at entry k has charged k - 1 entries; the nodes
                // stepped into to reveal entry k were visited but its entry
                // was neither charged nor returned. The physical count covers
                // every trie node the iterator stepped into (the seek path,
                // then each node once as the walk reaches it), so it is at
                // most the map's node count, not equal to the number of
                // entries.
                let (key, value) = match range.next() {
                    None => break Ok(()),
                    Some(Err(error)) => break Err(Fault::from(error)),
                    Some(Ok(entry)) => entry,
                };
                if !key.0.starts_with(prefix) {
                    break Ok(());
                }
                if let Err(reason) = meter.charge_group(&[
                    (OperationClass::ScanEntry, 1),
                    (OperationClass::ReadByte, units(key.0.len() + value.0.len())),
                ]) {
                    break Err(reason.into());
                }
                entries.push(Entry {
                    key: key.0.to_vec(),
                    value: value.0.to_vec(),
                });
            };
            meter.note_record_nodes(range.probe().nodes);
            outcome.map(|()| entries)
        })
    }

    pub(super) fn descriptor(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> Result<ReadResult<NamespaceDescriptor>, StorageError> {
        self.read(namespace, budget, |existing, _, _| Ok(existing.descriptor))
    }

    pub(super) fn record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> Result<ReadResult<Option<Row>>, StorageError> {
        if let Err(fault) = typed::validate_projection(self.limits, projection) {
            return self.refuse(budget, Rejection::InvalidQuery(fault));
        }
        self.read(namespace, budget, |existing, loaders, meter| {
            typed::get_record(&existing.typed, loaders, key, projection, meter)
        })
    }

    pub(super) fn query(
        &self,
        request: &QueryRequest<'_>,
        budget: Budget,
    ) -> Result<ReadResult<QueryPage>, StorageError> {
        let after = match request.validate(self.limits, budget) {
            Ok(after) => after,
            Err(reason) => return self.refuse(budget, reason),
        };
        self.read(request.namespace, budget, |existing, loaders, meter| {
            let (rows, next) = typed::query(
                &existing.typed,
                loaders,
                request.query,
                request.projection,
                request.limit,
                after,
                Evaluation::of(budget),
                meter,
            )?;
            Ok(QueryPage {
                rows,
                next: next.map(|after| request.continuation(after)),
            })
        })
    }

    pub(super) fn count(
        &self,
        namespace: NamespaceId,
        query: &Query,
        budget: Budget,
    ) -> Result<ReadResult<u64>, StorageError> {
        let admission = query_admission(self.limits, budget);
        if let Err(fault) = typed::validate_query(&admission, query, 1) {
            return self.refuse(budget, Rejection::InvalidQuery(fault));
        }
        self.read(namespace, budget, |existing, loaders, meter| {
            typed::count(
                &existing.typed,
                loaders,
                query,
                Evaluation::of(budget),
                meter,
            )
        })
    }

    pub(super) fn terms(
        &self,
        namespace: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: &Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> Result<ReadResult<Vec<Value>>, StorageError> {
        if let Err(fault) = typed::validate_terms(self.limits, tag, lower, limit) {
            return self.refuse(budget, Rejection::InvalidQuery(fault));
        }
        self.read(namespace, budget, |existing, loaders, meter| {
            typed::terms(&existing.typed, loaders, name, tag, lower, limit, meter)
        })
    }
}

/// One query request with its cursor context.
pub(super) struct QueryRequest<'r> {
    pub(super) namespace: NamespaceId,
    pub(super) query: &'r Query,
    pub(super) projection: &'r Projection,
    pub(super) limit: usize,
    pub(super) cursor: Option<&'r Cursor>,
    pub(super) context: CursorContext,
}

/// The limits a query or count read is admitted against: the instance's,
/// with `max_groups`, `max_literals` and `max_page` replaced by the budget's
/// read-side bounds when it carries them ([`Budget::with_query_limits`]).
fn query_admission(limits: &Limits, budget: Budget) -> Limits {
    match budget.query_limits() {
        Some(bounds) => Limits {
            max_groups: bounds.max_groups,
            max_literals: bounds.max_literals,
            max_page: bounds.max_page,
            ..*limits
        },
        None => *limits,
    }
}

impl QueryRequest<'_> {
    /// Admission: the query shape and the projection against the limits,
    /// then the cursor binding. All are decided before any charge.
    fn validate(&self, limits: &Limits, budget: Budget) -> Result<Option<RecordId>, Rejection> {
        typed::validate_query(&query_admission(limits, budget), self.query, self.limit)
            .map_err(Rejection::InvalidQuery)?;
        typed::validate_projection(limits, self.projection).map_err(Rejection::InvalidQuery)?;
        let Some(cursor) = self.cursor else {
            return Ok(None);
        };
        let bound = cursor.context == self.context
            && cursor.namespace == self.namespace
            && *cursor.query == *self.query
            && *cursor.projection == *self.projection;
        if bound {
            Ok(Some(cursor.after))
        } else {
            Err(Rejection::CursorMismatch)
        }
    }

    fn continuation(&self, after: RecordId) -> Cursor {
        let (query, projection) = match self.cursor {
            Some(cursor) => (Arc::clone(&cursor.query), Arc::clone(&cursor.projection)),
            None => (
                Arc::new(self.query.clone()),
                Arc::new(self.projection.clone()),
            ),
        };
        Cursor {
            context: self.context,
            namespace: self.namespace,
            query,
            projection,
            after,
        }
    }
}
