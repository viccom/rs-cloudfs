//! MockStorageDriver——内存后端（conformance kit 第一公民）。
//!
//! 用途：驱动 conformance 套件的参照实现 + 上层（L3+）单测的假后端。
//! **R4：能力位诚实**——只声明套件已验证的位，未实现的可选 trait
//! （RapidUpload/ChangeFeed/TokenEvents）对应位保持 false。
//!
//! 后端模型：
//! - `nodes: BTreeMap<RelPath, node>` 即「远端」（BTreeMap 迭代序 = 字典序，
//!   直接承载 list 的稳定有序契约）；
//! - staging 会话（`sessions`）独立于节点表——commit 前对 stat/list 不可见；
//!   RESUME 语义：stager 被 Drop（非 abort）后已完成的整块保留在会话里
//!   （uploadid 类比），下一次同路径同 size 提示的 writer 复用（只补差集）；
//! - `bytes_received` 只在「发送分片给后端」时递增——断言⑦的可观测点。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::stream;

use crate::capability::Capabilities;
use crate::driver::StorageDriver;
use crate::error::StorageError;
use crate::ids::{BackendHandle, EntryId, VolumeId};
use crate::stager::UploadStager;
use crate::vocab::{
    ByteStream, Entry, EntryKind, Listing, Page, PageCursor, Quota, Range, RelPath, WriteHint,
};

/// 内存 mock 驱动。
///
/// 测试支持面：
/// - [`MockStorageDriver::fail_next_stat`]：让下一次 `stat` 失败（断言⑤
///   的故障注入——注入的是**已映射**的 StorageError，errno→StorageError
///   映射表归驱动测试侧，L2 运行时不认识后端错误码——R1）；
/// - [`MockStorageDriver::bytes_received`]：后端累计收到的字节数
///   （断言⑦ RESUME 可观测点）。
pub struct MockStorageDriver {
    volume: VolumeId,
    chunk_size: u64,
    state: Arc<Mutex<MockState>>,
    // 故障注入队列（仅 stat 消费——套件断言⑤的回放通道）
    faults: Mutex<VecDeque<StorageError>>,
    bytes_received: Arc<AtomicU64>,
}

struct MockState {
    /// 「远端」节点表（BTreeMap 字典序 = list 稳定有序的来源）。
    nodes: BTreeMap<RelPath, MockNode>,
    /// handle → path 反查索引（delete/reader 按句柄寻址）。
    handles: BTreeMap<String, RelPath>,
    next_handle: u64,
    /// 上传会话（staging 区域；RESUME 的差集来源）。
    sessions: BTreeMap<RelPath, UploadSession>,
}

struct MockNode {
    id: EntryId,
    kind: EntryKind,
    size: u64,
    mtime: f64,
    data: Vec<u8>,
}

struct UploadSession {
    /// 已发送到「后端」的完整分片（按序）。
    chunks: Vec<Vec<u8>>,
    chunk_size: u64,
    /// 创建会话时的 size 提示——复用会话必须同提示（防错内容续传）。
    hinted_size: Option<u64>,
}

impl MockNode {
    fn entry(&self, path: &RelPath) -> Entry {
        Entry {
            id: self.id.clone(),
            path: path.clone(),
            kind: self.kind,
            size: self.size,
            mtime: self.mtime,
        }
    }
}

impl MockState {
    fn insert_node(
        &mut self,
        volume: &VolumeId,
        path: &RelPath,
        kind: EntryKind,
        data: Vec<u8>,
    ) -> Entry {
        let handle = self.next_handle.to_string();
        self.next_handle += 1;
        let id = EntryId::new(volume.clone(), BackendHandle::new(handle.clone()));
        let size = match kind {
            EntryKind::File => data.len() as u64,
            EntryKind::Dir => 0,
        };
        let node = MockNode {
            id: id.clone(),
            kind,
            size,
            mtime: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            data,
        };
        let entry = node.entry(path);
        self.nodes.insert(path.clone(), node);
        self.handles.insert(handle, path.clone());
        entry
    }

    /// 隐式创建缺失的祖先目录（不含 path 自身——最后一段归 insert/上传，
    /// writer/mkdir/rename 共用的父目录语义）。
    fn ensure_parents(&mut self, volume: &VolumeId, path: &RelPath) {
        let comps: Vec<&str> = path.components().collect();
        let mut prefix = RelPath::root();
        for comp in comps[..comps.len().saturating_sub(1)].iter() {
            if let Ok(next) = prefix.join(comp) {
                if !self.nodes.contains_key(&next) {
                    self.insert_node(volume, &next, EntryKind::Dir, Vec::new());
                }
                prefix = next;
            }
        }
    }
}

impl MockStorageDriver {
    /// 默认分块 16 字节的 mock（小分块让跨块/多块覆盖在极小数据量下成立）。
    pub fn new(volume: VolumeId) -> Self {
        Self::with_chunk_size(volume, 16)
    }

    pub fn with_chunk_size(volume: VolumeId, chunk_size: u64) -> Self {
        MockStorageDriver {
            volume,
            chunk_size: chunk_size.max(1),
            state: Arc::new(Mutex::new(MockState {
                nodes: BTreeMap::new(),
                handles: BTreeMap::new(),
                next_handle: 1,
                sessions: BTreeMap::new(),
            })),
            faults: Mutex::new(VecDeque::new()),
            bytes_received: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 套件断言②/⑦使用的分块边界。
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// 注入：下一次 `stat` 返回该错误（恰好一次）。
    pub fn fail_next_stat(&self, err: StorageError) {
        self.faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(err);
    }

    /// 后端至今收到的字节数（staging 分片「发送」即计入）。
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received.load(Ordering::SeqCst)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, MockState> {
        // 中毒恢复统一模式（code-style §2）
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[async_trait]
impl StorageDriver for MockStorageDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        // R4 诚实声明：全部经 conformance 套件八条验证过的位
        Capabilities {
            range_read: true,
            resume: true,
            multipart: false,
            server_side_move: true,
            rapid_upload: false,
            authoritative_index: true,
            change_feed: false,
            inbound: false,
            chat: false,
        }
    }

    async fn list(&self, dir: &RelPath, page: Page) -> Result<Listing, StorageError> {
        let st = self.lock_state();
        if !dir.is_root() && !st.nodes.contains_key(dir) {
            return Err(StorageError::NotFound);
        }
        let prefix = if dir.is_root() {
            String::new()
        } else {
            format!("{}/", dir.as_str())
        };
        // 直接子条目：去掉前缀后不再含 '/'（depth-1）
        let children: Vec<RelPath> = st
            .nodes
            .range(dir.clone()..)
            .filter_map(|(p, _)| {
                let rest = p.as_str().strip_prefix(prefix.as_str())?;
                (!rest.contains('/')).then_some(p.clone())
            })
            .collect();
        let offset = match &page.cursor {
            PageCursor::Start => 0usize,
            PageCursor::Next(tok) => tok
                .strip_prefix("off:")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        };
        let end = (offset + page.limit).min(children.len());
        let entries: Vec<Entry> = children[offset..end]
            .iter()
            .filter_map(|p| st.nodes.get(p).map(|n| n.entry(p)))
            .collect();
        let next = if end < children.len() {
            Some(PageCursor::Next(format!("off:{end}")))
        } else {
            None
        };
        Ok(Listing { entries, next })
    }

    async fn stat(&self, path: &RelPath) -> Result<Entry, StorageError> {
        if let Some(e) = self
            .faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
        {
            return Err(e);
        }
        let st = self.lock_state();
        if path.is_root() {
            // 根目录隐式存在（固定句柄 "root"）
            return Ok(Entry {
                id: EntryId::new(self.volume.clone(), BackendHandle::new("root")),
                path: path.clone(),
                kind: EntryKind::Dir,
                size: 0,
                mtime: 0.0,
            });
        }
        st.nodes
            .get(path)
            .map(|n| n.entry(path))
            .ok_or(StorageError::NotFound)
    }

    async fn mkdir(&self, path: &RelPath) -> Result<(), StorageError> {
        if path.is_root() {
            return Err(StorageError::Exists);
        }
        let mut st = self.lock_state();
        if st.nodes.contains_key(path) {
            return Err(StorageError::Exists);
        }
        st.ensure_parents(&self.volume, path);
        st.insert_node(&self.volume, path, EntryKind::Dir, Vec::new());
        Ok(())
    }

    async fn delete(&self, id: &EntryId) -> Result<(), StorageError> {
        let mut st = self.lock_state();
        if id.volume != self.volume {
            return Err(StorageError::NotFound);
        }
        let Some(path) = st.handles.get(id.handle.as_str()).cloned() else {
            // 声明形态（conformance 断言④）：mock 选择恒 NotFound
            return Err(StorageError::NotFound);
        };
        let Some(node) = st.nodes.get(&path) else {
            return Err(StorageError::NotFound);
        };
        let is_dir = node.kind == EntryKind::Dir;
        let prefix = format!("{}/", path.as_str());
        let removed: Vec<RelPath> = if is_dir {
            st.nodes
                .keys()
                .filter(|p| p.as_str() == path.as_str() || p.as_str().starts_with(&prefix))
                .cloned()
                .collect()
        } else {
            vec![path.clone()]
        };
        for p in &removed {
            if let Some(node) = st.nodes.remove(p) {
                st.handles.remove(node.id.handle.as_str());
            }
        }
        st.sessions.remove(&path);
        Ok(())
    }

    async fn rename(&self, from: &RelPath, to: &RelPath) -> Result<(), StorageError> {
        let mut st = self.lock_state();
        if from.is_root() || !st.nodes.contains_key(from) {
            return Err(StorageError::NotFound);
        }
        if st.nodes.contains_key(to) {
            return Err(StorageError::Exists);
        }
        let from_prefix = format!("{}/", from.as_str());
        if to.as_str().starts_with(&from_prefix) {
            return Err(StorageError::Invalid);
        }
        st.ensure_parents(&self.volume, to);
        // 受影响键：自身 + 全部后代（目录递归搬移）
        let moved: Vec<RelPath> = st
            .nodes
            .keys()
            .filter(|p| p.as_str() == from.as_str() || p.as_str().starts_with(&from_prefix))
            .cloned()
            .collect();
        for old in moved {
            let new = if old.as_str() == from.as_str() {
                to.clone()
            } else {
                RelPath::new(&format!(
                    "{}/{}",
                    to.as_str(),
                    &old.as_str()[from_prefix.len()..]
                ))
                .map_err(|_| StorageError::Invalid)?
            };
            let node = st.nodes.remove(&old).expect("keys 来自现存节点表");
            st.handles
                .insert(node.id.handle.as_str().to_string(), new.clone());
            st.nodes.insert(new, node);
        }
        if let Some(sess) = st.sessions.remove(from) {
            st.sessions.insert(to.clone(), sess);
        }
        Ok(())
    }

    async fn reader(&self, id: &EntryId, range: Option<Range>) -> Result<ByteStream, StorageError> {
        let st = self.lock_state();
        if id.volume != self.volume {
            return Err(StorageError::NotFound);
        }
        let Some(path) = st.handles.get(id.handle.as_str()) else {
            return Err(StorageError::NotFound);
        };
        let Some(node) = st.nodes.get(path) else {
            return Err(StorageError::NotFound);
        };
        if node.kind == EntryKind::Dir {
            return Err(StorageError::Invalid);
        }
        let len = node.size;
        let (start, end) = match range {
            None => (0u64, len),
            Some(r) => (r.start, r.end.unwrap_or(len).min(len)),
        };
        // 声明形态（conformance 断言②）：start >= size → 空流
        let data: Vec<u8> = if start >= len {
            Vec::new()
        } else {
            node.data[start as usize..end as usize].to_vec()
        };
        Ok(Box::pin(stream::iter(vec![Ok(bytes::Bytes::from(data))])))
    }

    async fn writer(
        &self,
        path: &RelPath,
        hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        if path.is_root() {
            return Err(StorageError::Invalid);
        }
        let mut st = self.lock_state();
        if let Some(node) = st.nodes.get(path) {
            if node.kind == EntryKind::Dir {
                return Err(StorageError::Invalid);
            }
        }
        // 会话复用条件：同路径且同 size 提示；否则旧会话作废重开
        let adoptable = st
            .sessions
            .get(path)
            .map(|s| s.hinted_size == hint.size)
            .unwrap_or(false);
        if !adoptable {
            st.sessions.remove(path);
            st.sessions.insert(
                path.clone(),
                UploadSession {
                    chunks: Vec::new(),
                    chunk_size: self.chunk_size,
                    hinted_size: hint.size,
                },
            );
        }
        st.ensure_parents(&self.volume, path);
        Ok(Box::new(MockStager {
            state: Arc::clone(&self.state),
            volume: self.volume.clone(),
            path: path.clone(),
            pos: 0,
            buf: Vec::new(),
            bytes: Arc::clone(&self.bytes_received),
        }))
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        let st = self.lock_state();
        let used = st
            .nodes
            .values()
            .filter(|n| n.kind == EntryKind::File)
            .map(|n| n.size)
            .sum();
        Ok(Quota {
            total: Some(1 << 40),
            used,
        })
    }
}

/// mock 暂存器：分片写 staging 会话，close 才建远端节点。
struct MockStager {
    state: Arc<Mutex<MockState>>,
    volume: VolumeId,
    path: RelPath,
    /// 本次 stager 已从调用方消费的字节数（续传场景调用方从 0 重写，
    /// 已 staging 的前缀按块边界跳过重发）。
    pos: u64,
    buf: Vec<u8>,
    bytes: Arc<AtomicU64>,
}

#[async_trait]
impl UploadStager for MockStager {
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.buf.extend_from_slice(data);
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(sess) = st.sessions.get_mut(&self.path) else {
            return Err(StorageError::Invalid);
        };
        let cs = sess.chunk_size as usize;
        while self.buf.len() >= cs {
            let chunk: Vec<u8> = self.buf.drain(..cs).collect();
            let idx = (self.pos / sess.chunk_size) as usize;
            if idx >= sess.chunks.len() {
                // 「发送到后端」：可观测点计数 + 入会话
                self.bytes.fetch_add(cs as u64, Ordering::SeqCst);
                sess.chunks.push(chunk);
            }
            // else：该块已被先前（被丢弃的）stager staging 完成——差集续传跳过
            self.pos += cs as u64;
        }
        Ok(())
    }

    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(sess) = st.sessions.get_mut(&self.path) else {
            return Err(StorageError::Invalid);
        };
        // 尾块（不足一个整块的部分）也要发送
        if !self.buf.is_empty() {
            let cs = sess.chunk_size;
            let idx = (self.pos / cs) as usize;
            if idx >= sess.chunks.len() {
                self.bytes
                    .fetch_add(self.buf.len() as u64, Ordering::SeqCst);
                sess.chunks.push(std::mem::take(&mut self.buf));
            }
        }
        let hinted = sess.hinted_size;
        let data: Vec<u8> = sess.chunks.concat();
        if let Some(h) = hinted {
            if data.len() as u64 != h {
                st.sessions.remove(&self.path);
                return Err(StorageError::Invalid);
            }
        }
        st.sessions.remove(&self.path);
        Ok(st.insert_node(&self.volume, &self.path, EntryKind::File, data))
    }

    async fn abort(self: Box<Self>) -> Result<(), StorageError> {
        // 显式放弃：staging 会话一并清除（不留「垃圾」，含 RESUME 状态）
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .remove(&self.path);
        Ok(())
    }
}
