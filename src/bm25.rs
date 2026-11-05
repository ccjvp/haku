use crate::{args::Args, bitmap::Bitmap, io::IO, tokenizer::tokenize};
use anyhow::{Result, ensure};
use bytemuck::{CheckedBitPattern, NoUninit, bytes_of, checked::try_pod_read_unaligned};
use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, AtomicUsize},
    },
};
use tokio::{
    fs,
    sync::{Mutex, RwLock},
};

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
    keys: [u8; 4],
    children: [NodeRef; 4],
    postings: NodeRef,
    doc_count: u32,
}

impl Node4 {
    fn new(
        count: u8,
        keys: [u8; 4],
        children: [NodeRef; 4],
        postings: NodeRef,
        doc_count: u32,
    ) -> Self {
        Self {
            count,
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

        Self::new(node.count, keys, children, node.postings, node.doc_count)
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

        Self::new(node.count, keys, children, node.postings, node.doc_count)
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

        Self::new(
            u16::from(node.count),
            keys,
            children,
            node.postings,
            node.doc_count,
        )
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
            keys,
            children,
            postings,
            doc_count,
        }
    }

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

    fn get(&self, slot: u16) -> T {
        self.slots[slot as usize]
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

    fn get(&self, node_ref: NodeRef) -> Result<T> {
        let buf = self.bufs.get(&node_ref.buf_id);
        Ok(buf.unwrap().get(node_ref.slot))
    }

    fn allocate(&mut self, bm25: &BM25) -> Result<u32> {
        let buf_id = bm25.get_buf_id();

        ensure!(!self.bufs.contains_key(&buf_id));

        self.bufs.insert(buf_id, Buf::new());
        Ok(buf_id)
    }

    fn update(&mut self, node_ref: NodeRef, node: T) -> Result<NodeRef> {
        ensure!(node_ref.kind == self.kind);

        let buf = self.bufs.get_mut(&node_ref.buf_id).unwrap();
        let slot = node_ref.slot as usize;

        ensure!(!buf.free.get(slot));

        buf.slots[slot] = node;
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

        // TODO: Cache if the birmap has space?
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
    update_guard: Mutex<()>,
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

        let term_root = bm25.terms.write().await.add(TermLeaf::default(), &bm25)?;
        let doc_root = bm25.node4.write().await.add(Node4::default(), &bm25)?;

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

    async fn get_postings(&self, node_ref: NodeRef) -> anyhow::Result<(NodeRef, u32)> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let node = self.node4.read().await.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Node16 => {
                let node = self.node16.read().await.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Node48 => {
                let node = self.node48.read().await.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Node256 => {
                let node = self.node256.read().await.get(node_ref)?;
                Ok((node.postings, node.doc_count))
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                Ok((NodeRef::default(), 0))
            }
        }
    }

    async fn set_postings(
        &self,
        node_ref: NodeRef,
        postings: NodeRef,
        doc_count: u32,
    ) -> anyhow::Result<()> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write().await;
                let mut node = node4.get(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                node4.update(node_ref, node).map(|_| ())
            }
            NodeKind::Node16 => {
                let mut node16 = self.node16.write().await;
                let mut node = node16.get(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                node16.update(node_ref, node).map(|_| ())
            }
            NodeKind::Node48 => {
                let mut node48 = self.node48.write().await;
                let mut node = node48.get(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                node48.update(node_ref, node).map(|_| ())
            }
            NodeKind::Node256 => {
                let mut node256 = self.node256.write().await;
                let mut node = node256.get(node_ref)?;
                node.postings = postings;
                node.doc_count = doc_count;
                node256.update(node_ref, node).map(|_| ())
            }
            NodeKind::Empty | NodeKind::TermLeaf | NodeKind::ScalarLeaf => {
                unreachable!()
            }
        }
    }

    async fn child_for(&self, edge: Edge) -> anyhow::Result<Option<NodeRef>> {
        let Edge { node_ref, key } = edge;

        match node_ref.kind {
            NodeKind::Node4 => {
                let node = self.node4.read().await.get(node_ref)?;
                Ok(node.child_for(key))
            }
            NodeKind::Node16 => {
                let node = self.node16.read().await.get(node_ref)?;
                Ok(node.child_for(key))
            }
            NodeKind::Node48 => {
                let node = self.node48.read().await.get(node_ref)?;
                Ok(node.child_for(key))
            }
            NodeKind::Node256 => {
                let node = self.node256.read().await.get(node_ref)?;
                Ok(node.child_for(key))
            }
            _ => unreachable!(),
        }
    }

    async fn update_node(&self, edge: Edge, child_ref: NodeRef) -> anyhow::Result<()> {
        let Edge { node_ref, key } = edge;

        match node_ref.kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write().await;
                let mut node = node4.get(node_ref)?;
                node.insert_child(key, child_ref)?;
                node4.update(node_ref, node).map(|_| ())
            }
            NodeKind::Node16 => {
                let mut node16 = self.node16.write().await;
                let mut node = node16.get(node_ref)?;
                node.insert_child(key, child_ref)?;
                node16.update(node_ref, node).map(|_| ())
            }
            NodeKind::Node48 => {
                let mut node48 = self.node48.write().await;
                let mut node = node48.get(node_ref)?;
                node.insert_child(key, child_ref)?;
                node48.update(node_ref, node).map(|_| ())
            }
            NodeKind::Node256 => {
                let mut node256 = self.node256.write().await;
                let mut node = node256.get(node_ref)?;
                node.insert_child(key, child_ref)?;
                node256.update(node_ref, node).map(|_| ())
            }
            _ => unreachable!(),
        }
    }

    async fn insert_node(&self, edge: Edge, node_ref: NodeRef) -> anyhow::Result<()> {
        let Edge {
            node_ref: parent_ref,
            key,
        } = edge;

        match parent_ref.kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write().await;
                let mut node = node4.get(parent_ref)?;
                for i in 0..node.count as usize {
                    if node.keys[i] == key {
                        node.children[i] = node_ref;
                        node4.update(parent_ref, node)?;
                        return Ok(());
                    }
                }
                unreachable!()
            }
            NodeKind::Node16 => {
                let mut node16 = self.node16.write().await;
                let mut node = node16.get(parent_ref)?;
                for i in 0..node.count as usize {
                    if node.keys[i] == key {
                        node.children[i] = node_ref;
                        node16.update(parent_ref, node)?;
                        return Ok(());
                    }
                }

                unreachable!()
            }
            NodeKind::Node48 => {
                let mut node48 = self.node48.write().await;
                let mut node = node48.get(parent_ref)?;
                let key_index = node.keys[key as usize];

                ensure!(key_index != u8::MAX);

                node.children[key_index as usize] = node_ref;
                node48.update(parent_ref, node)?;
                Ok(())
            }
            NodeKind::Node256 => {
                let mut node256 = self.node256.write().await;
                let mut node = node256.get(parent_ref)?;
                let slot = key as usize;

                ensure!(node.children[slot].kind != NodeKind::Empty);

                node.children[slot] = node_ref;
                node256.update(parent_ref, node)?;
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    async fn is_full(&self, node_ref: NodeRef) -> anyhow::Result<bool> {
        match node_ref.kind {
            NodeKind::Node4 => {
                let node = self.node4.read().await.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            NodeKind::Node16 => {
                let node = self.node16.read().await.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            NodeKind::Node48 => {
                let node = self.node48.read().await.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            NodeKind::Node256 => {
                let node = self.node256.read().await.get(node_ref)?;
                Ok(node.count as usize == node.children.len())
            }
            _ => unreachable!(),
        }
    }

    async fn insert_child(&self, mut edge: Edge, child_ref: NodeRef) -> anyhow::Result<NodeRef> {
        anyhow::ensure!(self.child_for(edge).await?.is_none());

        if self.is_full(edge.node_ref).await? {
            edge.node_ref = self.grow_node(edge.node_ref).await?;
        }

        self.update_node(edge, child_ref).await?;
        Ok(edge.node_ref)
    }

    async fn grow_node(&self, node_ref: NodeRef) -> Result<NodeRef> {
        let grown_ref = match node_ref.kind {
            NodeKind::Node4 => {
                let node = self.node4.read().await.get(node_ref)?;
                let grown = Node16::from_node4(node);
                self.node16.write().await.add(grown, self)?
            }
            NodeKind::Node16 => {
                let node = self.node16.read().await.get(node_ref)?;
                let grown = Node48::from_node16(node);
                self.node48.write().await.add(grown, self)?
            }
            NodeKind::Node48 => {
                let node = self.node48.read().await.get(node_ref)?;
                let grown = Node256::from_node48(node);
                self.node256.write().await.add(grown, self)?
            }
            _ => unreachable!(),
        };

        Ok(grown_ref)
    }

    async fn split_term_leaf(&self, leaf_ref: NodeRef) -> Result<NodeRef> {
        ensure!(leaf_ref.kind == NodeKind::TermLeaf);

        let leaf = self.terms.read().await.get(leaf_ref)?;

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
            let mut terms = self.terms.write().await;
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
                self.node4.write().await.add(n4, self)
            }
            5..=16 => {
                let mut keys = [0; 16];
                let mut child_slots = [NodeRef::default(); 16];

                for (index, (key, child_ref)) in children.into_iter().enumerate() {
                    keys[index] = key;
                    child_slots[index] = child_ref;
                }

                let n16 = Node16::new(count, keys, child_slots, postings, doc_count);
                self.node16.write().await.add(n16, self)
            }
            17..=48 => {
                let mut keys = [u8::MAX; 256];
                let mut child_slots = [NodeRef::default(); 48];

                for (index, (key, child_ref)) in children.into_iter().enumerate() {
                    keys[key as usize] = index as u8;
                    child_slots[index] = child_ref;
                }

                let n48 = Node48::new(count, keys, child_slots, postings, doc_count);
                self.node48.write().await.add(n48, self)
            }
            49..=254 => {
                let mut keys = [0; 256];
                let mut child_slots = [NodeRef::default(); 256];

                for (key, child_ref) in children {
                    keys[key as usize] = key;
                    child_slots[key as usize] = child_ref;
                }

                let n256 = Node256::new(u16::from(count), keys, child_slots, postings, doc_count);
                self.node256.write().await.add(n256, self)
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
    ) -> Result<bool> {
        ensure!(suffix.len() <= u8::MAX as usize);

        let header_len = size_of::<TermHeader>();
        let mut leaf = self.terms.read().await.get(leaf_ref)?;
        let (term, insert_at) = self.find_term(&leaf, suffix)?;

        if let Some((index, mut header)) = term {
            if header.postings.kind == NodeKind::Empty {
                let mut node4 = self.node4.write().await;
                let n4 = node4.add(Node4::default(), self)?;
                header.postings = n4;
            }

            let old_postings = header.postings;
            let (postings, freq) = self.update_postings(old_postings, doc_id).await?;
            header.postings = postings;

            if freq == 1 {
                header.doc_count = match header.doc_count.checked_add(1) {
                    Some(doc_count) => doc_count,
                    None => anyhow::bail!("term document count overflow"),
                };
            }

            let mut terms = self.terms.write().await;
            leaf = terms.get(leaf_ref)?;

            let header_start = index * header_len;
            let header_end = header_start + header_len;
            let header_b = bytes_of(&header);
            leaf.data[header_start..header_end].copy_from_slice(header_b);

            terms.update(leaf_ref, leaf)?;
            if postings != old_postings {
                self.to_remove.lock().await.push(old_postings);
            }

            return Ok(true);
        }

        self.insert_term(leaf_ref, suffix, insert_at, doc_id).await
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

    async fn create_postings(&self, doc_id: u64) -> Result<NodeRef> {
        let root = {
            let mut node4 = self.node4.write().await;
            node4.add(Node4::default(), self)?
        };
        let (root, _) = self.update_postings(root, doc_id).await?;
        Ok(root)
    }

    async fn insert_term(
        &self,
        leaf_ref: NodeRef,
        suffix: &[u8],
        insert_at: usize,
        doc_id: u64,
    ) -> Result<bool> {
        let header_len = size_of::<TermHeader>();
        let mut leaf = self.terms.read().await.get(leaf_ref)?;

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

        let postings = self.create_postings(doc_id).await?;

        let header = TermHeader {
            offset: suffix_start as u16,
            doc_count: 1,
            postings,
            len: suffix.len() as u8,
            _pad: 0,
        };

        leaf.data[suffix_start..suffix_end].copy_from_slice(suffix);
        leaf.suffix_start = suffix_start as u16;

        let header_start = insert_at * header_len;
        let header_end = header_start + header_len;
        let header_shift_end = count * header_len;
        let headers_to_shift = header_start..header_shift_end;
        leaf.data.copy_within(headers_to_shift, header_end);
        leaf.data[header_start..header_end].copy_from_slice(bytes_of(&header));
        leaf.count += 1;

        self.terms.write().await.update(leaf_ref, leaf)?;
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

        for (pos, &key) in bytes.iter().enumerate() {
            let child_kind = if pos + 1 < bytes.len() {
                NodeKind::Node4
            } else {
                leaf_kind
            };

            let (updated_ref, child_ref) = self
                .child_or_create(Edge::new(node_ref, key), child_kind)
                .await?;

            if updated_ref != node_ref {
                if let Some(edge) = parent {
                    self.insert_node(edge, updated_ref).await?;
                    self.to_remove.lock().await.push(node_ref);
                } else {
                    updated_root = updated_ref;
                }
            }

            parent = Some(Edge::new(updated_ref, key));
            node_ref = child_ref;
        }

        anyhow::ensure!(node_ref.kind == leaf_kind);
        Ok((updated_root, node_ref))
    }

    async fn update_postings(&self, root: NodeRef, doc_id: u64) -> Result<(NodeRef, u32)> {
        let (root, node_ref) = self
            .leaf_or_create(root, doc_id, NodeKind::ScalarLeaf)
            .await?;

        let prev = self.scalars.read().await.get(node_ref)?.value;
        let freq = match prev.checked_add(1) {
            Some(freq) => freq,
            None => anyhow::bail!("term frequency overflow"),
        };

        let new_leaf = ScalarLeaf::new(freq);
        let mut postings = self.scalars.write().await;
        postings.update(node_ref, new_leaf)?;

        Ok((root, freq))
    }

    async fn set_doc_length(&self, doc_id: u64, len: u32) -> Result<()> {
        let old_root = self.header.read().await.doc_root;
        let (root, node_ref) = self
            .leaf_or_create(old_root, doc_id, NodeKind::ScalarLeaf)
            .await?;

        if root != old_root {
            self.header.write().await.doc_root = root;
            self.to_remove.lock().await.push(old_root);
        }

        let mut docs = self.scalars.write().await;
        docs.update(node_ref, ScalarLeaf::new(len))?;

        Ok(())
    }

    async fn child_or_create(
        &self,
        edge: Edge,
        child_kind: NodeKind,
    ) -> anyhow::Result<(NodeRef, NodeRef)> {
        if let Some(child_ref) = self.child_for(edge).await? {
            return Ok((edge.node_ref, child_ref));
        }

        let child_ref = match child_kind {
            NodeKind::Node4 => {
                let mut node4 = self.node4.write().await;
                node4.add(Node4::default(), self)?
            }
            NodeKind::TermLeaf => {
                let mut terms = self.terms.write().await;
                terms.add(TermLeaf::default(), self)?
            }
            NodeKind::ScalarLeaf => {
                let mut scalars = self.scalars.write().await;
                scalars.add(ScalarLeaf::default(), self)?
            }
            _ => unreachable!(),
        };

        let updated_ref = self.insert_child(edge, child_ref).await?;

        Ok((updated_ref, child_ref))
    }

    pub async fn add(&self, doc: &str) -> Result<()> {
        let started_at = std::time::Instant::now();
        let doc_id = self.get_doc_id();
        let tokens = tokenize(doc);

        let old_root = self.header.read().await.term_root;
        let mut header_ref = old_root;

        let mut len = 0_u32;
        for token in tokens {
            let bytes = token.as_bytes();

            let mut node_ref = header_ref;

            let mut parent: Option<Edge> = None;
            let mut pos = 0;
            loop {
                match node_ref.kind {
                    NodeKind::TermLeaf => {
                        let suffix = &bytes[pos..];
                        if suffix.len() <= u8::MAX as usize {
                            if self.update_term_leaf(node_ref, suffix, doc_id).await? {
                                break;
                            }
                        }

                        let updated_ref = self.split_term_leaf(node_ref).await?;
                        if let Some(parent) = parent {
                            self.insert_node(parent, updated_ref).await?;
                        } else {
                            header_ref = updated_ref;
                        }

                        if node_ref != old_root {
                            self.to_remove.lock().await.push(node_ref);
                        }

                        node_ref = updated_ref;
                    }
                    NodeKind::Node4 | NodeKind::Node16 | NodeKind::Node48 | NodeKind::Node256 => {
                        let Some(&key) = bytes.get(pos) else {
                            let (old_postings, doc_count) = self.get_postings(node_ref).await?;

                            let (postings, freq) = if old_postings.kind == NodeKind::Empty {
                                (self.create_postings(doc_id).await?, 1)
                            } else {
                                self.update_postings(old_postings, doc_id).await?
                            };

                            let doc_count = if freq == 1 {
                                match doc_count.checked_add(1) {
                                    Some(count) => count,
                                    None => anyhow::bail!("term document count overflow"),
                                }
                            } else {
                                doc_count
                            };

                            self.set_postings(node_ref, postings, doc_count).await?;
                            if old_postings.kind != NodeKind::Empty && postings != old_postings {
                                self.to_remove.lock().await.push(old_postings);
                            }

                            break;
                        };

                        let child_bytes = bytes.len() - (pos + 1);
                        let child_kind = if child_bytes > u8::MAX as usize {
                            NodeKind::Node4
                        } else {
                            NodeKind::TermLeaf
                        };

                        let (updated_ref, child_ref) = self
                            .child_or_create(Edge::new(node_ref, key), child_kind)
                            .await?;

                        if updated_ref != node_ref {
                            if let Some(parent) = parent {
                                self.insert_node(parent, updated_ref).await?;
                            } else {
                                header_ref = updated_ref;
                            }

                            if node_ref != old_root {
                                self.to_remove.lock().await.push(node_ref);
                            }
                        }

                        node_ref = child_ref;
                        pos += 1;
                        parent = Some(Edge::new(updated_ref, key));
                    }
                    _ => unreachable!(),
                }
            }

            len = match len.checked_add(1) {
                Some(len) => len,
                None => anyhow::bail!("document length overflow"),
            };
        }

        self.set_doc_length(doc_id, len).await?;
        self.update_header(header_ref, len).await?;

        if header_ref != old_root {
            self.to_remove.lock().await.push(old_root);
        }

        let elapsed_ms = started_at.elapsed().as_millis();
        tracing::info!(elapsed_ms, "Doc indexed");

        Ok(())
    }

    async fn update_header(&self, term_root: NodeRef, len: u32) -> Result<()> {
        let mut header = self.header.write().await;

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

        let mut to_remove = self.to_remove.lock().await;
        if to_remove.is_empty() {
            return Ok(());
        }

        let started_at = std::time::Instant::now();
        while let Some(&node_ref) = to_remove.last() {
            match node_ref.kind {
                NodeKind::Node4 => self.node4.write().await.remove(node_ref)?,
                NodeKind::Node16 => self.node16.write().await.remove(node_ref)?,
                NodeKind::Node48 => self.node48.write().await.remove(node_ref)?,
                NodeKind::Node256 => self.node256.write().await.remove(node_ref)?,
                NodeKind::TermLeaf => self.terms.write().await.remove(node_ref)?,
                NodeKind::ScalarLeaf => self.scalars.write().await.remove(node_ref)?,
                NodeKind::Empty => anyhow::bail!("cannot remove an empty node"),
            }

            to_remove.pop();
        }

        let elapsed_ms = started_at.elapsed().as_millis();
        tracing::info!(elapsed_ms, "Nodes removed");

        Ok(())
    }

    pub async fn update(&self, args: Arc<Args>) -> Result<()> {
        let _guard = self.update_guard.lock().await;
        let mut entries = fs::read_dir(&args.wal_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let source = entry.path();
            if !entry.file_type().await?.is_file() {
                continue;
            }

            let bytes = fs::read(&source).await?;
            let doc = std::str::from_utf8(&bytes)?;

            self.add(doc).await?;
            self.remove_nodes().await?;

            let dest = args.data_dir.join(entry.file_name());
            fs::create_dir_all(&args.data_dir).await?;
            fs::rename(source, dest).await?;
        }

        self.remove_nodes().await?;

        Ok(())
    }
}
