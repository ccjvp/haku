use crate::{args::Args, bitmap::Bitmap, io::IO, tokenizer::tokenize};
use anyhow::{Result, ensure};
use bytemuck::{CheckedBitPattern, NoUninit, bytes_of, checked::try_pod_read_unaligned};
use parking_lot::{Mutex, RwLock};
use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, AtomicUsize},
    },
    time::Instant,
};
use tokio::fs;

#[derive(Clone, Copy, Default)]
struct ScalarLeaf {
    value: u32,
}

impl ScalarLeaf {
    fn new(value: u32) -> Self {
        Self { value }
    }
}

#[derive(Clone, Copy, Debug, CheckedBitPattern, NoUninit)]
#[repr(C)]
struct TermHeader {
    doc_count: u32,
    postings: NodeRef,
    offset: u16,
    len: u8,
    _pad: u8,
}

#[derive(Clone, Copy)]
struct TermLeaf {
    count: u16,
    suffix_start: u16,
    // Term headers grow from the start of `data` and
    // suffixes which complete the terms from the back
    data: [u8; 4092],
}

impl TermLeaf {
    fn new(count: u16, suffix_start: u16, data: [u8; 4092]) -> Self {
        Self {
            count,
            suffix_start,
            data,
        }
    }
}

impl Default for TermLeaf {
    fn default() -> Self {
        Self::new(0, 4092, [0; 4092])
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, CheckedBitPattern, NoUninit, Default)]
#[repr(u8)]
enum NodeKind {
    #[default]
    Empty,
    Node4,
    Node16,
    Node48,
    Node256,
    TermLeaf,
    ScalarLeaf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, CheckedBitPattern, NoUninit, Default)]
#[repr(C)]
struct NodeRef {
    buf_id: u32,
    slot: u16,
    kind: NodeKind,
    _pad: u8,
}

#[derive(Clone, Copy)]
struct Edge {
    node_ref: NodeRef,
    key: u8,
}

impl Edge {
    fn new(node_ref: NodeRef, key: u8) -> Self {
        Self { node_ref, key }
    }
}

impl NodeRef {
    fn new(kind: NodeKind, buf_id: u32, slot: u16) -> Self {
        Self {
            buf_id,
            kind,
            slot,
            _pad: 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Node4 {
    count: u8,
    prefix: [u8; 7],
    prefix_len: u8,
    keys: [u8; 4],
    children: [NodeRef; 4],
    postings: NodeRef,
    doc_count: u32,
}

impl Node4 {
    fn with_prefix(prefix: &[u8]) -> Self {
        assert!(prefix.len() <= 7);

        let mut node = Self::default();
        node.prefix[..prefix.len()].copy_from_slice(prefix);
        node.prefix_len = prefix.len() as u8;
        node
    }

    fn new(
        count: u8,
        keys: [u8; 4],
        children: [NodeRef; 4],
        postings: NodeRef,
        doc_count: u32,
    ) -> Self {
        Self {
            count,
            prefix: [0; 7],
            prefix_len: 0,
            keys,
            children,
            postings,
            doc_count,
        }
    }

    fn child_for(&self, key: u8) -> Option<NodeRef> {
        for i in 0..self.count as usize {
            if self.keys[i] == key {
                return Some(self.children[i]);
            }
        }
        None
    }

    fn insert_child(&mut self, key: u8, child: NodeRef) -> Result<()> {
        let count = self.count as usize;
        ensure!(count < self.keys.len());
        ensure!(self.child_for(key).is_none());
        self.keys[count] = key;
        self.children[count] = child;
        self.count += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct Node16 {
    count: u8,
    prefix: [u8; 7],
    prefix_len: u8,
    keys: [u8; 16],
    children: [NodeRef; 16],
    postings: NodeRef,
    doc_count: u32,
}

impl Node16 {
    fn from_node4(node: Node4) -> Self {
        let count = node.count as usize;

        let mut keys = [0; 16];
        keys[..count].copy_from_slice(&node.keys[..count]);

        let mut children = [NodeRef::default(); 16];
        children[..count as usize].copy_from_slice(&node.children[..count]);

        Self {
            count: node.count,
            prefix: node.prefix,
            prefix_len: node.prefix_len,
            keys,
            children,
            postings: node.postings,
            doc_count: node.doc_count,
        }
    }

    fn new(
        count: u8,
        keys: [u8; 16],
        children: [NodeRef; 16],
        postings: NodeRef,
        doc_count: u32,
    ) -> Self {
        Self {
            count,
            prefix: [0; 7],
            prefix_len: 0,
            keys,
            children,
            postings,
            doc_count,
        }
    }

    fn child_for(&self, key: u8) -> Option<NodeRef> {
        for i in 0..self.count as usize {
            if self.keys[i] == key {
                return Some(self.children[i]);
            }
        }
        None
    }

    fn insert_child(&mut self, key: u8, child: NodeRef) -> Result<()> {
        let count = self.count as usize;

        ensure!(count < self.keys.len());
        ensure!(self.child_for(key).is_none());

        self.keys[count] = key;
        self.children[count] = child;
        self.count += 1;

        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Node48 {
    count: u8,
    prefix: [u8; 7],
    prefix_len: u8,
    keys: [u8; 256],
    children: [NodeRef; 48],
    postings: NodeRef,
    doc_count: u32,
}

impl Node48 {
    fn from_node16(node: Node16) -> Self {
        let count = node.count as usize;

        let mut keys = [u8::MAX; 256];
        let mut children = [NodeRef::default(); 48];
        for i in 0..count {
            let key = node.keys[i] as usize;
            keys[key] = i as u8;
            children[i] = node.children[i];
        }

        Self {
            count: node.count,
            prefix: node.prefix,
            prefix_len: node.prefix_len,
            keys,
            children,
            postings: node.postings,
            doc_count: node.doc_count,
        }
    }

    fn new(
        count: u8,
        keys: [u8; 256],
        children: [NodeRef; 48],
        postings: NodeRef,
        doc_count: u32,
    ) -> Self {
        Self {
            count,
            prefix: [0; 7],
            prefix_len: 0,
            keys,
            children,
            postings,
            doc_count,
        }
    }

    fn child_for(&self, key: u8) -> Option<NodeRef> {
        let child_i = self.keys[key as usize];
        if child_i != u8::MAX {
            Some(self.children[child_i as usize])
        } else {
            None
        }
    }

    fn insert_child(&mut self, key: u8, child: NodeRef) -> Result<()> {
        let child_index = self.count as usize;

        ensure!(child_index < self.children.len());
        ensure!(self.keys[key as usize] == u8::MAX);

        self.keys[key as usize] = child_index as u8;
        self.children[child_index] = child;
        self.count += 1;
        Ok(())
    }
}

impl Default for Node48 {
    fn default() -> Self {
        Self::new(
            0,
            [u8::MAX; 256],
            [NodeRef::default(); 48],
            NodeRef::default(),
            0,
        )
    }
}

#[derive(Clone, Copy)]
struct Node256 {
    count: u16,
    prefix: [u8; 7],
    prefix_len: u8,
    keys: [u8; 256],
    children: [NodeRef; 256],
    postings: NodeRef,
    doc_count: u32,
}

impl Node256 {
    fn from_node48(node: Node48) -> Self {
        let mut keys = [0; 256];
        let mut children = [NodeRef::default(); 256];

        for (byte, key) in node.keys.iter().copied().enumerate() {
            if key != u8::MAX {
                children[byte] = node.children[key as usize];
                keys[byte] = byte as u8;
            }
        }

        Self {
            count: u16::from(node.count),
            prefix: node.prefix,
            prefix_len: node.prefix_len,
            keys,
            children,
            postings: node.postings,
            doc_count: node.doc_count,
        }
    }

    fn new(
        count: u16,
        keys: [u8; 256],
        children: [NodeRef; 256],
        postings: NodeRef,
        doc_count: u32,
    ) -> Self {
        Self {
            count,
            prefix: [0; 7],
            prefix_len: 0,
            keys,
            children,
            postings,
            doc_count,
        }
    }

    #[inline(always)]
    fn child_for(&self, key: u8) -> Option<NodeRef> {
        let child = self.children[key as usize];
        (child.kind != NodeKind::Empty).then_some(child)
    }

    fn insert_child(&mut self, key: u8, child: NodeRef) -> Result<()> {
        let slot = key as usize;

        ensure!(self.children[slot].kind == NodeKind::Empty);

        self.children[slot] = child;
        self.keys[slot] = key;
        self.count += 1;
        Ok(())
    }
}

impl Default for Node256 {
    fn default() -> Self {
        Self::new(
            0,
            [0; 256],
            [NodeRef::default(); 256],
            NodeRef::default(),
            0,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum BufState {
    OnDisk,
    _InMemory,
    Dirty,
}

#[repr(C)]
struct Buf<T: Copy, const N: usize, const B: usize> {
    slots: [T; N],
    free: Bitmap<B>,
    state: BufState,
}

impl<T: Copy + Default, const N: usize, const B: usize> Buf<T, N, B> {
    fn new() -> Self {
        assert!(N.div_ceil(64) == B);

        Self {
            state: BufState::OnDisk,
            slots: [T::default(); N],
            free: Bitmap::new(true, N),
        }
    }

    fn get(&self, slot: u16) -> &T {
        &self.slots[slot as usize]
    }

    fn get_mut(&mut self, slot: u16) -> &mut T {
        &mut self.slots[slot as usize]
    }
}

struct Allocator<T: Copy, const N: usize, const B: usize> {
    bufs: HashMap<u32, Buf<T, N, B>>,
    free: Vec<u32>,
    kind: NodeKind,
}

impl<T: Copy + Default, const N: usize, const B: usize> Allocator<T, N, B> {
    fn new(kind: NodeKind) -> Self {
        Self {
            bufs: HashMap::new(),
            free: vec![],
            kind,
        }
    }

    fn get(&self, node_ref: NodeRef) -> Result<&T> {
        let buf = self.bufs.get(&node_ref.buf_id);
        Ok(buf.unwrap().get(node_ref.slot))
    }

    fn get_mut(&mut self, node_ref: NodeRef) -> Result<&mut T> {
        ensure!(node_ref.kind == self.kind);
        let buf = self.bufs.get_mut(&node_ref.buf_id).unwrap();
        ensure!(!buf.free.get(node_ref.slot as usize));
        buf.state = BufState::Dirty;
        Ok(buf.get_mut(node_ref.slot))
    }

    fn allocate(&mut self, bm25: &BM25) -> Result<u32> {
        let buf_id = bm25.get_buf_id();

        ensure!(!self.bufs.contains_key(&buf_id));

        self.bufs.insert(buf_id, Buf::new());
        Ok(buf_id)
    }

    fn update(&mut self, node_ref: NodeRef, node: &T) -> Result<NodeRef> {
        ensure!(node_ref.kind == self.kind);

        let buf = self.bufs.get_mut(&node_ref.buf_id).unwrap();
        let slot = node_ref.slot as usize;

        ensure!(!buf.free.get(slot));

        buf.slots[slot] = *node;
        buf.state = BufState::Dirty;
        Ok(node_ref)
    }

    fn remove(&mut self, node_ref: NodeRef) -> Result<()> {
        ensure!(node_ref.kind == self.kind);

        let buf = self.bufs.get_mut(&node_ref.buf_id).unwrap();
        let slot = node_ref.slot as usize;

        ensure!(!buf.free.get(slot));

        let was_full = buf.free.words.iter().all(|&word| word == 0);

        buf.slots[slot] = T::default();
        buf.free.set(slot, true);
        buf.state = BufState::Dirty;

        if was_full {
            self.free.push(node_ref.buf_id);
        }

        Ok(())
    }

    fn add(&mut self, node: T, bm25: &BM25) -> Result<NodeRef> {
        let mut slot = None;

        if let Some(buf_id) = self.free.pop() {
            if let Some(buf) = self.bufs.get(&buf_id) {
                for (i, w) in buf.free.words.iter().enumerate() {
                    if *w == 0 {
                        continue;
                    }

                    let j = i * 64 + w.trailing_zeros() as usize;

                    if buf.free.get(j) {
                        slot = Some((buf_id, j));
                        break;
                    }
                }
            }
        }

        let (buf_id, slot) = match slot {
            Some(slot) => slot,
            None => (self.allocate(bm25)?, 0),
        };

        let buf = match self.bufs.get_mut(&buf_id) {
            Some(buf) => buf,
            None => unreachable!(),
        };

        buf.state = BufState::Dirty;
        buf.slots[slot] = node;
        buf.free.set(slot, false);

        // TODO: Cache if the bitmap has space?
        if buf.free.words.iter().any(|&word| word != 0) {
            self.free.push(buf_id);
        }

        Ok(NodeRef::new(self.kind, buf_id, slot as u16))
    }
}

#[repr(C)]
#[derive(Clone, Copy, CheckedBitPattern, NoUninit)]
struct Header {
    doc_count: u32,
    token_count: u32,
    version: u64,
    term_root: NodeRef,
    doc_root: NodeRef,
}

impl Header {
    fn new(term_root: NodeRef, doc_root: NodeRef) -> Self {
        Self {
            doc_count: 0,
            token_count: 0,
            version: 0,
            term_root,
            doc_root,
        }
    }
}

pub struct BM25 {
    header: RwLock<Header>,
    to_remove: Mutex<Vec<NodeRef>>,
    update_guard: tokio::sync::Mutex<()>,
    active_readers: AtomicUsize,
    buf_id: AtomicU32,
    doc_id: AtomicU64,
    node4: RwLock<Allocator<Node4, 4946, 78>>,
    node16: RwLock<Allocator<Node16, 1600, 25>>,
    node48: RwLock<Allocator<Node48, 384, 6>>,
    node256: RwLock<Allocator<Node256, 112, 2>>,
    terms: RwLock<Allocator<TermLeaf, 48, 1>>,
    scalars: RwLock<Allocator<ScalarLeaf, 52416, 819>>,
}

impl BM25 {
    pub async fn open(io: Arc<IO>, args: &Args) -> Result<Self> {
        let path = args.bm25_path.to_str().unwrap();
        let handle = io.get(path).await;

        let mut bm25 = Self {
            header: RwLock::new(Header::new(NodeRef::default(), NodeRef::default())),
            to_remove: Mutex::new(Vec::new()),
            update_guard: tokio::sync::Mutex::new(()),
            active_readers: AtomicUsize::new(0),
            buf_id: AtomicU32::new(0),
            doc_id: AtomicU64::new(0),
            terms: RwLock::new(Allocator::new(NodeKind::TermLeaf)),
            scalars: RwLock::new(Allocator::new(NodeKind::ScalarLeaf)),
            node4: RwLock::new(Allocator::new(NodeKind::Node4)),
            node16: RwLock::new(Allocator::new(NodeKind::Node16)),
            node48: RwLock::new(Allocator::new(NodeKind::Node48)),
            node256: RwLock::new(Allocator::new(NodeKind::Node256)),
        };

        let term_root = bm25.terms.write().add(TermLeaf::default(), &bm25)?;
        let doc_root = NodeRef::default();

        let header: Header = match handle {
            Ok(handle) => {
                let b = io.read(handle).await?;
                try_pod_read_unaligned(&b).unwrap()
            }
            Err(_) => {
                let header = Header::new(term_root, doc_root);
                io.write(path, bytes_of(&header)).await?;

                header
            }
        };

        *bm25.header.get_mut() = header;
        Ok(bm25)
    }

    fn get_doc_id(&self) -> u64 {
        self.doc_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn get_buf_id(&self) -> u32 {
        self.buf_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn _write(&self, _path: &str) -> Result<()> {
        todo!("Save the index")
    }

    fn get_prefix(&self, node_ref: NodeRef) -> Result<([u8; 7], usize)> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let n4 = self.node4.read();
                let node = n4.get(node_ref)?;
                Ok((node.prefix, node.prefix_len as usize))
            }
            NodeKind::Node16 => {
                let nodes = self.node16.read();
                let node = nodes.get(node_ref)?;
                Ok((node.prefix, node.prefix_len as usize))
            }
            NodeKind::Node48 => {
                let nodes = self.node48.read();
                let node = nodes.get(node_ref)?;
                Ok((node.prefix, node.prefix_len as usize))
            }
            NodeKind::Node256 => {
                let nodes = self.node256.read();
                let node = nodes.get(node_ref)?;
                Ok((node.prefix, node.prefix_len as usize))
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                anyhow::bail!("cannot get a prefix from a non-internal node")
            }
        }
    }

    // In place is ok as this is ever done before attaching the node
    fn set_prefix(&self, node_ref: NodeRef, prefix: &[u8]) -> Result<()> {
        ensure!(prefix.len() <= 7, "prefix exceeds the node prefix capacity");

        let mut padded_prefix = [0; 7];
        padded_prefix[..prefix.len()].copy_from_slice(prefix);

        match node_ref.kind {
            NodeKind::Node4 => {
                let mut nodes = self.node4.write();
                let node = nodes.get_mut(node_ref)?;
                node.prefix = padded_prefix;
                node.prefix_len = prefix.len() as u8;
            }
            NodeKind::Node16 => {
                let mut nodes = self.node16.write();
                let node = nodes.get_mut(node_ref)?;
                node.prefix = padded_prefix;
                node.prefix_len = prefix.len() as u8;
            }
            NodeKind::Node48 => {
                let mut nodes = self.node48.write();
                let node = nodes.get_mut(node_ref)?;
                node.prefix = padded_prefix;
                node.prefix_len = prefix.len() as u8;
            }
            NodeKind::Node256 => {
                let mut nodes = self.node256.write();
                let node = nodes.get_mut(node_ref)?;
                node.prefix = padded_prefix;
                node.prefix_len = prefix.len() as u8;
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                anyhow::bail!("cannot set a prefix on a non-internal node")
            }
        }

        Ok(())
    }

    fn copy_with_prefix(&self, node_ref: NodeRef, prefix: &[u8]) -> Result<NodeRef> {
        ensure!(prefix.len() <= 7, "prefix exceeds the node prefix capacity");

        match node_ref.kind {
            NodeKind::Node4 => {
                let mut node = {
                    let n4 = self.node4.read();
                    n4.get(node_ref)?.clone()
                };

                node.prefix = [0; 7];
                node.prefix[..prefix.len()].copy_from_slice(prefix);
                node.prefix_len = prefix.len() as u8;
                Ok(self.node4.write().add(node, self)?)
            }
            NodeKind::Node16 => {
                let mut node = {
                    let n16 = self.node16.read();
                    n16.get(node_ref)?.clone()
                };

                node.prefix = [0; 7];
                node.prefix[..prefix.len()].copy_from_slice(prefix);
                node.prefix_len = prefix.len() as u8;
                Ok(self.node16.write().add(node, self)?)
            }
            NodeKind::Node48 => {
                let mut node = {
                    let n48 = self.node48.read();
                    n48.get(node_ref)?.clone()
                };

                node.prefix = [0; 7];
                node.prefix[..prefix.len()].copy_from_slice(prefix);
                node.prefix_len = prefix.len() as u8;
                Ok(self.node48.write().add(node, self)?)
            }
            NodeKind::Node256 => {
                let mut node = {
                    let n256 = self.node256.read();
                    n256.get(node_ref)?.clone()
                };

                node.prefix = [0; 7];
                node.prefix[..prefix.len()].copy_from_slice(prefix);
                node.prefix_len = prefix.len() as u8;
                Ok(self.node256.write().add(node, self)?)
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                anyhow::bail!("cannot copy a prefix onto a non-internal node")
            }
        }
    }

    fn split_prefix(
        &self,
        node_ref: NodeRef,
        prefix: &[u8],
        prefix_len: usize,
        bytes: &[u8],
        byte_pos: usize,
        split_at: usize,
    ) -> Result<(NodeRef, NodeRef)> {
        let new_ref = {
            let mut n4 = self.node4.write();
            let range = byte_pos + split_at + 1..bytes.len() - 1;
            n4.add(Node4::with_prefix(&bytes[range]), self)?
        };

        let bottom_ref = self.copy_with_prefix(node_ref, &prefix[split_at + 1..prefix_len])?;

        let mut branch_ref = {
            let mut n4 = self.node4.write();
            n4.add(Node4::with_prefix(&prefix[..split_at]), self)?
        };

        let to_bottom = Edge::new(branch_ref, prefix[split_at]);
        branch_ref = self.insert_child(to_bottom, bottom_ref)?;

        let to_new = Edge::new(branch_ref, bytes[byte_pos + split_at]);
        branch_ref = self.insert_child(to_new, new_ref)?;

        Ok((branch_ref, new_ref))
    }

    fn get_node_postings(&self, node_ref: NodeRef) -> anyhow::Result<(NodeRef, u32)> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let nodes = self.node4.read();
                let node = nodes.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Node16 => {
                let nodes = self.node16.read();
                let node = nodes.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Node48 => {
                let nodes = self.node48.read();
                let node = nodes.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Node256 => {
                let nodes = self.node256.read();
                let node = nodes.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                Ok((NodeRef::default(), 0))
            }
        }
    }

    fn set_postings(
        &self,
        node_ref: NodeRef,
        postings: NodeRef,
        doc_count: u32,
    ) -> anyhow::Result<()> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write();
                let node = node4.get_mut(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                Ok(())
            }
            NodeKind::Node16 => {
                let mut node16 = self.node16.write();
                let node = node16.get_mut(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                Ok(())
            }
            NodeKind::Node48 => {
                let mut node48 = self.node48.write();
                let node = node48.get_mut(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                Ok(())
            }
            NodeKind::Node256 => {
                let mut node256 = self.node256.write();
                let node = node256.get_mut(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                Ok(())
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                unreachable!()
            }
        }
    }

    fn child_for(&self, edge: Edge) -> anyhow::Result<Option<NodeRef>> {
        let Edge { node_ref, key } = edge;

        match node_ref.kind {
            NodeKind::Node4 => {
                let nodes = self.node4.read();
                let node = nodes.get(node_ref)?;
                Ok(node.child_for(key))
            }
            NodeKind::Node16 => {
                let nodes = self.node16.read();
                let node = nodes.get(node_ref)?;
                Ok(node.child_for(key))
            }
            NodeKind::Node48 => {
                let nodes = self.node48.read();
                let node = nodes.get(node_ref)?;
                Ok(node.child_for(key))
            }
            NodeKind::Node256 => {
                let nodes = self.node256.read();
                let node = nodes.get(node_ref)?;
                Ok(node.child_for(key))
            }
            _ => unreachable!(),
        }
    }

    fn update_node(&self, edge: Edge, child_ref: NodeRef) -> anyhow::Result<()> {
        let Edge { node_ref, key } = edge;

        match node_ref.kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write();
                let node = node4.get_mut(node_ref)?;
                node.insert_child(key, child_ref)?;
                Ok(())
            }
            NodeKind::Node16 => {
                let mut node16 = self.node16.write();
                let node = node16.get_mut(node_ref)?;
                node.insert_child(key, child_ref)?;
                Ok(())
            }
            NodeKind::Node48 => {
                let mut node48 = self.node48.write();
                let node = node48.get_mut(node_ref)?;
                node.insert_child(key, child_ref)?;
                Ok(())
            }
            NodeKind::Node256 => {
                let mut node256 = self.node256.write();
                let node = node256.get_mut(node_ref)?;
                node.insert_child(key, child_ref)?;
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    fn insert_node(&self, edge: Edge, node_ref: NodeRef) -> anyhow::Result<()> {
        let Edge {
            node_ref: parent_ref,
            key,
        } = edge;

        match parent_ref.kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write();
                let node = node4.get_mut(parent_ref)?;
                for i in 0..node.count as usize {
                    if node.keys[i] == key {
                        node.children[i] = node_ref;
                        return Ok(());
                    }
                }
                unreachable!()
            }
            NodeKind::Node16 => {
                let mut node16 = self.node16.write();
                let node = node16.get_mut(parent_ref)?;
                for i in 0..node.count as usize {
                    if node.keys[i] == key {
                        node.children[i] = node_ref;
                        return Ok(());
                    }
                }

                unreachable!()
            }
            NodeKind::Node48 => {
                let mut node48 = self.node48.write();
                let node = node48.get_mut(parent_ref)?;
                let key_index = node.keys[key as usize];

                ensure!(key_index != u8::MAX);

                node.children[key_index as usize] = node_ref;
                Ok(())
            }
            NodeKind::Node256 => {
                let mut node256 = self.node256.write();
                let node = node256.get_mut(parent_ref)?;
                let slot = key as usize;

                ensure!(node.children[slot].kind != NodeKind::Empty);

                node.children[slot] = node_ref;
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    fn is_full(&self, node_ref: NodeRef) -> anyhow::Result<bool> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let nodes = self.node4.read();
                let node = nodes.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            NodeKind::Node16 => {
                let nodes = self.node16.read();
                let node = nodes.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            NodeKind::Node48 => {
                let nodes = self.node48.read();
                let node = nodes.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            NodeKind::Node256 => {
                let nodes = self.node256.read();
                let node = nodes.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            _ => unreachable!(),
        }
    }

    fn insert_child(&self, mut edge: Edge, child_ref: NodeRef) -> anyhow::Result<NodeRef> {
        anyhow::ensure!(self.child_for(edge)?.is_none());

        if self.is_full(edge.node_ref)? {
            edge.node_ref = self.grow_node(edge.node_ref)?;
        }

        self.update_node(edge, child_ref)?;
        Ok(edge.node_ref)
    }

    fn grow_node(&self, node_ref: NodeRef) -> Result<NodeRef> {
        let grown_ref = match node_ref.kind {
            NodeKind::Node4 => {
                let nodes = self.node4.read();
                let node = nodes.get(node_ref)?;
                let grown = Node16::from_node4(*node);
                self.node16.write().add(grown, self)?
            }
            NodeKind::Node16 => {
                let nodes = self.node16.read();
                let node = nodes.get(node_ref)?;
                let grown = Node48::from_node16(*node);
                self.node48.write().add(grown, self)?
            }
            NodeKind::Node48 => {
                let nodes = self.node48.read();
                let node = nodes.get(node_ref)?;
                let grown = Node256::from_node48(*node);
                self.node256.write().add(grown, self)?
            }
            _ => unreachable!(),
        };

        Ok(grown_ref)
    }

    fn split_term_leaf(&self, leaf_ref: NodeRef) -> Result<NodeRef> {
        ensure!(leaf_ref.kind == NodeKind::TermLeaf);

        let leaf = *self.terms.read().get(leaf_ref)?;

        let mut partitions = HashMap::new();
        let mut postings = NodeRef::default();
        let mut doc_count = 0;
        for index in 0..leaf.count as usize {
            let mut header = self.read_term_header(&leaf, index)?;
            let start = header.offset as usize;
            let end = start + header.len as usize;

            ensure!(end <= leaf.data.len());

            let suffix = &leaf.data[start..end];
            let Some((&key, suffix)) = suffix.split_first() else {
                postings = header.postings;
                doc_count = header.doc_count;
                continue;
            };

            let child = partitions.entry(key).or_insert_with(TermLeaf::default);
            let count = child.count as usize;
            let data_start = child.suffix_start as usize - suffix.len();
            let data_end = data_start + suffix.len();
            let header_len = size_of::<TermHeader>();

            ensure!((count + 1) * header_len <= data_start);

            child.data[data_start..data_end].copy_from_slice(suffix);
            child.suffix_start = data_start as u16;
            header.offset = data_start as u16;
            header.len = suffix.len() as u8;

            let header_start = count * header_len;
            let header_end = header_start + header_len;
            let header_b = bytes_of(&header);

            child.data[header_start..header_end].copy_from_slice(header_b);
            child.count += 1;
        }

        ensure!(partitions.len() < u8::MAX as usize);

        let mut children = Vec::with_capacity(partitions.len());
        {
            let mut terms = self.terms.write();
            for (key, child) in partitions {
                let child_ref = terms.add(child, self)?;
                children.push((key, child_ref));
            }
        }

        let count = children.len() as u8;
        match count {
            0..=4 => {
                let mut keys = [0; 4];
                let mut child_slots = [NodeRef::default(); 4];

                for (index, (key, child_ref)) in children.into_iter().enumerate() {
                    keys[index] = key;
                    child_slots[index] = child_ref;
                }

                let n4 = Node4::new(count, keys, child_slots, postings, doc_count);
                self.node4.write().add(n4, self)
            }
            5..=16 => {
                let mut keys = [0; 16];
                let mut child_slots = [NodeRef::default(); 16];

                for (index, (key, child_ref)) in children.into_iter().enumerate() {
                    keys[index] = key;
                    child_slots[index] = child_ref;
                }

                let n16 = Node16::new(count, keys, child_slots, postings, doc_count);
                self.node16.write().add(n16, self)
            }
            17..=48 => {
                let mut keys = [u8::MAX; 256];
                let mut child_slots = [NodeRef::default(); 48];

                for (index, (key, child_ref)) in children.into_iter().enumerate() {
                    keys[key as usize] = index as u8;
                    child_slots[index] = child_ref;
                }

                let n48 = Node48::new(count, keys, child_slots, postings, doc_count);
                self.node48.write().add(n48, self)
            }
            49..=254 => {
                let mut keys = [0; 256];
                let mut child_slots = [NodeRef::default(); 256];

                for (key, child_ref) in children {
                    keys[key as usize] = key;
                    child_slots[key as usize] = child_ref;
                }

                let n256 = Node256::new(u16::from(count), keys, child_slots, postings, doc_count);
                self.node256.write().add(n256, self)
            }
            _ => unreachable!(),
        }
    }

    fn read_term_header(&self, leaf: &TermLeaf, index: usize) -> Result<TermHeader> {
        ensure!(index < leaf.count as usize,);

        let header_len = size_of::<TermHeader>();
        let start = index * header_len;
        let end = start + header_len;

        match try_pod_read_unaligned(&leaf.data[start..end]) {
            Ok(header) => Ok(header),
            Err(error) => anyhow::bail!("invalid term header: {error:?}"),
        }
    }

    async fn update_term_leaf(
        &self,
        leaf_ref: NodeRef,
        suffix: &[u8],
        doc_id: u64,
        freq: u32,
    ) -> Result<bool> {
        ensure!(suffix.len() <= u8::MAX as usize);

        let header_len = size_of::<TermHeader>();
        let (term, insert_at) = {
            let terms = self.terms.read();
            self.find_term(terms.get(leaf_ref)?, suffix)?
        };

        if let Some((index, mut header)) = term {
            let old_postings = header.postings;
            let (postings, updated_freq) = self.update_postings(old_postings, doc_id, freq).await?;
            header.postings = postings;

            if freq == updated_freq {
                header.doc_count = match header.doc_count.checked_add(1) {
                    Some(doc_count) => doc_count,
                    None => anyhow::bail!("term document count overflow"),
                };
            }

            {
                let mut terms = self.terms.write();
                let leaf = terms.get_mut(leaf_ref)?;

                let header_start = index * header_len;
                let header_end = header_start + header_len;
                let header_b = bytes_of(&header);
                leaf.data[header_start..header_end].copy_from_slice(header_b);
            }

            return Ok(true);
        }

        self.insert_term(leaf_ref, suffix, insert_at, doc_id, freq)
            .await
    }

    fn find_term(
        &self,
        leaf: &TermLeaf,
        suffix: &[u8],
    ) -> Result<(Option<(usize, TermHeader)>, usize)> {
        let mut insert_at = leaf.count as usize;

        for index in 0..leaf.count as usize {
            let header = self.read_term_header(leaf, index)?;
            let suffix_start = header.offset as usize;
            let suffix_end = suffix_start + header.len as usize;

            match suffix.cmp(&leaf.data[suffix_start..suffix_end]) {
                Ordering::Equal => return Ok((Some((index, header)), insert_at)),
                Ordering::Less => {
                    insert_at = index;
                    break;
                }
                Ordering::Greater => {}
            }
        }

        Ok((None, insert_at))
    }

    async fn create_postings(&self, doc_id: u64, freq: u32) -> Result<NodeRef> {
        let node = NodeRef::default();
        let (root, _) = self.update_postings(node, doc_id, freq).await?;
        Ok(root)
    }

    async fn insert_term(
        &self,
        leaf_ref: NodeRef,
        suffix: &[u8],
        insert_at: usize,
        doc_id: u64,
        freq: u32,
    ) -> Result<bool> {
        let header_len = size_of::<TermHeader>();
        let leaf = *self.terms.read().get(leaf_ref)?;

        let count = leaf.count as usize;
        if leaf.count == u16::MAX {
            return Ok(false);
        }

        let suffix_end = leaf.suffix_start as usize;
        let Some(suffix_start) = suffix_end.checked_sub(suffix.len()) else {
            return Ok(false);
        };

        let headers_end = (count + 1) * header_len;
        if headers_end > suffix_start {
            return Ok(false);
        }

        let postings = self.create_postings(doc_id, freq).await?;

        let header = TermHeader {
            offset: suffix_start as u16,
            doc_count: 1,
            postings,
            len: suffix.len() as u8,
            _pad: 0,
        };

        let mut terms = self.terms.write();
        let leaf = terms.get_mut(leaf_ref)?;
        leaf.data[suffix_start..suffix_end].copy_from_slice(suffix);
        leaf.suffix_start = suffix_start as u16;

        let header_start = insert_at * header_len;
        let header_end = header_start + header_len;
        let header_shift_end = count * header_len;
        let headers_to_shift = header_start..header_shift_end;
        leaf.data.copy_within(headers_to_shift, header_end);
        leaf.data[header_start..header_end].copy_from_slice(bytes_of(&header));
        leaf.count += 1;

        Ok(true)
    }

    async fn leaf_or_create(
        &self,
        root: NodeRef,
        doc_id: u64,
        leaf_kind: NodeKind,
    ) -> Result<(NodeRef, NodeRef)> {
        let bytes = doc_id.to_be_bytes();
        let mut updated_root = root;
        let mut node_ref = root;
        let mut parent: Option<Edge> = None;

        if node_ref.kind == NodeKind::Empty {
            let node = Node4::with_prefix(&bytes[..7]);
            let mut n4 = self.node4.write();

            node_ref = n4.add(node, self)?;
            updated_root = node_ref;
        }

        // TODO: Only read the node once?
        let mut byte_pos = 0;
        'walk: while byte_pos < bytes.len() {
            let (prefix, prefix_len) = self.get_prefix(node_ref)?;

            for i in 0..prefix_len {
                if prefix[i] != bytes[byte_pos + i] {
                    let (branch_ref, new_ref) =
                        self.split_prefix(node_ref, &prefix, prefix_len, &bytes, byte_pos, i)?;

                    if let Some(edge) = parent {
                        self.insert_node(edge, branch_ref)?;
                    } else {
                        updated_root = branch_ref;
                    }

                    self.to_remove.lock().push(node_ref);

                    node_ref = new_ref;
                    byte_pos += i + 1;
                    continue 'walk;
                } else if i == prefix_len - 1 {
                    byte_pos += prefix_len;
                }
            }

            let key = bytes[byte_pos];

            let child_kind = if byte_pos + 1 < bytes.len() {
                NodeKind::Node4
            } else {
                leaf_kind
            };

            let (updated_ref, child_ref, child_created) =
                self.child_or_create(Edge::new(node_ref, key), child_kind)?;

            if child_created && child_kind == NodeKind::Node4 {
                let prefix = &bytes[byte_pos + 1..7];
                self.set_prefix(child_ref, prefix)?;
            }

            if updated_ref != node_ref {
                if let Some(edge) = parent {
                    self.insert_node(edge, updated_ref)?;
                } else {
                    updated_root = updated_ref;
                }

                self.to_remove.lock().push(node_ref);
            }

            parent = Some(Edge::new(updated_ref, key));
            node_ref = child_ref;

            if node_ref.kind == leaf_kind {
                break;
            }

            byte_pos += 1;
        }

        anyhow::ensure!(node_ref.kind == leaf_kind);
        Ok((updated_root, node_ref))
    }

    async fn update_postings(
        &self,
        root: NodeRef,
        doc_id: u64,
        token_count: u32,
    ) -> Result<(NodeRef, u32)> {
        let (root, node_ref) = self
            .leaf_or_create(root, doc_id, NodeKind::ScalarLeaf)
            .await?;

        let prev = self.scalars.read().get(node_ref)?.value;
        let updated_freq = match prev.checked_add(token_count) {
            Some(updated_freq) => updated_freq,
            None => anyhow::bail!("term frequency overflow"),
        };

        let new_leaf = ScalarLeaf::new(updated_freq);
        let mut postings = self.scalars.write();
        postings.update(node_ref, &new_leaf)?;

        Ok((root, updated_freq))
    }

    async fn set_doc_length(&self, doc_id: u64, len: u32) -> Result<()> {
        let old_root = self.header.read().doc_root;
        let (root, node_ref) = self
            .leaf_or_create(old_root, doc_id, NodeKind::ScalarLeaf)
            .await?;

        if root != old_root {
            self.header.write().doc_root = root;
        }

        let mut docs = self.scalars.write();
        let leaf = ScalarLeaf::new(len);
        docs.update(node_ref, &leaf)?;

        Ok(())
    }

    #[inline(never)]
    fn create_scalar_leaf(&self) -> Result<NodeRef> {
        let mut scalars = self.scalars.write();
        scalars.add(ScalarLeaf::default(), self)
    }

    fn child_or_create(
        &self,
        edge: Edge,
        child_kind: NodeKind,
    ) -> Result<(NodeRef, NodeRef, bool)> {
        if let Some(child_ref) = self.child_for(edge)? {
            return Ok((edge.node_ref, child_ref, false));
        }

        let child_ref = match child_kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write();
                node4.add(Node4::default(), self)?
            }
            NodeKind::TermLeaf => {
                let mut terms = self.terms.write();
                terms.add(TermLeaf::default(), self)?
            }
            NodeKind::ScalarLeaf => self.create_scalar_leaf()?,
            _ => unreachable!(),
        };

        let updated_ref = self.insert_child(edge, child_ref)?;

        Ok((updated_ref, child_ref, true))
    }

    // TODO: CAS
    pub async fn add(&self, doc: &str) -> Result<()> {
        let doc_id = self.get_doc_id();
        let tokens = tokenize(doc);

        let old_root = self.header.read().term_root;
        let mut header_ref = old_root;

        let mut len = 0_u32;
        for (token, freq) in tokens {
            let bytes = token.as_bytes();

            let mut node_ref = header_ref;

            let mut parent: Option<Edge> = None;
            let mut pos = 0;
            loop {
                match node_ref.kind {
                    NodeKind::TermLeaf => {
                        let suffix = &bytes[pos..];
                        if suffix.len() <= u8::MAX as usize {
                            if self
                                .update_term_leaf(node_ref, suffix, doc_id, freq)
                                .await?
                            {
                                break;
                            }
                        }

                        let updated_ref = self.split_term_leaf(node_ref)?;
                        if let Some(parent) = parent {
                            self.insert_node(parent, updated_ref)?;
                        } else {
                            header_ref = updated_ref;
                        }

                        if node_ref != old_root {
                            self.to_remove.lock().push(node_ref);
                        }

                        node_ref = updated_ref;
                    }
                    NodeKind::Node4 | NodeKind::Node16 | NodeKind::Node48 | NodeKind::Node256 => {
                        let Some(&key) = bytes.get(pos) else {
                            let (old_postings, doc_count) = self.get_node_postings(node_ref)?;

                            let (postings, updated_freq) = if old_postings.kind == NodeKind::Empty {
                                (self.create_postings(doc_id, freq).await?, freq)
                            } else {
                                self.update_postings(old_postings, doc_id, freq).await?
                            };

                            let doc_count = if freq == updated_freq {
                                match doc_count.checked_add(1) {
                                    Some(count) => count,
                                    None => anyhow::bail!("term document count overflow"),
                                }
                            } else {
                                doc_count
                            };

                            self.set_postings(node_ref, postings, doc_count)?;

                            break;
                        };

                        let child_bytes = bytes.len() - (pos + 1);
                        let child_kind = if child_bytes > u8::MAX as usize {
                            NodeKind::Node4
                        } else {
                            NodeKind::TermLeaf
                        };

                        let (updated_ref, child_ref, _) =
                            self.child_or_create(Edge::new(node_ref, key), child_kind)?;

                        if updated_ref != node_ref {
                            if let Some(parent) = parent {
                                self.insert_node(parent, updated_ref)?;
                            } else {
                                header_ref = updated_ref;
                            }

                            if node_ref != old_root {
                                self.to_remove.lock().push(node_ref);
                            }
                        }

                        node_ref = child_ref;
                        pos += 1;
                        parent = Some(Edge::new(updated_ref, key));
                    }
                    _ => unreachable!(),
                }
            }

            len = match len.checked_add(freq) {
                Some(len) => len,
                None => anyhow::bail!("document length overflow"),
            };
        }

        self.set_doc_length(doc_id, len).await?;
        self.update_header(header_ref, len)?;

        if header_ref != old_root {
            self.to_remove.lock().push(old_root);
        }

        Ok(())
    }

    fn update_header(&self, term_root: NodeRef, len: u32) -> Result<()> {
        let mut header = self.header.write();

        let doc_count = match header.doc_count.checked_add(1) {
            Some(count) => count,
            None => anyhow::bail!("document count overflow"),
        };

        let token_count = match header.token_count.checked_add(len) {
            Some(count) => count,
            None => anyhow::bail!("token count overflow"),
        };

        header.doc_count = doc_count;
        header.token_count = token_count;
        header.term_root = term_root;

        Ok(())
    }

    fn get_active_readers(&self) -> usize {
        self.active_readers
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn remove_nodes(&self) -> Result<()> {
        if self.get_active_readers() != 0 {
            return Ok(());
        }

        let mut to_remove = self.to_remove.lock();
        if to_remove.is_empty() {
            return Ok(());
        }

        while let Some(&node_ref) = to_remove.last() {
            match node_ref.kind {
                NodeKind::Node4 => self.node4.write().remove(node_ref)?,
                NodeKind::Node16 => self.node16.write().remove(node_ref)?,
                NodeKind::Node48 => self.node48.write().remove(node_ref)?,
                NodeKind::Node256 => self.node256.write().remove(node_ref)?,
                NodeKind::TermLeaf => self.terms.write().remove(node_ref)?,
                NodeKind::ScalarLeaf => self.scalars.write().remove(node_ref)?,
                NodeKind::Empty => anyhow::bail!("cannot remove an empty node"),
            }

            to_remove.pop();
        }

        Ok(())
    }

    pub async fn update(&self, args: Arc<Args>) -> Result<()> {
        let _guard = self.update_guard.lock().await;
        fs::create_dir_all(&args.docs_dir).await?;

        let mut entries = fs::read_dir(&args.wal_dir).await?;
        let mut sources = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_file() {
                sources.push(entry.path());
            }
        }

        let total = sources.len();
        let started = Instant::now();
        let mut last_info = Instant::now();

        if total > 0 {
            tracing::info!(total, "Indexing documents");
        }

        for (index, source) in sources.into_iter().enumerate() {
            let filename = source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let bytes = fs::read(&source).await?;
            let doc = std::str::from_utf8(&bytes)?;

            self.add(doc).await?;
            self.remove_nodes().await?;

            let dest = args.docs_dir.join(filename);
            fs::rename(source, dest).await?;

            let processed = index + 1;
            if last_info.elapsed().as_secs() >= 1 || processed == total {
                tracing::info!(
                    processed,
                    total,
                    percent = processed as f64 / total as f64 * 100.0,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Documents indexed"
                );

                last_info = Instant::now();
            }
        }

        self.remove_nodes().await?;

        Ok(())
    }

    fn get_term_header(&self, s: &str) -> Result<Option<TermHeader>> {
        let b = s.as_bytes();
        let mut node_ref = self.header.read().term_root;

        let mut pos = 0;
        loop {
            match node_ref.kind {
                NodeKind::TermLeaf => {
                    let terms = self.terms.read();
                    let leaf = terms.get(node_ref)?;

                    for i in 0..leaf.count as usize {
                        let header = self.read_term_header(leaf, i)?;
                        let start = header.offset as usize;
                        let end = start + header.len as usize;

                        ensure!(end <= leaf.data.len());

                        let suffix = &leaf.data[start..end];
                        if *suffix == b[pos..] {
                            return Ok(Some(header));
                        }
                    }

                    unreachable!()
                }
                NodeKind::Node4 | NodeKind::Node16 | NodeKind::Node48 | NodeKind::Node256 => {
                    let (prefix, prefix_len) = self.get_prefix(node_ref)?;

                    if b[pos..].starts_with(&prefix) {
                        pos += prefix_len;
                    }

                    let to_child = Edge::new(node_ref, b[pos]);

                    match self.child_for(to_child) {
                        Ok(Some(child_ref)) => {
                            node_ref = child_ref;
                            pos += 1
                        }
                        _ => break,
                    }
                }
                _ => unreachable!(),
            }
        }

        Ok(None)
    }

    fn idf(&self, token_doc_count: u32) -> f32 {
        let header = self.header.read();
        f32::ln(header.doc_count as f32 / token_doc_count as f32)
    }

    fn tf(
        &self,
        freqs: &mut Vec<(u64, u32)>,
        node_ref: NodeRef,
        path: [u8; 8],
        pos: usize,
    ) -> Result<()> {
        let prefix_path = |prefix: [u8; 7], prefix_len: u8| -> [u8; 8] {
            let len = prefix_len as usize;
            let src = &prefix[0..len];

            let mut dest = path;
            dest[pos..pos + len].copy_from_slice(&src);

            dest
        };

        match node_ref.kind {
            NodeKind::ScalarLeaf => {
                let freq = self.scalars.read().get(node_ref)?.value;
                let doc_id = u64::from_be_bytes(path);
                freqs.push((doc_id, freq));
            }
            NodeKind::Node4 => {
                let n4 = self.node4.read();
                let node = n4.get(node_ref)?;

                let prefix_path = prefix_path(node.prefix, node.prefix_len);
                let prefix_pos = pos + node.prefix_len as usize;
                for i in 0..node.count as usize {
                    let mut child_path = prefix_path;
                    child_path[prefix_pos] = node.keys[i];

                    let child_pos = prefix_pos + 1;
                    self.tf(freqs, node.children[i], child_path, child_pos)?;
                }
            }
            NodeKind::Node16 => {
                let n16 = self.node16.read();
                let node = n16.get(node_ref)?;

                let prefix_path = prefix_path(node.prefix, node.prefix_len);
                let prefix_pos = pos + node.prefix_len as usize;
                for i in 0..node.count as usize {
                    let mut child_path = prefix_path;
                    child_path[prefix_pos] = node.keys[i];

                    let child_pos = prefix_pos + 1;
                    self.tf(freqs, node.children[i], child_path, child_pos)?;
                }
            }
            NodeKind::Node48 => {
                let n48 = self.node48.read();
                let node = n48.get(node_ref)?;

                let prefix_path = prefix_path(node.prefix, node.prefix_len);
                let prefix_pos = pos + node.prefix_len as usize;

                for key in 0..256 {
                    let child_index = node.keys[key];

                    if child_index != u8::MAX {
                        let mut child_path = prefix_path;
                        child_path[prefix_pos] = key as u8;

                        let child_ref = node.children[child_index as usize];
                        let child_pos = prefix_pos + 1;
                        self.tf(freqs, child_ref, child_path, child_pos)?;
                    }
                }
            }
            NodeKind::Node256 => {
                let n256 = self.node256.read();
                let node = n256.get(node_ref)?;

                let prefix_path = prefix_path(node.prefix, node.prefix_len);
                let prefix_pos = pos + node.prefix_len as usize;
                for i in 0..256 {
                    let child_ref = node.children[i];
                    if child_ref.kind != NodeKind::Empty {
                        let mut child_path = prefix_path;
                        child_path[prefix_pos] = node.keys[i];

                        let child_pos = prefix_pos + 1;
                        self.tf(freqs, child_ref, child_path, child_pos)?;
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    pub fn search(&self, query: &str) -> Result<HashMap<u64, f32>> {
        let started_at = Instant::now();
        let tokens = tokenize(query);

        dbg!(query);

        let mut out = HashMap::new();
        for (token, _) in tokens {
            let Some(header) = self.get_term_header(&token)? else {
                continue;
            };

            let mut tfs = vec![];
            self.tf(&mut tfs, header.postings, [0u8; 8], 0)?;

            let idf = self.idf(header.doc_count);
            for (doc_id, tf) in tfs {
                let tf_idf = tf as f32 * idf;
                *out.entry(doc_id).or_insert(0.0) += tf_idf;
            }
        }

        let elapsed_micros = started_at.elapsed().as_micros();
        tracing::info!(elapsed_micros, query, "Search");

        Ok(out)
    }
}
