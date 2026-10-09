//! Independent state-format-1 leaf construction over the logical block model.
//! No engine codec/layout/digest or trie implementation under test is used.
use super::{CellKind, Model, Value, Weights};
use alloy_primitives::{B256, keccak256};
use std::collections::{BTreeMap, BTreeSet};
#[path = "../../../arkiv-db-trie/tests/support/mod.rs"]
mod trie_support;
fn bytes(b: &[u8]) -> Vec<u8> {
    let mut v = (b.len() as u32).to_be_bytes().to_vec();
    v.extend(b);
    v
}
fn padded(b: &[u8], n: usize) -> Vec<u8> {
    let mut v = b.to_vec();
    v.resize(n, 0);
    v.push(b.len() as u8);
    v
}
fn tag(v: &Value) -> u8 {
    match v {
        Value::Bool(_) => 1,
        Value::Str(_) => 2,
        Value::Bytes(_) => 3,
        Value::U64(_) => 13,
        Value::I64(_) => 17,
        _ => panic!("scalar outside the root fixture"),
    }
}
fn value(v: &Value) -> Vec<u8> {
    let mut b = vec![tag(v)];
    match v {
        Value::Str(v) => b.extend(bytes(v.as_bytes())),
        Value::Bytes(v) => b.extend(bytes(v)),
        fixed => b.extend(term_value(fixed)),
    };
    b
}
fn term_value(v: &Value) -> Vec<u8> {
    match v {
        Value::Bool(v) => vec![u8::from(*v)],
        Value::I64(v) => ((*v as u64) ^ (1 << 63)).to_be_bytes().to_vec(),
        Value::U64(v) => v.to_be_bytes().to_vec(),
        Value::Str(v) => v.as_bytes().to_vec(),
        Value::Bytes(_) => unreachable!(),
        _ => panic!("scalar outside the root fixture"),
    }
}
fn term(name: &str, t: u8, v: Option<Vec<u8>>) -> Vec<u8> {
    let mut b = padded(name.as_bytes(), 32);
    b.push(t);
    b.push(u8::from(v.is_some()));
    b.extend(padded(&v.unwrap_or_default(), 128));
    b
}
fn handle(m: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut b = trie_support::reference::root(m).to_vec();
    b.extend((m.len() as u64).to_be_bytes());
    b
}
fn schedule(w: Weights) -> Vec<u8> {
    let mut b = w.id.to_be_bytes().to_vec();
    b.extend(31u32.to_be_bytes());
    for n in [
        w.directory_lookup,
        w.namespace_create,
        w.namespace_transfer,
        w.put_record,
        w.delete_record,
        w.write_key_byte,
        w.write_value_byte,
        w.point_read,
        w.scan_seek,
        w.scan_entry,
        w.read_byte,
        w.record_create,
        w.record_patch,
        w.record_delete,
        w.cell_write,
        w.cell_byte,
        w.cell_remove,
        w.index_insert,
        w.index_term_create,
        w.index_remove,
        w.query_group,
        w.term_seek,
        w.term_visit,
        w.posting_entry,
        w.row_visit,
        w.record_probe,
        w.result_row,
        w.root_update,
        w.row_byte,
        w.proof_node,
        w.proof_byte,
    ] {
        b.extend(n.to_be_bytes());
    }
    b
}
pub(super) fn root(model: &Model) -> B256 {
    let mut catalog = BTreeMap::new();
    for (id, ns) in &model.namespaces {
        let raw = ns
            .raw
            .iter()
            .map(|(k, v)| (padded(k, 20), bytes(v)))
            .collect();
        let mut keys = BTreeMap::new();
        let mut rows = BTreeMap::new();
        let mut posts: BTreeMap<Vec<u8>, BTreeSet<u64>> = BTreeMap::new();
        for (key, row) in &ns.records {
            let mut bound = vec![key.len() as u8];
            bound.extend(key);
            bound.resize(33, 0);
            keys.insert(bound, row.id.to_be_bytes().to_vec());
            let mut leaf = vec![key.len() as u8];
            leaf.extend(key);
            leaf.extend((row.cells.len() as u32).to_be_bytes());
            for (name, cell) in &row.cells {
                leaf.extend(bytes(name.as_str().as_bytes()));
                leaf.push(if cell.kind == CellKind::Attribute {
                    1
                } else {
                    2
                });
                leaf.extend(value(&cell.value));
                if cell.kind == CellKind::Attribute {
                    for k in [
                        term(name.as_str(), tag(&cell.value), None),
                        term(
                            name.as_str(),
                            tag(&cell.value),
                            Some(term_value(&cell.value)),
                        ),
                    ] {
                        posts.entry(k).or_default().insert(row.id);
                    }
                }
            }
            rows.insert(row.id.to_be_bytes().to_vec(), leaf);
        }
        let terms = posts
            .into_iter()
            .map(|(k, ids)| {
                let members = ids
                    .into_iter()
                    .map(|id| (id.to_be_bytes().to_vec(), vec![1]))
                    .collect();
                (k, handle(&members))
            })
            .collect();
        let mut leaf = id.to_be_bytes().to_vec();
        leaf.extend(bytes(ns.name.as_str().as_bytes()));
        leaf.extend(ns.owner.as_bytes());
        leaf.extend(ns.revision.to_be_bytes());
        leaf.extend(ns.next_record.to_be_bytes());
        for map in [&raw, &keys, &rows, &terms] {
            leaf.extend(handle(map));
        }
        catalog.insert(id.to_be_bytes().to_vec(), leaf);
    }
    let mut host = BTreeMap::new();
    for ((height, namespace), count) in &model.due {
        let mut key = vec![0; 32];
        key[0] = b'D';
        key[1..9].copy_from_slice(&height.to_be_bytes());
        key[9..17].copy_from_slice(&namespace.to_be_bytes());
        host.insert(key, bytes(&count.to_be_bytes()));
    }
    for (height, count) in &model.totals {
        let mut key = vec![0; 32];
        key[0] = b'T';
        key[1..9].copy_from_slice(&height.to_be_bytes());
        host.insert(key, bytes(&count.to_be_bytes()));
    }
    let mut header = 1u32.to_be_bytes().to_vec();
    header.extend(handle(&catalog));
    header.extend(model.next_namespace.to_be_bytes());
    header.extend(schedule(model.pending));
    header.extend(handle(&host));
    let domain = b"arkiv/state/v1";
    let mut preimage = vec![domain.len() as u8];
    preimage.extend(domain);
    preimage.extend(header);
    keccak256(preimage)
}
