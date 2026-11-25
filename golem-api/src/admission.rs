//! golem-db's write admission and its per-record accounting, for the adapter.
//!
//! golem-db does these in `golemdb-record` (`admission.rs`, `meta.rs`,
//! `outcome.rs`), which keeps them private. They are copied here: the same
//! checks, in the same order, with the same messages, so a caller sees
//! golem-db's errors and receipt details whatever engine stores the record.

use std::collections::BTreeMap;

use real::{
    ApiError, CellKind, CellLimits, CellName, CellValue, CreateEntry, Details, PatchChange,
    PatchEntry, RecordLimits, RecordMeta, Result,
};

/// A record's user cells, as golem-db values, in name order.
pub(crate) type Cells = BTreeMap<CellName, CellValue>;

/// golem's cell size (`golemdb-record`): record id, name, encoded value.
pub(crate) fn cell_size(name: &CellName, value: &CellValue) -> u64 {
    (8 + name.as_bytes().len() + value.encoded_bytes().len()) as u64
}

/// golem's index-term size: name, two framing bytes, payload.
pub(crate) fn index_size(name: &CellName, value: &CellValue) -> u64 {
    (name.as_bytes().len() + 2 + value.value().len()) as u64
}

/// golem's `#meta` counters, which it stores and the adapter derives.
pub(crate) fn meta_of(cells: &Cells) -> RecordMeta {
    let mut meta = RecordMeta::default();
    for (name, value) in cells {
        meta.cells += 1;
        meta.cell_bytes += cell_size(name, value);
        if value.is_indexable() {
            meta.indexed_cells += 1;
            meta.index_bytes += index_size(name, value);
        }
    }
    meta
}

/// golem's `Effects::change`: one user cell's effect on the receipt details.
pub(crate) fn effect(
    details: &mut Details,
    name: &CellName,
    before: Option<&CellValue>,
    after: Option<&CellValue>,
) {
    match (before, after) {
        (None, Some(_)) => details.cells_created += 1,
        (Some(_), Some(_)) => details.cells_updated += 1,
        (Some(_), None) => details.cells_deleted += 1,
        (None, None) => return,
    }
    if let Some(value) = before {
        details.cell_bytes_deleted += cell_size(name, value);
        if value.is_indexable() {
            details.index_leaves += 1;
            details.index_bytes_deleted += index_size(name, value);
        }
    }
    if let Some(value) = after {
        details.cell_bytes_written += cell_size(name, value);
        if value.is_indexable() {
            details.index_joins += 1;
            details.index_bytes_written += index_size(name, value);
        }
    }
}

/// golem's `RecordLimits::check`.
pub(crate) fn check_caps(caps: RecordLimits, meta: RecordMeta) -> Result<()> {
    for (name, count, cap) in [
        ("#maxRecordCells", meta.cells, caps.max_cells),
        (
            "#maxRecordIndexedCells",
            meta.indexed_cells,
            caps.max_indexed_cells,
        ),
    ] {
        if count > cap {
            return Err(ApiError::invalid_argument(format!(
                "resulting record count {count} exceeds {name} ({cap})"
            )));
        }
    }
    Ok(())
}

fn invalid(
    step: &str,
    name: &CellName,
    message: &str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
) -> ApiError {
    let bytes = name.as_bytes();
    let shown = &bytes[..bytes.len().min(64)];
    let suffix = if shown.len() < bytes.len() { "..." } else { "" };
    ApiError::InvalidArgument {
        message: format!("{step}(\"{}{suffix}\"): {message}", shown.escape_ascii()),
        source,
    }
}

fn write_step(kind: CellKind) -> &'static str {
    match kind {
        CellKind::Field => "field",
        CellKind::Attribute => "attribute",
    }
}

fn admit_entry(
    limits: &CellLimits,
    step: &str,
    name: &CellName,
    value: Option<&CellValue>,
) -> Result<()> {
    let max = limits.max_cell_name_len as usize;
    if name.as_bytes().len() > max {
        return Err(invalid(
            step,
            name,
            "invalid cell name",
            Some(Box::new(golemdb_cells::CellNameError::TooLong {
                max,
                actual: name.as_bytes().len(),
            })),
        ));
    }
    if let Some(value) = value {
        limits
            .validate_value(value.as_view())
            .map_err(|source| invalid(step, name, "invalid cell value", Some(Box::new(source))))?;
    }
    limits
        .parse_user_name(name.as_bytes())
        .map_err(|source| invalid(step, name, "invalid cell name", Some(Box::new(source))))?;
    Ok(())
}

fn with_kind(step: &str, name: &CellName, value: &CellValue, kind: CellKind) -> Result<CellValue> {
    value
        .clone()
        .with_kind(kind)
        .map_err(|source| invalid(step, name, "invalid cell value", Some(Box::new(source))))
}

fn duplicate(step: &str, name: &CellName) -> ApiError {
    invalid(
        step,
        name,
        "the operation already writes or removes this cell",
        None,
    )
}

pub(crate) fn admit_create(limits: &CellLimits, entries: &[CreateEntry]) -> Result<Cells> {
    let mut cells = Cells::new();
    for CreateEntry { name, value, kind } in entries {
        let step = write_step(*kind);
        admit_entry(limits, step, name, Some(value))?;
        let value = with_kind(step, name, value, *kind)?;
        if cells.insert(name.clone(), value).is_some() {
            return Err(duplicate(step, name));
        }
    }
    Ok(cells)
}

/// One admitted patch entry, before the stored cell is known.
pub(crate) enum Change {
    Write(CellValue),
    Set(CellValue),
    Remove,
}

pub(crate) fn admit_patch(
    limits: &CellLimits,
    entries: &[PatchEntry],
) -> Result<BTreeMap<CellName, Change>> {
    let mut changes = BTreeMap::new();
    for PatchEntry { name, change } in entries {
        let (step, value) = match change {
            PatchChange::Write { value, kind } => (write_step(*kind), Some(value)),
            PatchChange::Set(value) => ("set", Some(value)),
            PatchChange::Remove => ("remove", None),
        };
        admit_entry(limits, step, name, value)?;
        let change = match change {
            PatchChange::Write { value, kind } => {
                Change::Write(with_kind(step, name, value, *kind)?)
            }
            PatchChange::Set(value) => Change::Set(value.clone()),
            PatchChange::Remove => Change::Remove,
        };
        if changes.insert(name.clone(), change).is_some() {
            return Err(duplicate(step, name));
        }
    }
    Ok(changes)
}
