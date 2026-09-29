//! Bounded, root-reachable structural inspection for diagnostic tools.
use crate::{
    Link, MerkleMap, NodeKind, NodeLoader, ObjectId, Probe, TrieKey, TrieValue,
    map::{Role, resolve},
    node::Node,
};
use alloy_primitives::keccak256;

/// A child reference. Inline bytes are the bytes embedded in the parent.
#[derive(Clone, Debug)]
pub struct ExploreChild {
    pub slot: u8,
    pub hash: Option<ObjectId>,
    pub inline: Option<Vec<u8>>,
}
/// One actual node reached by branch slots (0..15) and extension steps (16).
#[derive(Clone, Debug)]
pub struct ExploreNode {
    pub prefix: Vec<u8>,
    pub hash: ObjectId,
    pub rlp: Vec<u8>,
    pub kind: NodeKind,
    pub compressed_path: Vec<u8>,
    pub key: Option<Vec<u8>>,
    pub value: Option<Vec<u8>>,
    pub children: Vec<ExploreChild>,
}
impl<K: TrieKey, V: TrieValue> MerkleMap<K, V> {
    /// Follows only the supplied root-relative route. Invalid routes return None;
    /// a loader/integrity failure remains an error. At most 2*key bytes+1 nodes.
    pub fn explore<L: NodeLoader<V>>(
        &self,
        path: &[u8],
        loader: &L,
    ) -> Result<Option<ExploreNode>, L::Error> {
        if path.len() > K::LEN * 2 || path.iter().any(|s| *s > 16) {
            return Ok(None);
        }
        let mut link = self.root.clone();
        let mut role = Role::Root;
        let mut prefix = Vec::new();
        let mut probe = Probe::new();
        for depth in 0..=path.len() {
            let Some(node) = resolve(&link, role, loader, &mut probe)? else {
                return Ok(None);
            };
            if depth == path.len() {
                let mut rlp = Vec::new();
                node.encode(&mut rlp);
                let child = |slot, link: &Link<V>| {
                    let (hash, inline) = match link {
                        Link::Empty => (None, None),
                        Link::Stored(id) => (Some(*id), None),
                        Link::Mem(n) => {
                            let mut b = Vec::new();
                            n.encode(&mut b);
                            (Some(keccak256(&b)), (b.len() < 32).then_some(b))
                        }
                    };
                    ExploreChild { slot, hash, inline }
                };
                let (compressed_path, value, children) = match &node.node {
                    Node::Leaf { path, value } => (path.to_vec(), Some(value.encode()), vec![]),
                    Node::Extension { path, child: c } => (path.to_vec(), None, vec![child(16, c)]),
                    Node::Branch { children } => (
                        vec![],
                        None,
                        children
                            .iter()
                            .enumerate()
                            .map(|(i, c)| child(i as u8, c))
                            .collect(),
                    ),
                };
                let key = if value.is_some() {
                    let mut n = prefix.clone();
                    n.extend(&compressed_path);
                    if n.len() != K::LEN * 2 {
                        return Err(crate::TrieError::inconsistent(
                            "explored leaf has wrong key length",
                        )
                        .into());
                    }
                    Some(n.chunks_exact(2).map(|c| c[0] * 16 + c[1]).collect())
                } else {
                    None
                };
                return Ok(Some(ExploreNode {
                    prefix,
                    hash: keccak256(&rlp),
                    rlp,
                    kind: node.kind(),
                    compressed_path,
                    key,
                    value,
                    children,
                }));
            }
            match &node.node {
                Node::Branch { children } if path[depth] < 16 => {
                    prefix.push(path[depth]);
                    link = children[usize::from(path[depth])].clone();
                    role = Role::Child;
                }
                Node::Extension { path: p, child } if path[depth] == 16 => {
                    prefix.extend_from_slice(p);
                    link = child.clone();
                    role = Role::ExtensionChild;
                }
                _ => return Ok(None),
            }
            if prefix.len() > K::LEN * 2 {
                return Err(
                    crate::TrieError::inconsistent("explored path exceeds key length").into(),
                );
            }
        }
        Ok(None)
    }
}
