use neon::prelude::*;
use neon::types::buffer::TypedArray;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use thiserror::Error;

use crate::consts;
use crate::smt_db;
use crate::utils;

#[derive(Clone, Debug, PartialEq)]
pub struct UpdateData {
    data: HashMap<Vec<u8>, Vec<u8>>,
}

#[derive(Error, Debug)]
pub enum SMTError {
    #[error("Invalid input: `{0}`")]
    InvalidInput(String),
    #[error("unknown data not found error `{0}`")]
    NotFound(String),
    #[error("Invalid state root `{0}`")]
    InvalidRoot(String),
    #[error("unknown data store error `{0}`")]
    Unknown(String),
}

const PREFIX_INT_LEAF_HASH: u8 = 0;
const PREFIX_INT_BRANCH_HASH: u8 = 1;
const PREFIX_INT_EMPTY: u8 = 2;
const HASH_SIZE: usize = 32;
const PREFIX_SIZE: usize = 6;
static PREFIX_LEAF_HASH: &[u8] = &[0];
static PREFIX_BRANCH_HASH: &[u8] = &[1];
static PREFIX_EMPTY: &[u8] = &[2];

impl rocksdb::WriteBatchIterator for UpdateData {
    /// Called with a key and value that were `put` into the batch.
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        self.data.insert(key_hash(&key), value_hash(&value));
    }
    /// Called with a key that was `delete`d from the batch.
    fn delete(&mut self, key: Box<[u8]>) {
        self.data.insert(key_hash(&key), vec![]);
    }
}

struct KVPair(Vec<u8>, Vec<u8>);

impl UpdateData {
    pub fn new_from(data: HashMap<Vec<u8>, Vec<u8>>) -> Self {
        Self { data: data }
    }

    pub fn new_with_hash(data: HashMap<Vec<u8>, Vec<u8>>) -> Self {
        let mut new_data = HashMap::new();
        for (k, v) in data {
            if v.len() != 0 {
                new_data.insert(key_hash(&k), value_hash(&v));
            } else {
                new_data.insert(key_hash(&k), vec![]);
            }
        }
        Self { data: new_data }
    }

    pub fn entries(&self) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let mut kvpairs = vec![];
        for (k, v) in self.data.iter() {
            kvpairs.push(KVPair(k.clone(), v.clone()));
        }
        kvpairs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut keys = vec![];
        let mut values = vec![];
        for kv in kvpairs {
            keys.push(kv.0);
            values.push(kv.1);
        }
        (keys, values)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }
}

fn key_hash(key: &[u8]) -> Vec<u8> {
    let prefix = key[..PREFIX_SIZE].to_vec();
    let body = key[PREFIX_SIZE..].to_vec();
    let mut hasher = Sha256::new();
    hasher.update(body);
    let result = hasher.finalize();
    return [prefix, result.as_slice().to_vec()].concat();
}

fn value_hash(value: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(value);
    let result = hasher.finalize();
    return result.as_slice().to_vec();
}

fn leaf_hash(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(PREFIX_LEAF_HASH);
    hasher.update(key);
    hasher.update(value);
    let result = hasher.finalize();
    return result.as_slice().to_vec();
}

fn branch_hash(node_hash: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(PREFIX_BRANCH_HASH);
    hasher.update(node_hash);
    let result = hasher.finalize();
    return result.as_slice().to_vec();
}

fn empty_hash() -> Vec<u8> {
    let hasher = Sha256::new();
    let result = hasher.finalize();
    return result.as_slice().to_vec();
}

#[derive(Clone, Debug)]
pub struct Proof {
    pub sibling_hashes: Vec<Vec<u8>>,
    pub queries: Vec<QueryProof>,
}

#[derive(Clone, Debug)]
pub struct QueryProof {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub bitmap: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
enum NodeKind {
    Empty,
    Leaf,
    Stub,
    Temp,
}

#[derive(Clone, Debug)]
struct Node {
    kind: NodeKind,
    key: Vec<u8>,
    data: Vec<u8>,
    hash: Vec<u8>,
}

impl Node {
    fn new_temp() -> Self {
        Self {
            kind: NodeKind::Temp,
            data: vec![],
            hash: vec![],
            key: vec![],
        }
    }

    fn new_stub(node_hash: &[u8]) -> Self {
        let data = [PREFIX_BRANCH_HASH, node_hash].concat();
        Self {
            kind: NodeKind::Stub,
            data: data,
            hash: node_hash.to_vec(),
            key: vec![],
        }
    }

    fn new_leaf(key: &[u8], value: &[u8]) -> Self {
        let h = leaf_hash(key, value);
        let data = [PREFIX_LEAF_HASH, key, value].concat();
        Self {
            kind: NodeKind::Leaf,
            data: data,
            hash: h,
            key: key.to_vec(),
        }
    }

    fn new_empty() -> Self {
        let h = empty_hash();
        let data = [PREFIX_EMPTY].concat();
        Self {
            kind: NodeKind::Empty,
            data: data,
            hash: h,
            key: vec![],
        }
    }
}

#[derive(Clone, Debug)]
struct SubTree {
    structure: Vec<u8>,
    nodes: Vec<Node>,
    root: Vec<u8>,
}

impl SubTree {
    pub fn new(data: Vec<u8>, key_length: usize, hasher: Hasher) -> Result<Self, SMTError> {
        if data.len() == 0 {
            return Err(SMTError::InvalidInput(String::from("keys length is zero")));
        }
        let node_length: usize = data[0] as usize + 1;
        let structure = data[1..node_length + 1].to_vec();
        let node_data = data[node_length + 1..].to_vec();
        let mut nodes = vec![];
        let mut idx = 0;

        while idx < node_data.len() {
            match node_data[idx] {
                PREFIX_INT_LEAF_HASH => {
                    let key = node_data
                        [idx + PREFIX_LEAF_HASH.len()..idx + PREFIX_LEAF_HASH.len() + key_length]
                        .to_vec();
                    let value = node_data[idx + PREFIX_LEAF_HASH.len() + key_length
                        ..idx + PREFIX_LEAF_HASH.len() + key_length + HASH_SIZE]
                        .to_vec();
                    let node = Node::new_leaf(key.as_slice(), value.as_slice());
                    nodes.push(node);
                    idx += PREFIX_LEAF_HASH.len() + key_length + HASH_SIZE;
                }
                PREFIX_INT_BRANCH_HASH => {
                    let node_hash = node_data[idx + PREFIX_BRANCH_HASH.len()
                        ..idx + PREFIX_BRANCH_HASH.len() + HASH_SIZE]
                        .to_vec();
                    nodes.push(Node::new_stub(node_hash.as_slice()));
                    idx += PREFIX_BRANCH_HASH.len() + HASH_SIZE;
                }
                PREFIX_INT_EMPTY => {
                    nodes.push(Node::new_empty());
                    idx += PREFIX_EMPTY.len();
                }
                _ => {
                    return Err(SMTError::InvalidInput(String::from(
                        "Invalid data. key prefix is invalid.",
                    )));
                }
            }
        }

        SubTree::from_data(structure, nodes, hasher)
    }

    pub fn from_data(
        structure: Vec<u8>,
        nodes: Vec<Node>,
        hasher: Hasher,
    ) -> Result<Self, SMTError> {
        let height = structure
            .iter()
            .max()
            .ok_or(SMTError::Unknown(String::from("Invalid structure")))?;

        let node_hashes = nodes.iter().map(|n| n.hash.clone()).collect();
        let calculated = hasher(&node_hashes, &structure, *height as usize);

        Ok(Self {
            structure: structure,
            nodes: nodes,
            root: calculated,
        })
    }

    pub fn new_empty() -> Self {
        let structure = vec![0];
        let empty = Node::new_empty();
        let node_hashes = vec![Node::new_empty()];

        Self {
            structure: structure,
            nodes: node_hashes,
            root: empty.hash,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let node_length = (self.structure.len() - 1) as u8;
        let node_hashes: Vec<Vec<u8>> = self.nodes.iter().map(|n| {
            n.data.clone()
        }).collect();
        [
            vec![node_length],
            self.structure.clone(),
            node_hashes.concat(),
        ]
        .concat()
    }
}

type Hasher = fn(node_hashes: &Vec<Vec<u8>>, structure: &Vec<u8>, height: usize) -> Vec<u8>;

pub struct SMT {
    root: Vec<u8>,
    key_length: usize,
    subtree_height: usize,
    max_number_of_nodes: usize,
    hasher: Hasher,
}

pub trait DB {
    fn get(&self, key: Vec<u8>) -> Result<Option<Vec<u8>>, rocksdb::Error>;
    fn set(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), rocksdb::Error>;
    fn del(&mut self, key: Vec<u8>) -> Result<(), rocksdb::Error>;
}

fn tree_hasher(node_hashes: &Vec<Vec<u8>>, structure: &Vec<u8>, height: usize) -> Vec<u8> {
    if node_hashes.len() == 1 {
        return node_hashes[0].clone();
    }
    let mut next_hashes = vec![];
    let mut next_structure = vec![];
    let mut i = 0;

    while i < node_hashes.len() {
        if structure[i] == height as u8 {
            let branch = [node_hashes[i].clone(), node_hashes[i + 1].clone()].concat();
            let hash = branch_hash(branch.as_slice());
            next_hashes.push(hash);
            next_structure.push(structure[i] - 1);
            i += 1;
        } else {
            next_hashes.push(node_hashes[i].clone());
            next_structure.push(structure[i]);
        }
        i += 1;
    }

    if height == 1 {
        return next_hashes[0].clone();
    }

    tree_hasher(&next_hashes, &next_structure, height - 1)
}

fn calculate_subtree(
    layer_nodes: &Vec<Node>,
    layer_structure: &Vec<u8>,
    height: u8,
    tree_map: &mut VecDeque<(Vec<Node>, Vec<u8>)>,
    hasher: Hasher,
) -> Result<SubTree, SMTError> {
    if height == 0 {
        return SubTree::from_data(vec![0], layer_nodes.clone(), hasher);
    }
    let mut next_layer_nodes: Vec<Node> = vec![];
    let mut next_layer_structure: Vec<u8> = vec![];
    let mut i = 0;
    while i < layer_nodes.len() {
        if layer_structure[i] != height {
            next_layer_nodes.push(layer_nodes[i].clone());
            next_layer_structure.push(layer_structure[i]);
            i += 1;
            continue;
        }
        let parent_node = if layer_nodes[i].kind == NodeKind::Empty
            && layer_nodes[i + 1].kind == NodeKind::Empty
        {
            layer_nodes[i].clone()
        } else if layer_nodes[i].kind == NodeKind::Empty
            && layer_nodes[i + 1].kind == NodeKind::Leaf
        {
            layer_nodes[i + 1].clone()
        } else if layer_nodes[i].kind == NodeKind::Leaf
            && layer_nodes[i + 1].kind == NodeKind::Empty
        {
            layer_nodes[i].clone()
        } else {
            let (mut left_nodes, mut left_structure) = if layer_nodes[i].kind == NodeKind::Temp {
                let (nodes, structure) = tree_map.pop_back().ok_or(SMTError::Unknown(
                    String::from("Subtree must exist for stub"),
                ))?;
                (nodes.clone(), structure.clone())
            } else {
                (vec![layer_nodes[i].clone()], vec![layer_structure[i]])
            };
            let (right_nodes, right_structure) = if layer_nodes[i + 1].kind == NodeKind::Temp {
                let (nodes, structure) = tree_map.pop_back().ok_or(SMTError::Unknown(
                    String::from("Subtree must exist for stub"),
                ))?;
                (nodes.clone(), structure.clone())
            } else {
                (
                    vec![layer_nodes[i + 1].clone()],
                    vec![layer_structure[i + 1]],
                )
            };
            left_structure.extend(right_structure);
            left_nodes.extend(right_nodes);
            let stub = Node::new_temp();
            tree_map.push_front((left_nodes, left_structure));

            stub
        };
        next_layer_nodes.push(parent_node.clone());
        next_layer_structure.push(layer_structure[i] - 1);
        // using 2 layer nodes
        i += 2;
    }
    if height == 1 {
        if next_layer_nodes[0].kind == NodeKind::Temp {
            let (nodes, structure) = tree_map
                .pop_front()
                .ok_or(SMTError::Unknown(String::from(
                    "Subtree must exist for stub",
                )))
                .and_then(|node| Ok(node.clone()))?;
            return SubTree::from_data(structure, nodes, hasher);
        }
        return SubTree::from_data(vec![0], next_layer_nodes, hasher);
    }
    calculate_subtree(
        &next_layer_nodes,
        &next_layer_structure,
        height - 1,
        tree_map,
        hasher,
    )
}

impl SMT {
    pub fn new(root: Vec<u8>, key_length: usize, subtree_height: usize) -> Self {
        let max_number_of_nodes = 1 << subtree_height;
        let r = if root.len() == 0 {
            utils::empty_hash()
        } else {
            root
        };
        Self {
            root: r,
            key_length: key_length,
            hasher: tree_hasher,
            subtree_height: subtree_height,
            max_number_of_nodes: max_number_of_nodes,
        }
    }

    pub fn commit(&mut self, db: &mut impl DB, data: &mut UpdateData) -> Result<Vec<u8>, SMTError> {
        if data.len() == 0 {
            return Ok(self.root.clone());
        }
        let (update_keys, update_values) = data.entries();
        let root = self.get_subtree(db, &self.root)?;
        let new_root = self.update_subtree(db, update_keys, update_values, &root, 0)?;
        self.root = new_root.root;
        Ok(self.root.clone())
    }

    pub fn prove(&mut self, db: &mut impl DB, queries: Vec<Vec<u8>>) -> Result<Proof, SMTError> {
        Ok(Proof {
            queries: vec![],
            sibling_hashes: vec![],
        })
    }

    fn get_subtree(&self, db: &impl DB, node_hash: &Vec<u8>) -> Result<SubTree, SMTError> {
        if node_hash.len() == 0 {
            return Ok(SubTree::new_empty());
        }

        if utils::is_empty_hash(node_hash) {
            return Ok(SubTree::new_empty());
        }

        let value = db
            .get(node_hash.clone())
            .or_else(|err| Err(SMTError::Unknown(err.to_string())))?
            .ok_or(SMTError::NotFound(String::from("node_hash does not exist")))?;

        SubTree::new(value, self.key_length, self.hasher)
    }

    fn update_subtree(
        &mut self,
        db: &mut impl DB,
        key_bin: Vec<Vec<u8>>,
        value_bin: Vec<Vec<u8>>,
        current_subtree: &SubTree,
        height: u32,
    ) -> Result<SubTree, SMTError> {
        if key_bin.len() == 0 {
            return Ok(current_subtree.clone());
        }
        let mut bin_keys = vec![];
        let mut bin_values = vec![];

        for _ in 0..self.max_number_of_nodes {
            bin_keys.push(vec![]);
            bin_values.push(vec![]);
        }

        let b = (height / 8) as usize;
        for i in 0..key_bin.len() {
            let k = key_bin[i].clone();
            let v = value_bin[i].clone();
            let bin_idx = if self.subtree_height == 4 {
                match height % 8 {
                    0 => Ok(k[b] >> 4),
                    4 => Ok(k[b] & 15),
                    _ => Err(SMTError::Unknown(String::from("Invalid bin index"))),
                }?
            // when subtree_height is 8
            } else {
                k[b]
            };
            bin_keys[bin_idx as usize].push(k);
            bin_values[bin_idx as usize].push(v);
        }

        let mut new_nodes: Vec<Node> = vec![];
        let mut new_structures: Vec<u8> = vec![];

        let mut bin_offset = 0;
        for i in 0..current_subtree.nodes.len() {
            let h = current_subtree.structure[i];
            let current_node = current_subtree.nodes[i].clone();
            let new_offset = 1 << (self.subtree_height - h as usize);

            let slice_keys = bin_keys[bin_offset..bin_offset + new_offset].to_vec();
            let slice_values = bin_values[bin_offset..bin_offset + new_offset].to_vec();
            let mut sum = 0;
            let base_length: Vec<u32> = slice_keys
                .iter()
                .map(|kb| {
                    sum += kb.len() as u32;
                    sum
                })
                .collect();

            let (nodes, heights) = self.update_node(
                db,
                slice_keys,
                slice_values,
                base_length,
                0,
                current_node,
                height,
                h,
            )?;

            new_nodes.extend(nodes);
            new_structures.extend(heights);
            bin_offset += new_offset;
        }

        if bin_offset != self.max_number_of_nodes {
            return Err(SMTError::Unknown(format!("bin_offset {} expected {}", bin_offset, self.max_number_of_nodes)));
        }
        // Go through nodes again and push up empty nodes
        let max_structure = new_structures
            .iter()
            .max()
            .ok_or(SMTError::Unknown(String::from("Invalid structure")))?;
        let mut tree_map = VecDeque::new();

        let new_subtree = calculate_subtree(
            &new_nodes,
            &new_structures,
            *max_structure,
            &mut tree_map,
            self.hasher,
        )?;
        let value = new_subtree.encode();
        db.set(new_subtree.root.clone(), value)
            .or_else(|err| Err(SMTError::Unknown(err.to_string())))?;

        Ok(new_subtree)
    }

    fn update_node(
        &mut self,
        db: &mut impl DB,
        key_bins: Vec<Vec<Vec<u8>>>,
        value_bins: Vec<Vec<Vec<u8>>>,
        length_bins: Vec<u32>,
        length_base: u32,
        current_node: Node,
        height: u32,
        h: u8,
    ) -> Result<(Vec<Node>, Vec<u8>), SMTError> {
        let total_data = length_bins[length_bins.len() - 1] - length_base;
        if total_data == 0 {
            return Ok((vec![current_node], vec![h]));
        }
        if total_data == 1 {
            let idx = length_bins
                .iter()
                .position(|&r| r == length_base + 1)
                .ok_or(SMTError::Unknown(String::from("Invalid index")))?;

            if current_node.kind == NodeKind::Empty {
                if value_bins[idx][0].len() != 0 {
                    let new_leaf =
                        Node::new_leaf(key_bins[idx][0].as_slice(), value_bins[idx][0].as_slice());
                    return Ok((vec![new_leaf], vec![h]));
                }
                return Ok((vec![current_node], vec![h]));
            }

            if current_node.kind == NodeKind::Leaf
                && utils::is_bytes_equal(&current_node.key, &key_bins[idx][0])
            {
                if value_bins[idx][0].len() != 0 {
                    let new_leaf =
                        Node::new_leaf(key_bins[idx][0].as_slice(), value_bins[idx][0].as_slice());
                    return Ok((vec![new_leaf], vec![h]));
                }
                return Ok((vec![Node::new_empty()], vec![h]));
            }
        }

        if h == self.subtree_height as u8 {
            let btm_subtree = match current_node.kind {
                NodeKind::Stub => {
                    let subtree = self.get_subtree(db, &current_node.hash)?;
                    db.del(current_node.hash)
                        .or_else(|err| Err(SMTError::Unknown(err.to_string())))?;
                    subtree
                }
                NodeKind::Empty => self.get_subtree(db, &current_node.hash)?,
                NodeKind::Leaf => SubTree::from_data(vec![0], vec![current_node], self.hasher)?,
                _ => {
                    return Err(SMTError::Unknown(String::from("invalid node type")));
                }
            };
            if key_bins.len() != 1 || value_bins.len() != 1 {
                return Err(SMTError::Unknown(String::from("invalid key/value length")));
            }
            let new_subtree = self.update_subtree(
                db,
                key_bins[0].clone(),
                value_bins[0].clone(),
                &btm_subtree,
                height + h as u32,
            )?;
            if new_subtree.nodes.len() == 1 {
                return Ok((vec![new_subtree.nodes[0].clone()], vec![h]));
            }
            let new_branch = Node::new_stub(new_subtree.root.as_slice());

            return Ok((vec![new_branch], vec![h]));
        }

        let (left_node, right_node) = match current_node.kind {
            NodeKind::Empty => (Node::new_empty(), Node::new_empty()),
            NodeKind::Leaf => {
                if utils::is_bit_set(current_node.key.as_slice(), (height + h as u32) as usize) {
                    (Node::new_empty(), current_node)
                } else {
                    (current_node, Node::new_empty())
                }
            }
            _ => {
                return Err(SMTError::Unknown(String::from("Invalid node kind")));
            }
        };
        let idx = key_bins.len() / 2;
        let (mut left_nodes, mut left_heights) = self.update_node(
            db,
            key_bins[0..idx].to_vec(),
            value_bins[0..idx].to_vec(),
            length_bins[0..idx].to_vec(),
            length_base,
            left_node,
            height,
            h + 1,
        )?;
        let (right_nodes, right_heights) = self.update_node(
            db,
            key_bins[idx..].to_vec(),
            value_bins[idx..].to_vec(),
            length_bins[idx..].to_vec(),
            length_bins[idx - 1],
            right_node,
            height,
            h + 1,
        )?;

        left_nodes.extend(right_nodes);
        left_heights.extend(right_heights);

        Ok((left_nodes, left_heights))
    }
}

pub struct InMemorySMT {
    db: smt_db::InMemorySMTDB,
    key_length: usize,
}

impl Finalize for InMemorySMT {}

type SharedInMemorySMT = JsBox<RefCell<Arc<Mutex<InMemorySMT>>>>;

impl InMemorySMT {
    pub fn js_new(mut ctx: FunctionContext) -> JsResult<SharedInMemorySMT> {
        let key_length = ctx.argument::<JsNumber>(0)?.value(&mut ctx) as usize;
        let tree = InMemorySMT {
            db: smt_db::InMemorySMTDB::new(),
            key_length: key_length,
        };

        let ref_tree = RefCell::new(Arc::new(Mutex::new(tree)));
        return Ok(ctx.boxed(ref_tree));
    }

    pub fn js_update(mut ctx: FunctionContext) -> JsResult<JsUndefined> {
        let in_memory_smt = ctx
            .this()
            .downcast_or_throw::<SharedInMemorySMT, _>(&mut ctx)?;
        let in_memory_smt = in_memory_smt.borrow().clone();

        let state_root = ctx.argument::<JsTypedArray<u8>>(0)?.as_slice(&ctx).to_vec();

        let input = ctx.argument::<JsArray>(1)?.to_vec(&mut ctx)?;
        let mut data: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for key in input.iter() {
            let obj = key.downcast_or_throw::<JsObject, _>(&mut ctx)?;
            let key = obj
                .get::<JsTypedArray<u8>, _, _>(&mut ctx, "key")?.as_slice(&ctx).to_vec();
            let value = obj
                .get::<JsTypedArray<u8>, _, _>(&mut ctx, "value")?.as_slice(&ctx).to_vec();
            data.insert(key, value);
        }

        let cb = ctx.argument::<JsFunction>(2)?.root(&mut ctx);

        let channel = ctx.channel();

        thread::spawn(move || {
            let mut update_data = UpdateData::new_from(data);
            let mut inner_smt = in_memory_smt.lock().unwrap();
            let key_length = inner_smt.key_length;

            let mut tree = SMT::new(state_root, key_length, consts::SUBTREE_SIZE);

            let result = tree.commit(&mut inner_smt.db, &mut update_data);

            channel.send(move |mut ctx| {
                let callback = cb.into_inner(&mut ctx);
                let this = ctx.undefined();
                let args: Vec<Handle<JsValue>> = match result {
                    Ok(val) => {
                        let buffer = JsBuffer::external(&mut ctx, val.to_vec());
                        vec![ctx.null().upcast(), buffer.upcast()]
                    }
                    Err(err) => vec![ctx.error(err.to_string())?.upcast()],
                };
                callback.call(&mut ctx, this, args)?;

                Ok(())
            })
        });

        Ok(ctx.undefined())
    }

    pub fn js_prove(mut ctx: FunctionContext) -> JsResult<JsUndefined> {
        let in_memory_smt = ctx
            .this()
            .downcast_or_throw::<SharedInMemorySMT, _>(&mut ctx)?;
        let in_memory_smt = in_memory_smt.borrow().clone();

        let state_root = ctx.argument::<JsTypedArray<u8>>(0)?.as_slice(&ctx).to_vec();

        let input = ctx.argument::<JsArray>(1)?.to_vec(&mut ctx)?;
        let mut data: Vec<Vec<u8>> = vec![];
        for key in input.iter() {
            let key = key.downcast_or_throw::<JsTypedArray<u8>, _>(&mut ctx)?.as_slice(&ctx).to_vec();
            data.push(key);
        }

        let cb = ctx.argument::<JsFunction>(2)?.root(&mut ctx);

        let channel = ctx.channel();

        thread::spawn(move || {
            let mut inner_smt = in_memory_smt.lock().unwrap();
            let mut tree = SMT::new(state_root, inner_smt.key_length, consts::SUBTREE_SIZE);

            let result = tree.prove(&mut inner_smt.db, data);

            channel.send(move |mut ctx| {
                let callback = cb.into_inner(&mut ctx);
                let this = ctx.undefined();
                let args: Vec<Handle<JsValue>> = match result {
                    Ok(val) => {
                        let obj: Handle<JsObject> = ctx.empty_object();
                        let sibling_hashes = ctx.empty_array();
                        for (i, h) in val.sibling_hashes.iter().enumerate() {
                            let val_res = JsBuffer::external(&mut ctx, h.to_vec());
                            sibling_hashes.set(&mut ctx, i as u32, val_res)?;
                        }
                        obj.set(&mut ctx, "siblingHashes", sibling_hashes)?;
                        let queries = ctx.empty_array();
                        for (i, v) in val.queries.iter().enumerate() {
                            let obj = ctx.empty_object();
                            let key = JsBuffer::external(&mut ctx, v.key.to_vec());
                            obj.set(&mut ctx, "key", key)?;
                            let value = JsBuffer::external(&mut ctx, v.value.to_vec());
                            obj.set(&mut ctx, "value", value)?;
                            let bitmap = JsBuffer::external(&mut ctx, v.bitmap.to_vec());
                            obj.set(&mut ctx, "bitmap", bitmap)?;

                            queries.set(&mut ctx, i as u32, obj)?;
                        }
                        vec![ctx.null().upcast(), obj.upcast()]
                    }
                    Err(err) => vec![ctx.error(err.to_string())?.upcast()],
                };
                callback.call(&mut ctx, this, args)?;

                Ok(())
            })
        });

        Ok(ctx.undefined())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smt_db;

    #[test]
    fn test_subtree() {
        let test_data = vec![
            ("05030302020303001f930f4f669738b026406a872c24db29238731868957ae1de0e5a68bb0cf7da633e508533a13da9c33fc64eb78b18bd0646c82d6316697dece0aee5a3a92e45700082e6af17a61852d01dfc18e859c20b0b974472bf6169295c36ce1380c2550e16c16babfe7d3204f61852d100f553276ad154921988de3797622091f0581884b008b647996849b70889d2a382d8fa2f42405c3bca57189de0be52c92bbc03f0cd21194ddd776cf387a81d0117b6288e6a724ec14a58cdde3c196292191da360da800ec66ad4b484153de040869f8833a30a8fcde4fdf8fcbd78d33c2fb2182dd8ffa3b311d3a72a9aec8560c56c68d665ad54c5644d40ea4fc7ed914d4eea5da3c0400e93bd78ce150412056a9076cf58977ff1a697b1932abdd52d7b978fce69186d3a9cb7274eceac6b0807ce4db0763dc596cd00e59177172de6b5dd1593b33a78500c8c4673053da259999cbc9502aef75c3c0b84bce42b1d1a2d437df88d32b737bd36e7a6410939ac431914de947353f06bbbfc31c86609ec291ed9e13b665f86a", "7a208dc2a21cb829e5fa4dc7d876bef8e52ddd23ae5ea24c2567b264bcd91a23", vec![3, 3, 2, 2, 3, 3]),
            ("02010202020049720db77a5ca853713493d4e11926b417af0cae746a305a52f555738eed47cad58c7809f5cf4119cc0f25c224f7124d15b5d62ba93bc3d948db32871026f068018dfe7dfa8fb4a5a268168638c8cce0e26f87a227320aee691f8872ed6a3aba0e", "c0fcf4b2571622905dde0884ef56d494ad3481d28fa167466f970f2c633e2925", vec![1,2,2]),
            ("0f0404040404040404040404040404040401bbacc7102a28f2eecd0e4de3c130064e653d0118b1dc4129095901f190e70034019dcb747007aca526d4b0782ed20a88a5d48a4ab6276378bada201ab5b6e4d75b01e89b7270dd0ad80207e11422bfc28f8cda8932d59b1082486fa1bf5626ea0aba01858c61150861b89516244e07cfd9d3ebcb12b2d44c2de4e7e2faed96717202eb01f9437e84b231d85f7fc2690ed54b09e85c2e0fc98b26430f10418065374e40bf0189ae2184c9a2e70656ce37c89c903b258198ad6e9db66f135780f66d8613a6fd01058c3bef2957b130622e752f0a81ee8dcf60b4685675eb88e39d5150c954fe220161543e80c5356f580f8e7e4548576486ee754ffe22f4dd122ef48e41bffc7adc01f55a1089a16835a4cbe8b5e12227575ecfd99cd951e34b409f9b2ace6f25a49701e5dfbf3ecaf909728248a751e1a75f3b626777094fe1aab03ae6f526ddac799a01f88ad8cd4aec6cc4f8d2c2bc4a5f368fc9b877685eb55673baa01d652fa4c82b0182f8fb577797274de4f48d8bd7cc5a77068ea3c60477e8552b38c926466eba1101c149d0c79bc1355d763d01690139fd187a84488d534e7e38e4772279c3826b9b01006afab486675b0e3f9b6b06283da947df6749269fb8621afe843d5df942bce7011ead1b569f80edffa2044bf9d8b8703b970ca741b821127d6da69da83b52294f01c1a9d57b050c3ba96aca78a26c5eebc76bb51acab78ce70ed3bdea1ca9143cd8", "5a2f1f740cbea0944d5182fe8ef9190d7a07e8601d0b9fc1137d48b94ce73407", vec![4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
        ];

        for (data, hash, structure) in test_data {
            let decoded_data = hex::decode(data).unwrap();
            let tree = SubTree::new(decoded_data, 32, tree_hasher).unwrap();
            let decoded_hash = hex::decode(hash).unwrap();
            assert_eq!(tree.structure, structure);
            assert_eq!(tree.root, decoded_hash);
        }
    }
    #[test]
    fn test_subtree_encode() {
        let test_data = vec![
            ("05030302020303001f930f4f669738b026406a872c24db29238731868957ae1de0e5a68bb0cf7da633e508533a13da9c33fc64eb78b18bd0646c82d6316697dece0aee5a3a92e45700082e6af17a61852d01dfc18e859c20b0b974472bf6169295c36ce1380c2550e16c16babfe7d3204f61852d100f553276ad154921988de3797622091f0581884b008b647996849b70889d2a382d8fa2f42405c3bca57189de0be52c92bbc03f0cd21194ddd776cf387a81d0117b6288e6a724ec14a58cdde3c196292191da360da800ec66ad4b484153de040869f8833a30a8fcde4fdf8fcbd78d33c2fb2182dd8ffa3b311d3a72a9aec8560c56c68d665ad54c5644d40ea4fc7ed914d4eea5da3c0400e93bd78ce150412056a9076cf58977ff1a697b1932abdd52d7b978fce69186d3a9cb7274eceac6b0807ce4db0763dc596cd00e59177172de6b5dd1593b33a78500c8c4673053da259999cbc9502aef75c3c0b84bce42b1d1a2d437df88d32b737bd36e7a6410939ac431914de947353f06bbbfc31c86609ec291ed9e13b665f86a", "7a208dc2a21cb829e5fa4dc7d876bef8e52ddd23ae5ea24c2567b264bcd91a23", vec![3, 3, 2, 2, 3, 3]),
            ("02010202020049720db77a5ca853713493d4e11926b417af0cae746a305a52f555738eed47cad58c7809f5cf4119cc0f25c224f7124d15b5d62ba93bc3d948db32871026f068018dfe7dfa8fb4a5a268168638c8cce0e26f87a227320aee691f8872ed6a3aba0e", "c0fcf4b2571622905dde0884ef56d494ad3481d28fa167466f970f2c633e2925", vec![1,2,2]),
            ("0f0404040404040404040404040404040401bbacc7102a28f2eecd0e4de3c130064e653d0118b1dc4129095901f190e70034019dcb747007aca526d4b0782ed20a88a5d48a4ab6276378bada201ab5b6e4d75b01e89b7270dd0ad80207e11422bfc28f8cda8932d59b1082486fa1bf5626ea0aba01858c61150861b89516244e07cfd9d3ebcb12b2d44c2de4e7e2faed96717202eb01f9437e84b231d85f7fc2690ed54b09e85c2e0fc98b26430f10418065374e40bf0189ae2184c9a2e70656ce37c89c903b258198ad6e9db66f135780f66d8613a6fd01058c3bef2957b130622e752f0a81ee8dcf60b4685675eb88e39d5150c954fe220161543e80c5356f580f8e7e4548576486ee754ffe22f4dd122ef48e41bffc7adc01f55a1089a16835a4cbe8b5e12227575ecfd99cd951e34b409f9b2ace6f25a49701e5dfbf3ecaf909728248a751e1a75f3b626777094fe1aab03ae6f526ddac799a01f88ad8cd4aec6cc4f8d2c2bc4a5f368fc9b877685eb55673baa01d652fa4c82b0182f8fb577797274de4f48d8bd7cc5a77068ea3c60477e8552b38c926466eba1101c149d0c79bc1355d763d01690139fd187a84488d534e7e38e4772279c3826b9b01006afab486675b0e3f9b6b06283da947df6749269fb8621afe843d5df942bce7011ead1b569f80edffa2044bf9d8b8703b970ca741b821127d6da69da83b52294f01c1a9d57b050c3ba96aca78a26c5eebc76bb51acab78ce70ed3bdea1ca9143cd8", "5a2f1f740cbea0944d5182fe8ef9190d7a07e8601d0b9fc1137d48b94ce73407", vec![4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4]),
        ];

        for (data, _, _) in test_data {
            let decoded_data = hex::decode(data).unwrap();
            let tree = SubTree::new(decoded_data.clone(), 32, tree_hasher).unwrap();
            assert_eq!(tree.encode(), decoded_data.clone());
        }
    }

    #[test]
    fn test_empty_tree() {
        let mut tree = SMT::new(vec![], 32, 8);
        let mut data = UpdateData {
            data: HashMap::new(),
        };
        let mut db = smt_db::InMemorySMTDB::new();
        let result = tree.commit(&mut db, &mut data);

        assert_eq!(
            result.unwrap(),
            hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap()
        );
    }

    #[test]
    fn test_small_tree_0() {
        let test_data = vec![(
            vec![
                "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
            ],
            vec![
                "1406e05881e299367766d313e26c05564ec91bf721d31726bd6e46e60689539a",
            ],
            "ccd1c136c75ffd2e3947466ad17dd6687d890ce50cbeb7ca7a4da638df482b96",
        )];

        for (keys, values, root) in test_data {
            let mut tree = SMT::new(vec![], 32, 8);
            let mut data = UpdateData {
                data: HashMap::new(),
            };
            for idx in 0..keys.len() {
                data.data.insert(
                    hex::decode(keys[idx]).unwrap(),
                    hex::decode(values[idx]).unwrap(),
                );
            }
            let mut db = smt_db::InMemorySMTDB::new();
            let result = tree.commit(&mut db, &mut data);

            assert_eq!(result.unwrap(), hex::decode(root).unwrap());
        }
    }

    #[test]
    fn test_small_tree_1() {
        let test_data = vec![(
            vec![
                "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a",
                "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
            ],
            vec![
                "9c12cfdc04c74584d787ac3d23772132c18524bc7ab28dec4219b8fc5b425f70",
                "1406e05881e299367766d313e26c05564ec91bf721d31726bd6e46e60689539a",
            ],
            "6d13bfad2a210dc084b9a896f79243d58c7fbd2721181b86cdaed00af349f429",
        )];

        for (keys, values, root) in test_data {
            let mut tree = SMT::new(vec![], 32, 8);
            let mut data = UpdateData {
                data: HashMap::new(),
            };
            for idx in 0..keys.len() {
                data.data.insert(
                    hex::decode(keys[idx]).unwrap(),
                    hex::decode(values[idx]).unwrap(),
                );
            }
            let mut db = smt_db::InMemorySMTDB::new();
            let result = tree.commit(&mut db, &mut data);

            assert_eq!(result.unwrap(), hex::decode(root).unwrap());
        }
    }

    #[test]
    fn test_small_tree_2() {
        let test_data = vec![(
            vec![
                "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a",
                "e52d9c508c502347344d8c07ad91cbd6068afc75ff6292f062a09ca381c89e71",
                "e77b9a9ae9e30b0dbdb6f510a264ef9de781501d7b6b92ae89eb059c5ab743db",
                "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986",
                "084fed08b978af4d7d196a7446a86b58009e636b611db16211b65a9aadff29c5",
                "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
            ],
            vec![
                "9c12cfdc04c74584d787ac3d23772132c18524bc7ab28dec4219b8fc5b425f70",
                "214e63bf41490e67d34476778f6707aa6c8d2c8dccdf78ae11e40ee9f91e89a7",
                "88e443a340e2356812f72e04258672e5b287a177b66636e961cbc8d66b1e9b97",
                "1cc3adea40ebfd94433ac004777d68150cce9db4c771bc7de1b297a7b795bbba",
                "c942a06c127c2c18022677e888020afb174208d299354f3ecfedb124a1f3fa45",
                "1406e05881e299367766d313e26c05564ec91bf721d31726bd6e46e60689539a",
            ],
            "d336d7a29ec55728822a2f9ec6aae3bee549e743d50469d7fe924914348ff758",
        )];

        for (keys, values, root) in test_data {
            let mut tree = SMT::new(vec![], 32, 8);
            let mut data = UpdateData {
                data: HashMap::new(),
            };
            for idx in 0..keys.len() {
                data.data.insert(
                    hex::decode(keys[idx]).unwrap(),
                    hex::decode(values[idx]).unwrap(),
                );
            }
            let mut db = smt_db::InMemorySMTDB::new();
            let result = tree.commit(&mut db, &mut data);

            assert_eq!(result.unwrap(), hex::decode(root).unwrap());
        }
    }

    #[test]
    fn test_small_tree_3() {
        let test_data = vec![(
            vec![
                "ca358758f6d27e6cf45272937977a748fd88391db679ceda7dc7bf1f005ee879",
                "e77b9a9ae9e30b0dbdb6f510a264ef9de781501d7b6b92ae89eb059c5ab743db",
                "084fed08b978af4d7d196a7446a86b58009e636b611db16211b65a9aadff29c5",
                "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986",
                "e52d9c508c502347344d8c07ad91cbd6068afc75ff6292f062a09ca381c89e71",
                "beead77994cf573341ec17b58bbf7eb34d2711c993c1d976b128b3188dc1829a",
                "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a",
                "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
                "67586e98fad27da0b9968bc039a1ef34c939b9b8e523a8bef89d478608c5ecf6",
                "2b4c342f5433ebe591a1da77e013d1b72475562d48578dca8b84bac6651c3cb9",
            ],
            vec![
                "b6d58dfa6547c1eb7f0d4ffd3e3bd6452213210ea51baa70b97c31f011187215",
                "88e443a340e2356812f72e04258672e5b287a177b66636e961cbc8d66b1e9b97",
                "c942a06c127c2c18022677e888020afb174208d299354f3ecfedb124a1f3fa45",
                "1cc3adea40ebfd94433ac004777d68150cce9db4c771bc7de1b297a7b795bbba",
                "214e63bf41490e67d34476778f6707aa6c8d2c8dccdf78ae11e40ee9f91e89a7",
                "42bbafcdee807bf0e14577e5fa6ed1bc0cd19be4f7377d31d90cd7008cb74d73",
                "9c12cfdc04c74584d787ac3d23772132c18524bc7ab28dec4219b8fc5b425f70",
                "1406e05881e299367766d313e26c05564ec91bf721d31726bd6e46e60689539a",
                "f3035c79a84a2dda7a7b5f356b3aeb82fb934d5f126af99bbee9a404c425b888",
                "2ad16b189b68e7672a886c82a0550bc531782a3a4cfb2f08324e316bb0f3174d",
            ],
            "3f91f1b7bc96933102dcce6a6c9200c68146a8327c16b91f8e4b37f40e2e2fb4",
        )];

        for (keys, values, root) in test_data {
            let mut tree = SMT::new(vec![], 32, 8);
            let mut data = UpdateData {
                data: HashMap::new(),
            };
            for idx in 0..keys.len() {
                data.data.insert(
                    hex::decode(keys[idx]).unwrap(),
                    hex::decode(values[idx]).unwrap(),
                );
            }
            let mut db = smt_db::InMemorySMTDB::new();
            let result = tree.commit(&mut db, &mut data);

            assert_eq!(result.unwrap(), hex::decode(root).unwrap());
        }
    }
}
