//! Read-only diagnostic tree traversal. Never accepts arbitrary object IDs.
use super::*;
use crate::layout::{Posting, TermKey};
use arkiv_db_trie::{ExploreNode, MerkleMap, TrieKey, TrieValue};

/// Maximum traversal steps, derived from the selected state layout.
pub const MAX_TREE_PATH: usize = <TermKey as TrieKey>::LEN * 2;

#[derive(Clone, Debug)]
pub enum ExploredLeaf {
    Namespace {
        descriptor: NamespaceDescriptor,
        maps: Vec<(&'static str, B256, u64)>,
    },
    Term {
        attribute: String,
        tag: crate::TypeTag,
        value: Option<crate::Value>,
        root: B256,
        count: u64,
    },
    RecordId(u64),
    Row {
        id: u64,
        key: Vec<u8>,
        cells: Vec<(CellName, Cell)>,
    },
}
#[derive(Clone, Debug)]
pub struct ExploredMap {
    pub root: B256,
    pub count: u64,
    pub node: Option<ExploreNode>,
    pub decoded: Option<ExploredLeaf>,
}
impl Snapshot {
    /// Bounded diagnostics for catalog and namespace maps. Host state stays private.
    /// None means namespace/path not found; empty maps have node=None.
    pub fn explore_tree(
        &self,
        kind: &str,
        namespace: Option<NamespaceId>,
        path: &[u8],
        term_path: &[u8],
    ) -> Result<Option<ExploredMap>, StorageError> {
        if path.len() > MAX_TREE_PATH || term_path.len() > MAX_TREE_PATH {
            return Ok(None);
        }
        fn view<K: TrieKey, V: TrieValue + Send + Sync + 'static>(
            m: &MerkleMap<K, V>,
            path: &[u8],
            loader: &crate::nodes::Loader<'_>,
        ) -> Result<Option<ExploredMap>, StorageError> {
            let node = m.explore(path, loader).map_err(|e| e.0)?;
            if node.is_none() && (!m.is_empty() || !path.is_empty()) {
                return Ok(None);
            }
            Ok(Some(ExploredMap {
                root: m.root_hash(),
                count: m.len(),
                node,
                decoded: None,
            }))
        }
        let state = &self.state.state;
        let nodes = &self.state.nodes;
        let mut result = if kind == "catalog" {
            view(&state.catalog, path, &nodes.catalog())?
        } else {
            let Some(ns) = namespace else { return Ok(None) };
            let Some(n) = state.namespace(nodes, ns)? else {
                return Ok(None);
            };
            let l = nodes.namespace(ns);
            match kind {
                "records" => view(&n.records, path, &l.records)?,
                "keys" => view(&n.typed.keys, path, &l.keys)?,
                "rows" => view(&n.typed.rows, path, &l.rows)?,
                "terms" => view(&n.typed.terms, path, &l.terms)?,
                "postings" => {
                    let Some(term) = n
                        .typed
                        .terms
                        .explore(term_path, &l.terms)
                        .map_err(|e| e.0)?
                    else {
                        return Ok(None);
                    };
                    let Some(key) = term.key else { return Ok(None) };
                    let key = TermKey::decode(&key).map_err(|e| corrupt(e.to_string()))?;
                    let posting = n
                        .typed
                        .terms
                        .get(&key, &l.terms, &mut Probe::new())
                        .map_err(|e| e.0)?
                        .ok_or_else(|| corrupt("explored term disappeared"))?;
                    view(&posting.0, path, &l.postings)?
                }
                _ => return Ok(None),
            }
        };
        if let Some(map) = &mut result
            && let Some(node) = &map.node
            && let (Some(value), Some(key)) = (&node.value, &node.key)
        {
            map.decoded = match kind {
                "catalog" => {
                    let n = NamespaceState::decode(value).map_err(|e| corrupt(e.to_string()))?;
                    if n.descriptor.id.to_bytes().as_slice() != key {
                        return Err(corrupt("catalog leaf ID mismatch"));
                    }
                    Some(ExploredLeaf::Namespace {
                        descriptor: n.descriptor,
                        maps: vec![
                            ("records", n.records.root_hash(), n.records.len()),
                            ("keys", n.typed.keys.root_hash(), n.typed.keys.len()),
                            ("rows", n.typed.rows.root_hash(), n.typed.rows.len()),
                            ("terms", n.typed.terms.root_hash(), n.typed.terms.len()),
                        ],
                    })
                }
                "terms" => {
                    let t = TermKey::decode(key).map_err(|e| corrupt(e.to_string()))?;
                    let p = Posting::decode(value).map_err(|e| corrupt(e.to_string()))?;
                    Some(ExploredLeaf::Term {
                        attribute: t.name.as_str().into(),
                        tag: t.tag,
                        value: t
                            .value
                            .as_ref()
                            .and_then(|v| crate::Value::from_term_bytes(t.tag, v)),
                        root: p.0.root_hash(),
                        count: p.0.len(),
                    })
                }
                "keys" => Some(ExploredLeaf::RecordId(
                    <RecordId as TrieValue>::decode(value)
                        .map_err(|e| corrupt(e.to_string()))?
                        .ordinal(),
                )),
                "postings" => Some(ExploredLeaf::RecordId(
                    <RecordId as TrieKey>::decode(key)
                        .map_err(|e| corrupt(e.to_string()))?
                        .ordinal(),
                )),
                "rows" => {
                    let r = crate::layout::StoredRecord::decode(value)
                        .map_err(|e| corrupt(e.to_string()))?;
                    Some(ExploredLeaf::Row {
                        id: <RecordId as TrieKey>::decode(key)
                            .map_err(|e| corrupt(e.to_string()))?
                            .ordinal(),
                        key: r.key.as_bytes().to_vec(),
                        cells: r.cells.to_vec(),
                    })
                }
                _ => None,
            };
        }
        Ok(result)
    }
}
