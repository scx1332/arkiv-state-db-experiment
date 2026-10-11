//! Display-only JSON derived from the exact successfully verified witness.
//! These fields never authorize a proof badge; the full engine verifier does.
use super::wire::{hex, pack};
use arkiv_db::{
    B256, MapKind, NamespaceRoots, StateHeader, TypeTag,
    proof::{
        EqInspection, EqRequest, InspectedMapPath, InspectedPostingSet, RangeInspection,
        RangeRequest,
    },
};
use arkiv_db_trie::{IntervalDecision, NodeKind, PathChild, PathSource, PathTerminal};
use serde_json::{Value, json};

/// Inspection additionally copies node RLP into a graph. Keep its witness
/// smaller than the normal proof cap; the final JSON still has the shared
/// hard 8 MiB response cap and uses the shared four-request semaphore.
pub const INSPECT_PROOF_CAP: usize = 1024 * 1024;

fn nibbles(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|n| char::from(b"0123456789abcdef"[usize::from(*n)]))
        .collect()
}
fn map_handle(root: B256, entries: u64) -> Value {
    json!({"root":hex(root.as_slice()),"entries":entries.to_string()})
}
fn child(child: &PathChild, slot: Option<usize>) -> Value {
    let slot = slot.map(|n| format!("{n:x}"));
    match child {
        PathChild::Empty => json!({"slot":slot,"kind":"empty"}),
        PathChild::Hash(hash) => json!({"slot":slot,"kind":"hash","hash":hex(hash.as_slice())}),
        PathChild::Inline(bytes) => json!({"slot":slot,"kind":"inline","rlp":hex(bytes)}),
    }
}

pub fn json(request: &EqRequest, inspection: &EqInspection) -> Value {
    common(
        &request.query.namespace.ordinal().to_string(),
        request.query.digest(),
        &inspection.state_header,
        inspection.namespace.as_ref(),
        &inspection.point_paths,
        inspection.posting_set.as_ref(),
        super::profile::PROOF_PROFILE,
    )
}

fn common(
    namespace_id: &str,
    query_digest: B256,
    header: &StateHeader,
    namespace: Option<&NamespaceRoots>,
    point_paths: &[InspectedMapPath],
    posting_set: Option<&InspectedPostingSet>,
    profile: &str,
) -> Value {
    let namespace = namespace.map(|ns| {
        json!({
            "id":namespace_id,"nextRecord":ns.next_record.to_string(),
            "records":map_handle(ns.records.0,ns.records.1),
            "keys":map_handle(ns.keys.0,ns.keys.1),
            "rows":map_handle(ns.rows.0,ns.rows.1),
            "terms":map_handle(ns.terms.0,ns.terms.1),
        })
    });
    let paths = point_paths.iter().map(|path| {
        let map = match path.map {
            MapKind::Catalog => "catalog", MapKind::Terms => "terms",
            MapKind::Rows => "rows", MapKind::Keys => "keys",
            MapKind::Host => "host", MapKind::Records => "records", MapKind::Postings => "postings",
        };
        let record_id = path.record_id.map(|id| id.ordinal().to_string());
        let id = record_id.as_ref().map_or_else(||map.to_owned(),|id|format!("{map}:{id}"));
        let (kind, reason) = match path.trace.terminal {
            PathTerminal::Inclusion => ("inclusion","leaf-match"),
            PathTerminal::EmptyRoot => ("absence","empty-root"),
            PathTerminal::DivergentLeaf => ("absence","divergent-leaf"),
            PathTerminal::DivergentExtension => ("absence","divergent-extension"),
            PathTerminal::EmptyBranchSlot => ("absence","empty-branch-slot"),
            PathTerminal::KeyEndsAtBranch => ("absence","key-ends-at-branch"),
        };
        let nodes = path.trace.nodes.iter().enumerate().map(|(index,node)| {
            let kind = match node.kind { NodeKind::Branch => "branch", NodeKind::Extension => "extension", NodeKind::Leaf => "leaf" };
            let source = match node.source { PathSource::Root => "root", PathSource::Hash => "hash", PathSource::Inline => "inline" };
            let children = node.children.iter().enumerate().map(|(slot,c)|child(c,(node.kind==NodeKind::Branch).then_some(slot))).collect::<Vec<_>>();
            json!({"index":index,"proofIndex":node.proof_index,"depth":node.depth,
                "source":source,"hash":hex(node.hash.as_slice()),"rlp":hex(&node.rlp),"kind":kind,
                "compressedPath":nibbles(&node.compressed_path),"selectedNibble":node.selected_nibble.map(|n|format!("{n:x}")),
                "children":children,"valueBytes":node.value_bytes.to_string()})
        }).collect::<Vec<_>>();
        json!({"id":id,"map":map,"namespaceId":(path.map!=MapKind::Catalog).then_some(&namespace_id),"recordId":record_id,
            "root":hex(path.root.as_slice()),"key":hex(&path.key),"keyNibbles":hex(&path.key)[2..],
            "suppliedNodeCount":path.supplied_nodes,"terminal":{"kind":kind,"reason":reason},"nodes":nodes})
    }).collect::<Vec<_>>();
    let posting = posting_set.map(|posting| json!({
        "method":"complete-set-reconstruction",
        "authenticatedRoot":posting.authenticated_root.map(|root|hex(root.as_slice())),
        "reconstructedRoot":hex(posting.reconstructed_root.as_slice()),
        "termPresent":posting.authenticated_root.is_some(),"count":posting.ids.len(),
        "recordIds":posting.ids.iter().map(|id|id.ordinal().to_string()).collect::<Vec<_>>(),
        "selectedRecordIds":posting.selected_ids.iter().map(|id|id.ordinal().to_string()).collect::<Vec<_>>(),
    }));
    json!({"version":arkiv_db::LAYOUT_VERSION,"proofProfile":profile,"queryDigest":hex(query_digest.as_slice()),
        "stateComposition":{"stateRoot":hex(header.root().as_slice()),"domain":"arkiv/state/v1","headerBytes":pack(header),
            "catalog":map_handle(header.catalog_root,header.catalog_len),"host":map_handle(header.host_root,header.host_len),
            "nextNamespace":header.next_namespace.to_string(),"pricingBytes":pack(&header.pricing),"namespace":namespace},
        "pointPaths":paths,"postingSet":posting})
}

pub(crate) fn typed_value(value: &arkiv_db::Value) -> Value {
    crate::value_json::to_json(value)
}
pub(crate) fn tag_name(tag: TypeTag) -> &'static str {
    tag.name()
}

/// Every edge, witness position and decoded leaf comes from strict traversal.
/// Outside children expose only their commitment, never invented descendants.
pub fn range_json(request: &RangeRequest, inspection: &RangeInspection) -> Value {
    let namespace_id = request.query.namespace.ordinal().to_string();
    let mut result = common(
        &namespace_id,
        request.query.digest(),
        &inspection.state_header,
        inspection.namespace.as_ref(),
        &inspection.point_paths,
        None,
        arkiv_db::proof::RANGE_PROOF_PROFILE,
    );
    result["postingSets"] = json!(inspection.posting_sets.iter().map(|term| {
        let p = &term.posting;
        json!({"termNodeIndex":term.term_node_index,"termKey":hex(&term.term_key),
            "termValue":typed_value(&term.value),"termType":tag_name(term.value.tag()),
            "method":"complete-set-reconstruction",
            "authenticatedRoot":p.authenticated_root.map(|h|hex(h.as_slice())),
            "reconstructedRoot":hex(p.reconstructed_root.as_slice()),"termPresent":true,
            "count":p.ids.len(),
            "recordIds":p.ids.iter().map(|id|id.ordinal().to_string()).collect::<Vec<_>>(),
            "selectedRecordIds":p.selected_ids.iter().map(|id|id.ordinal().to_string()).collect::<Vec<_>>()})
    }).collect::<Vec<_>>());
    result["interval"] = inspection.interval.as_ref().map_or(Value::Null, |interval| {
        // Build child links in one pass. A trace index differs from a supplied
        // witness index when an embedded inline child is visited.
        let mut links = vec![Vec::<(Option<u8>,usize)>::new(); interval.trace.nodes.len()];
        for (index,node) in interval.trace.nodes.iter().enumerate() {
            if let Some(parent) = node.parent_index { links[parent].push((node.parent_slot,index)); }
        }
        let nodes = interval.trace.nodes.iter().enumerate().map(|(index,node)| {
            let n = &node.path_node;
            let kind = match n.kind { NodeKind::Branch=>"branch",NodeKind::Extension=>"extension",NodeKind::Leaf=>"leaf" };
            let source = match n.source { PathSource::Root=>"root",PathSource::Hash=>"hash",PathSource::Inline=>"inline" };
            let children = n.children.iter().enumerate().map(|(slot,c)| {
                let edge = (n.kind == NodeKind::Branch).then_some(slot);
                let mut value = child(c,edge);
                value["decision"] = json!(match node.child_decisions[slot] {
                    IntervalDecision::Open=>"open",IntervalDecision::Outside=>"outside",IntervalDecision::Empty=>"empty",
                });
                value["childIndex"] = json!(links[index].iter().find(|(s,_)|*s == edge.map(|n|n as u8)).map(|(_,i)|*i));
                value
            }).collect::<Vec<_>>();
            let leaf = interval.leaves[index].as_ref().map(|leaf| json!({
                "key":hex(&leaf.key),"attribute":leaf.attribute.as_str(),"valueType":tag_name(leaf.tag),
                "value":leaf.value.as_ref().map(typed_value),"included":leaf.included,
                "postingRoot":hex(leaf.posting_root.as_slice()),"postingCount":leaf.posting_count,
            }));
            json!({"index":index,"proofIndex":n.proof_index,"depth":n.depth,
                "source":source,"hash":hex(n.hash.as_slice()),"rlp":hex(&n.rlp),"kind":kind,
                "compressedPath":nibbles(&n.compressed_path),"selectedNibble":Value::Null,
                "children":children,"valueBytes":n.value_bytes.to_string(),
                "parentIndex":node.parent_index,"parentSlot":node.parent_slot.map(|s|format!("{s:x}")),
                "prefix":nibbles(&node.prefix),"leaf":leaf})
        }).collect::<Vec<_>>();
        let empty_reason = if interval.lower > interval.upper { Some("reversed-bounds") }
            else if interval.root == arkiv_db_trie::EMPTY_ROOT_HASH { Some("empty-root") } else { None };
        json!({"id":"terms:range","map":"terms","namespaceId":namespace_id,
            "root":hex(interval.root.as_slice()),"lowerKey":hex(&interval.lower),"upperKey":hex(&interval.upper),
            "lowerKeyNibbles":hex(&interval.lower)[2..],"upperKeyNibbles":hex(&interval.upper)[2..],
            "suppliedNodeCount":interval.supplied_nodes,"emptyReason":empty_reason,"nodes":nodes})
    });
    result
}
