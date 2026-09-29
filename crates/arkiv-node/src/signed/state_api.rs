//! Diagnostic full-node tree reads; these responses are not light-node proofs.
use super::{Node, Shared, parse, snapshot};
use crate::{
    signed::{Error, Result},
    simulator::wire::{REQUEST_CAP, hex, number, pack},
};
use arkiv_db::{
    NamespaceId, SnapshotId,
    engine::explore::{ExploredLeaf, MAX_TREE_PATH},
};
use arkiv_db_trie::NodeKind;
use axum::body::Body;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    height: String,
    #[serde(default = "catalog")]
    map: String,
    namespace: Option<String>,
    #[serde(default)]
    path: Vec<u8>,
    #[serde(default)]
    term_path: Vec<u8>,
}
fn catalog() -> String {
    "catalog".into()
}
fn nibbles(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|n| char::from(b"0123456789abcdef"[usize::from(*n)]))
        .collect()
}

pub(super) async fn handle(
    rt: Shared,
    body: Body,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Json> {
    let q: Request = parse(body, REQUEST_CAP).await?;
    if !matches!(
        q.map.as_str(),
        "catalog" | "records" | "keys" | "rows" | "terms" | "postings"
    ) || q.path.len() > MAX_TREE_PATH
        || q.term_path.len() > MAX_TREE_PATH
        || q.path.iter().chain(&q.term_path).any(|n| *n > 16)
        || (q.map != "postings" && !q.term_path.is_empty())
        || (q.map == "catalog" && q.namespace.is_some())
    {
        return Err(Error("InvalidRequest"));
    }
    let ns = q
        .namespace
        .as_deref()
        .map(number)
        .transpose()?
        .map(|n| NamespaceId::from_ordinal(n).ok_or(Error("InvalidRequest")))
        .transpose()?;
    if q.map != "catalog" && ns.is_none() {
        return Err(Error("InvalidRequest"));
    }
    let height = number(&q.height)?;
    tokio::task::spawn_blocking(move||{
        let _permit=permit;
        let node=rt.node.try_lock().map_err(|_|Error("ServerBusy"))?;
        let Node::Full(host)=&*node else{return Err(Error("UnsupportedNodeRole"))};
        let certificate=host.certificate(height)?;
        let s=host.engine.snapshot(SnapshotId::from_sequence(height))?;
        let h=s.state_header();
        if Some(h.root())!=certificate.header.state_root{return Err(Error("InvalidProof"))}
        let m=s.explore_tree(&q.map,ns,&q.path,&q.term_path)?.ok_or(Error("TreePathNotFound"))?;
        // Values can carry payloads. Bound bytes before hexadecimal/JSON amplification.
        if m.node.as_ref().is_some_and(|n|n.rlp.len()+n.value.as_ref().map_or(0,Vec::len)>384*1024){return Err(Error("ResponseTooLarge"))}
        let decoded=match m.decoded {
            None=>Json::Null,
            Some(ExploredLeaf::Namespace{descriptor,maps})=>json!({"namespace":descriptor.id.ordinal().to_string(),"name":descriptor.name.as_str(),"owner":hex(descriptor.owner.as_bytes()),"maps":maps.into_iter().map(|(kind,root,count)|json!({"kind":kind,"root":hex(root.as_slice()),"count":count.to_string()})).collect::<Vec<_>>()}),
            Some(ExploredLeaf::Term{attribute,tag,value,root,count})=>json!({"attribute":attribute,"valueType":crate::simulator::inspection::tag_name(tag),"value":value.as_ref().map(crate::simulator::inspection::typed_value),"existence":value.is_none(),"postingRoot":hex(root.as_slice()),"postingCount":count.to_string(),"termPath":q.path}),
            Some(ExploredLeaf::RecordId(id))=>json!({"recordId":id.to_string()}),
            Some(ExploredLeaf::Row{id,key,cells})=>json!({"recordId":id.to_string(),"recordKey":hex(&key),"cellCount":cells.len(),"cells":cells.into_iter().map(|(name,cell)|{
                let (value,truncated,byte_length)=match &cell.value {
                    arkiv_db::Value::Bytes(b)=>(json!(hex(&b[..b.len().min(32)])),b.len()>32,Some(b.len())),
                    arkiv_db::Value::Str(s)=>{let preview:String=s.chars().take(256).collect();let truncated=preview.len()!=s.len();(json!(preview),truncated,Some(s.len()))},
                    value=>(crate::simulator::inspection::typed_value(value),false,None)
                };
                json!({"name":name.as_str(),"kind":if cell.is_attribute(){"attribute"}else{"field"},"valueType":crate::simulator::inspection::tag_name(cell.value.tag()),"value":value,"valueTruncated":truncated,"byteLength":byte_length})
            }).collect::<Vec<_>>()}),
        };
        let tree_node=m.node.map(|n|json!({"path":q.path,"prefix":nibbles(&n.prefix),"hash":hex(n.hash.as_slice()),"rlp":hex(&n.rlp),"kind":match n.kind{NodeKind::Leaf=>"leaf",NodeKind::Extension=>"extension",NodeKind::Branch=>"branch"},"compressedPath":nibbles(&n.compressed_path),"key":n.key.as_ref().map(|k|hex(k)),"valueBytes":n.value.as_ref().map(|v|hex(v)),"decoded":decoded,"children":n.children.into_iter().map(|c|{let mut path=q.path.clone();path.push(c.slot);json!({"slot":c.slot,"kind":if c.hash.is_none(){"empty"}else if c.inline.is_some(){"inline"}else{"hash"},"hash":c.hash.map(|h|hex(h.as_slice())),"rlp":c.inline.map(|b|hex(&b)),"path":path})}).collect::<Vec<_>>()}));
        let result=json!({"apiVersion":1,"identity":host.identity,"snapshot":snapshot(&certificate.header),"certificateBytes":pack(&certificate),"verification":{"status":"exploratory","source":"full-node"},"stateComposition":{"domain":"arkiv/state/v1","encodedHeader":pack(&h),"stateRoot":hex(h.root().as_slice()),"catalogRoot":hex(h.catalog_root.as_slice()),"catalogCount":h.catalog_len.to_string(),"hostRoot":hex(h.host_root.as_slice()),"hostCount":h.host_len.to_string()},"map":{"kind":q.map,"namespace":q.namespace,"root":hex(m.root.as_slice()),"count":m.count.to_string(),"termPath":q.term_path},"node":tree_node});
        crate::simulator::wire::json_bytes(&result,1024*1024)?;
        Ok(result)
    }).await.map_err(|_|Error("StorageUnavailable"))?
}
