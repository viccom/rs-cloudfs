//! 上传写路径（Phase 6 / 123-3）：commit-on-close 暂存器 + 七步提交链。
//!
//! 全链（123-0 写路径腿真机验证形态——任务规格 §5.13 七步严格序）：
//!
//! ```text
//! write(...) 累积到本地 spool（到齐即传：承诺 size 到齐即起链）
//!   ↓ close()（或 write 到齐）
//! MD5 全量预计算（etag 必填无豁免——123-0 ⑥；错 etag 无内容校验，
//!   必须算真 MD5 否则假 etag 未来命中秒传取回错误内容）
//!   ↓ ① upload_request(/b/，首请求不带 duplicate)
//!   （Reuse=true → 秒传命中，零分片零流量，直接跳 size 校验；
//!    5060 → duplicate:2 重发（D4 覆盖真值；Reuse 优先于 5060））
//!   ↓ ② s3_list_upload_parts（小写 storageNode）——差集依据
//!   ↓ ③ s3_repare_upload_parts_batch（官方拼写 repare、大写 StorageNode、
//!      [start,end) 半开）只对缺失分片的连续区间预签名
//!   ↓ ④ 逐分片纯 PUT（传输裸 client：无 123pan 头无表单、显式
//!      Content-Length、超时 max(300s, MB×2s)、幂等重试）
//!   ↓ ⑤ 再 list 确认分片齐
//!   ↓ ⑥ s3_complete_multipart_upload（小写 storageNode）
//!   ↓ ⑦ upload_complete/v2 全量 body（isMultipart:true 恒真——repare
//!      恒 multipart 会话，新单键/false 形态 code=0 静默不入库）→
//!      data.file_info = 真实 FileId + 真 MD5
//!   ↓ size 校验（file_info.size 与本地 spool 一致——不符即错）
//! ```
//!
//! ## 暂存策略与 pan115 的差异
//!
//! 同为「本地 spool + commit-on-close」，差异都在协议面：123 的 etag 是
//! **全量 MD5**（115 是 SHA1+preid）；123 的 complete **不携带分片表**
//! （4 键形——115 的 Complete 要 parts 列表）；123 的会话由**服务端
//! 保留**（同参重发返回同一 UploadId——115 是 resume 端点 + 本地五元组
//! 优先）；123 的 abort **无远端释放端点**（115 有 AbortMultipartUpload
//! ——K75-2 教训的 123 形态：孤儿会话服务端保留，配额影响未知挂账）。
//!
//! ## resume（能力位⑦依据）
//!
//! - **重传路径（spike 钉死 c）**：同路径同内容重走 upload_request →
//!   服务端回**同一 UploadId**（本地五元组丢失亦可恢复）→ list_parts
//!   对账 → 只补缺片；
//! - **本地会话记录**（pan115 SessionStore 同形）：每片 PUT 成功即落
//!   （差集续传资产 + 诊断面）；**键 = `path|size|md5` 三元键**（内容级
//!   身份——强于 pan123-rs 的 mtime(±1s)/size 判据：md5 相等即内容
//!   相等，时间戳校验被包含与取代；且本仓 stager 必须算真 MD5，键内
//!   即内容身份，无额外校验面可加）；
//! - **会话失效**（ListParts NoSuchKey——会话已被 complete 消费）→
//!   重走 upload_request 全量重传**恰一次**；仍失效 → `Io`（不自陷
//!   循环）。
//!
//! ## errno 写侧（§D）
//!
//! 5060 在 writer 面内部以 duplicate=2 消化（**对外 Exists 语义只在
//! mkdir 面**——覆盖语义由 dup2 保证不泄漏）；400「请输入Etag」→ Invalid
//! （驱动恒发真 etag，出现即 bug——桩 400 钉可观测）；-1 rpc → Io 保留
//! 原文（ListParts NoSuchKey 形态被会话失效判据捕获）；OSS PUT 4xx/5xx
//! → §5.15 重试常量。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use cloudkit_storage::{Entry, EntryId, EntryKind, RelPath, StorageError, UploadStager, WriteHint};
use md5::{Digest, Md5};

use crate::api::{Pan123Client, PartsOutcome, RetryConfig, UploadRequestOutcome, UploadTicket};
use crate::pathcache;
use crate::Pan123Driver;

/// 客户端分片定值：5 MiB（123panNextGen 形态——服务端 `SliceSize`
/// "16777216"（16MiB）是上界参考非指令；OSS multipart 最小片 5MiB）。
const PART_MIN: u64 = 5 * 1024 * 1024;
/// OSS 分片数上限。
const PART_MAX_COUNT: u64 = 10_000;
/// 分片 PUT 超时下界（§5.6：`max(300s, MB×2s)`——pan123-rs 实测值照抄）。
const PUT_TIMEOUT_BASE: Duration = Duration::from_secs(300);
/// >64MB 的 complete 后确认等待（§5.13：常量采纳，真机未测挂账 123-5）。
const SETTLE_THRESHOLD: u64 = 64 * 1024 * 1024;
const SETTLE_DELAY: Duration = Duration::from_secs(3);

/// 分片大小：5 MiB 起步；超 10000 片时向上取整到 MiB（pan115
/// `part_size` 同规则——OSS 片数上限的驱动内消化）。
pub fn part_size(size: u64) -> u64 {
    if size <= PART_MIN {
        return PART_MIN;
    }
    let by_count = size.div_ceil(PART_MAX_COUNT);
    let by_count = by_count.div_ceil(1024 * 1024) * 1024 * 1024;
    PART_MIN.max(by_count)
}

/// 分片 PUT 超时：`max(300s, MiB × 2s)`（§5.6）。
pub fn put_timeout(bytes_len: u64) -> Duration {
    let mib = bytes_len / (1024 * 1024);
    PUT_TIMEOUT_BASE.max(Duration::from_secs(mib * 2))
}

/// >64MB 的 complete 后是否需要 settle 等待（§5.13 常量）。
pub fn needs_settle(size: u64) -> bool {
    size > SETTLE_THRESHOLD
}

/// 上传会话记录（resume 差集面；内存 + 可选落盘——pan115
/// `UploadSession` 同形）。
///
/// 键 = `path|size|md5` **三元键**（同路径同大小不同内容不得误复用会话，
/// baidu K7 教训同源；md5 是内容级身份——见模块文档 resume 节）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadSession {
    /// 会话五元组。
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    pub storage_node: String,
    /// 顶层临时 FileId（/v2 完成体的 `fileId` 键值）。
    pub up_file_id: i64,
    pub size: u64,
    /// 已传分片号。
    pub parts: Vec<u32>,
}

/// 会话存储（pan115 SessionStore 同形：内存优先 + 可选磁盘层）。
pub struct SessionStore {
    map: Arc<Mutex<std::collections::HashMap<String, UploadSession>>>,
    dir: Option<PathBuf>,
}

impl SessionStore {
    pub fn new(dir: Option<PathBuf>) -> Self {
        SessionStore {
            map: Arc::new(Mutex::new(std::collections::HashMap::new())),
            dir,
        }
    }

    fn key(path: &str, size: u64, md5: &str) -> String {
        format!("{path}|{size}|{md5}")
    }

    fn file_path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|root| {
            let digest = Md5::digest(key.as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            root.join("pan123_state")
                .join("sessions")
                .join(format!("{hex}.json"))
        })
    }

    /// 取会话（内存优先；miss 且有磁盘层则读文件回填——跨进程腿）。
    pub async fn load(&self, path: &str, size: u64, md5: &str) -> Option<UploadSession> {
        let key = Self::key(path, size, md5);
        if let Some(rec) = self.map.lock().unwrap().get(&key).cloned() {
            return Some(rec);
        }
        let file = self.file_path(&key)?;
        let text = tokio::fs::read_to_string(&file).await.ok()?;
        let rec: UploadSession = serde_json::from_str(&text).ok()?;
        self.map.lock().unwrap().insert(key, rec.clone());
        Some(rec)
    }

    /// 记会话（写内存 + 有磁盘层时原子落盘——tmp+rename）。
    pub async fn save(&self, path: &str, size: u64, md5: &str, rec: &UploadSession) {
        let key = Self::key(path, size, md5);
        self.map.lock().unwrap().insert(key.clone(), rec.clone());
        if let Some(file) = self.file_path(&key) {
            if let Some(parent) = file.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Ok(text) = serde_json::to_string(rec) {
                let tmp = file.with_extension("json.tmp");
                if tokio::fs::write(&tmp, text).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, &file).await;
                }
            }
        }
    }

    /// 清会话（abort / 完成收尾后调用）。
    pub async fn remove(&self, path: &str, size: u64, md5: &str) {
        let key = Self::key(path, size, md5);
        self.map.lock().unwrap().remove(&key);
        if let Some(file) = self.file_path(&key) {
            let _ = tokio::fs::remove_file(&file).await;
        }
    }
}

/// 传输链的中途状态（步骤 ①–⑤——**不含 ⑥⑦**：complete 归 close()，
/// 那是 commit-on-close 的提交点）。
struct TransferState {
    ticket: UploadTicket,
    /// 服务端 list 确认在册的分片号。
    done: Vec<u32>,
    /// 秒传命中（close 只需 size 复核收工）。
    rapid_file_id: Option<i64>,
    /// 本地内容的真 MD5（close/abort 的会话键与会话落盘复用——避免
    /// 二遍哈希）。
    etag: String,
}

/// 上传暂存器（[`UploadStager`] 实现；commit-on-close）。
pub struct Pan123Stager {
    client: Arc<Pan123Client>,
    volume: cloudkit_storage::VolumeId,
    /// 挂载点（目标父目录 cid）。
    parent_cid: String,
    file_name: String,
    /// 目标卷内路径（Entry 产出 + 会话键）。
    rel_path: RelPath,
    /// spool 文件（本地临时；Drop 时清理）。
    spool: PathBuf,
    written: u64,
    hint: WriteHint,
    sessions: Arc<SessionStore>,
    /// 分片 PUT 重试参数（§5.15——驱动参数注入面同源）。
    retry: RetryConfig,
    /// 目标父目录缓存（close 落库后就近失效）。
    paths: Arc<pathcache::PathCache>,
    /// 显式 abort 标记（Drop 时不留 spool）。
    aborted: bool,
    /// 传输进度（**到齐即传**：写入量达到承诺 size 的那一刻就起链——
    /// resume 差集观测点在 close 前成立）。`None` 表示尚未起链。
    transfer: Option<TransferState>,
}

impl Pan123Stager {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Arc<Pan123Client>,
        volume: cloudkit_storage::VolumeId,
        parent_cid: String,
        file_name: String,
        rel_path: RelPath,
        spool: PathBuf,
        hint: WriteHint,
        sessions: Arc<SessionStore>,
        retry: RetryConfig,
        paths: Arc<pathcache::PathCache>,
    ) -> Self {
        Pan123Stager {
            client,
            volume,
            parent_cid,
            file_name,
            rel_path,
            spool,
            written: 0,
            hint,
            sessions,
            retry,
            paths,
            aborted: false,
            transfer: None,
        }
    }

    /// 步骤 ①：upload_request（含 5060 → duplicate=2 重发 + 会话失效
    /// 重试恰一次）。产出工作会话（或秒传终态）+ 服务端在册分片。
    async fn establish_session(&mut self, etag: &str) -> Result<TransferState, StorageError> {
        let size = self.written;
        let parent: i64 = self.parent_cid.parse().unwrap_or(0);
        for attempt in 0..=1u32 {
            // 首请求不带 duplicate（§5.13）；5060 → duplicate:2 重发
            // （D4：2=同 FileId 原地覆盖——**绝不发 1**）。
            let mut outcome = self
                .client
                .upload_request_file(parent, &self.file_name, size, etag, None)
                .await?;
            if matches!(outcome, UploadRequestOutcome::Conflict) {
                outcome = self
                    .client
                    .upload_request_file(parent, &self.file_name, size, etag, Some(2))
                    .await?;
            }
            let ticket = match outcome {
                // 秒传命中（Reuse 优先于 5060——同内容直接入库不冲突）：
                // 零分片零流量，close 复核收工。
                UploadRequestOutcome::Rapid { file_id } => {
                    return Ok(TransferState {
                        ticket: UploadTicket {
                            up_file_id: 0,
                            bucket: String::new(),
                            key: String::new(),
                            upload_id: String::new(),
                            storage_node: String::new(),
                            slice_size: None,
                        },
                        done: Vec::new(),
                        rapid_file_id: Some(file_id),
                        etag: etag.to_string(),
                    })
                }
                UploadRequestOutcome::Conflict => {
                    // duplicate=2 后仍 5060：异常形态（同名目录等）——
                    // 上抛让调用方看到后端原文。
                    return Err(StorageError::Unavailable(
                        "upload_request: still 5060 after duplicate=2".into(),
                    ));
                }
                UploadRequestOutcome::Ticket(t) => t,
            };
            // 步骤 ②：list 对账（服务端真值——本地记录只是提示）。
            match self.client.s3_list_parts(&ticket).await? {
                PartsOutcome::Parts(parts) => {
                    return Ok(TransferState {
                        done: parts.into_iter().map(|(n, _)| n).collect(),
                        ticket: *ticket,
                        rapid_file_id: None,
                        etag: etag.to_string(),
                    });
                }
                // 会话失效（已被 complete 消费）→ 重走 upload_request
                // 全量重传；重试一次仍失效 → Io（不自陷循环）。
                PartsOutcome::SessionGone => {
                    if attempt == 1 {
                        return Err(StorageError::Io(
                            "pan123 upload session gone (ListParts NoSuchKey) after one \
                             re-request: refusing to loop"
                                .into(),
                        ));
                    }
                    tracing::warn!(
                        target: "ck_pan123::upload",
                        upload_id = %ticket.upload_id,
                        "upload session consumed server-side: re-requesting a fresh one"
                    );
                }
            }
        }
        unreachable!("the loop returns on its second iteration")
    }

    /// 传输链（到齐即传与 close 共用的同一段）：hash → 步骤 ①②（会话
    /// 建立 + 对账）→ ③④（只补缺片）→ ⑤（确认）。**不含 ⑥⑦**。
    async fn run_transfer(&mut self) -> Result<(), StorageError> {
        let size = self.written;
        let etag = self.hash_md5().await?;
        let state = self.establish_session(&etag).await?;
        if state.rapid_file_id.is_some() {
            self.transfer = Some(state);
            return Ok(());
        }
        let ticket = state.ticket.clone();
        let present = state.done.clone();
        self.transfer = Some(state);
        tracing::debug!(
            target: "ck_pan123::upload",
            rapid_hint = self.hint.rapid_upload,
            size,
            upload_id = %ticket.upload_id,
            present_parts = present.len(),
            "upload session established (rapid detection is server-side via the etag)"
        );

        // 步骤 ③④：缺失分片的连续区间批量预签名（含空洞时多余 URL 不
        // 用即可——单次 API 调用优先）+ 逐片 PUT（每片即落）。
        let part_bytes = part_size(size);
        let total_parts = size.div_ceil(part_bytes).max(1) as u32;
        let missing: Vec<u32> = (1..=total_parts).filter(|n| !present.contains(n)).collect();
        if !missing.is_empty() {
            let start = *missing.first().expect("non-empty");
            let end = missing.last().expect("non-empty") + 1; // [start, end) 半开
            let urls = self.client.s3_repare_presign(&ticket, start, end).await?;
            let http = self.client.transfer_http().clone();
            for n in &missing {
                let offset = (*n - 1) as u64 * part_bytes;
                let len = part_bytes.min(size - offset);
                let body = self.spool_range(offset, len).await?;
                let url = urls.get(n).ok_or_else(|| {
                    StorageError::Unavailable(format!(
                        "s3_repare: presigned URL for part {n} missing"
                    ))
                })?;
                put_part_with_retry(&http, url, &body, &self.retry).await?;
                if let Some(t) = self.transfer.as_mut() {
                    t.done.push(*n);
                    t.done.sort_unstable();
                    t.done.dedup();
                }
                // 每片即落（差集续传资产——崩溃/失败的下一腿从会话续）。
                self.persist_session().await;
            }
        }

        // 步骤 ⑤：再 list 确认分片齐。
        let confirmed = match self.client.s3_list_parts(&ticket).await? {
            PartsOutcome::Parts(parts) => parts,
            PartsOutcome::SessionGone => {
                return Err(StorageError::Io(
                    "pan123 upload session gone during the confirm list".into(),
                ))
            }
        };
        let missing_after: Vec<u32> = (1..=total_parts)
            .filter(|n| !confirmed.iter().any(|(pn, _)| pn == n))
            .collect();
        if !missing_after.is_empty() {
            return Err(StorageError::Unavailable(format!(
                "upload parts missing after the PUT stage: {missing_after:?}"
            )));
        }
        if let Some(t) = self.transfer.as_mut() {
            t.done = confirmed.into_iter().map(|(n, _)| n).collect();
        }
        self.persist_session().await;
        Ok(())
    }

    /// 会话落盘（五元组 + 已传分片；etag 取传输态——避免二遍哈希）。
    async fn persist_session(&self) {
        let Some(t) = self.transfer.as_ref() else {
            return;
        };
        if t.rapid_file_id.is_some() {
            return; // 秒传终态无会话
        }
        let rec = UploadSession {
            bucket: t.ticket.bucket.clone(),
            key: t.ticket.key.clone(),
            upload_id: t.ticket.upload_id.clone(),
            storage_node: t.ticket.storage_node.clone(),
            up_file_id: t.ticket.up_file_id,
            size: self.written,
            parts: t.done.clone(),
        };
        self.sessions
            .save(self.rel_path.as_str(), self.written, &t.etag, &rec)
            .await;
    }

    /// 读回 spool 的区间字节（分片切分）。
    async fn spool_range(&self, start: u64, len: u64) -> Result<Vec<u8>, StorageError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(&self.spool)
            .await
            .map_err(|e| StorageError::Io(format!("spool open: {e}")))?;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| StorageError::Io(format!("spool seek: {e}")))?;
        let mut buf = vec![0u8; len as usize];
        file.read_exact(&mut buf)
            .await
            .map_err(|e| StorageError::Io(format!("spool read: {e}")))?;
        Ok(buf)
    }

    /// 全量 MD5（小写 hex——etag 必填真值；一遍哈希 IO 代价照付）。
    ///
    /// `hint.content_hash` **不采信**：错哈希 → 假 etag → 未来秒传命中
    /// 会取回错误内容（123-0 ⑥ 推论）——本地重算是唯一可信源。
    async fn hash_md5(&self) -> Result<String, StorageError> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&self.spool)
            .await
            .map_err(|e| StorageError::Io(format!("spool open: {e}")))?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| StorageError::Io(format!("spool read: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let out = hasher.finalize();
        Ok(out.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// 完成态的本地收尾：会话清除 + spool 清理 + 父目录缓存失效。
    async fn finish_local(&self, etag: &str) {
        self.sessions
            .remove(self.rel_path.as_str(), self.written, etag)
            .await;
        let _ = tokio::fs::remove_file(&self.spool).await;
        self.paths.invalidate(&self.parent_cid).await;
    }
    /// size 复核（数据完整性纪律：不符即错，绝不静默——pan115
    /// zero-byte bug 类的唯一防线）。
    fn verify_size(remote: i64, local: u64, stage: &str) -> Result<u64, StorageError> {
        if remote < 0 || remote as u64 != local {
            return Err(StorageError::Unavailable(format!(
                "upload size mismatch at {stage}: local {local} vs remote {remote} \
                 (refusing to report success)"
            )));
        }
        Ok(local)
    }

    fn entry_of(&self, file_id: i64, size: u64, mtime: i64) -> Entry {
        Entry {
            id: EntryId::new(
                self.volume.clone(),
                cloudkit_storage::BackendHandle::new(file_id.to_string()),
            ),
            path: self.rel_path.clone(),
            kind: EntryKind::File,
            size,
            mtime: mtime as f64,
        }
    }
}

impl Drop for Pan123Stager {
    fn drop(&mut self) {
        // 会话保留（resume 声明）：Drop 只清 spool；服务端会话与本地
        // 会话记录保留（差集续传资产——服务端保留实证 + 每片即落）。
        // 显式 abort 才清本地会话记录（abort 里做）。
        if !self.aborted {
            let _ = std::fs::remove_file(&self.spool);
        }
    }
}

#[async_trait]
impl UploadStager for Pan123Stager {
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        // u64 溢出守卫（checked_add 语义——回绕不如拒绝）。
        self.written
            .checked_add(data.len() as u64)
            .ok_or(StorageError::Invalid)?;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&self.spool)
            .await
            .map_err(|e| StorageError::Io(format!("spool append: {e}")))?;
        file.write_all(data)
            .await
            .map_err(|e| StorageError::Io(format!("spool write: {e}")))?;
        self.written += data.len() as u64;
        // 超承诺：立刻失败（不再拉数据做无用工）。
        if let Some(hinted) = self.hint.size {
            if self.written > hinted {
                return Err(StorageError::Invalid);
            }
        }
        // **到齐即传**：承诺 size 到齐的那一次 write 之后立刻推传输链
        // （①–⑤；⑥⑦ 归 close）——resume 的差集观测点在 close 前成立。
        if let Some(hinted) = self.hint.size {
            if self.written == hinted && self.transfer.is_none() {
                self.run_transfer().await?;
            }
        }
        Ok(())
    }

    /// 提交（commit-on-close 的提交点）：传输链（若未起）→ ⑥ s3_complete
    /// → ⑦ upload_complete/v2 →（>64MB settle 等待）→ size 复核。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        // WriteHint 契约：承诺与实际不符 → Invalid（vfs 层据此拒绝）。
        if let Some(hinted) = self.hint.size {
            if self.written != hinted {
                return Err(StorageError::Invalid);
            }
        }
        let size = self.written;
        if self.transfer.is_none() {
            self.run_transfer().await?;
        }
        let t = self.transfer.take().expect("transfer state after run");
        let etag = t.etag.clone();

        // ---- 秒传命中（Reuse）：零数据上传，直接 size 复核收工。
        if let Some(file_id) = t.rapid_file_id {
            let row = self
                .client
                .file_info(file_id)
                .await?
                .ok_or(StorageError::NotFound)?;
            let remote = Self::verify_size(row.size, size, "Reuse read-back")?;
            self.finish_local(&etag).await;
            return Ok(self.entry_of(file_id, remote, row.update_at));
        }

        // ---- 步骤 ⑥：s3_complete（-1 MalformedXML 无害先例在 api 层
        //      容忍）。
        self.client.s3_complete_multipart(&t.ticket).await?;
        // ---- 步骤 ⑦：upload_complete/v2 全量 body（isMultipart:true
        //      恒真）→ data.file_info = 真实 FileId + 真 MD5。
        let row = self.client.upload_complete_v2(&t.ticket, size).await?;
        // >64MB：complete 后 settle 等待再确认（§5.13 常量——真机未测
        // 挂账 123-5）。
        if needs_settle(size) {
            tokio::time::sleep(SETTLE_DELAY).await;
        }
        let size = Self::verify_size(row.size, size, "upload_complete/v2")?;
        self.finish_local(&etag).await;
        Ok(self.entry_of(row.file_id, size, row.update_at))
    }

    /// 放弃暂存：清本地会话 + 删 spool。**远端释放尽力而为如实记录**：
    /// 123 web API 无已知的会话释放端点（对照 pan115 K75-2 的
    /// AbortMultipartUpload——123 的 s3_* 面没有 abort 形态），孤儿
    /// multipart 会话服务端保留（配额影响未知，挂账 123-5 真机观察）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        self.aborted = true;
        let mut etag: Option<String> = None;
        if let Some(t) = self.transfer.take() {
            etag = Some(t.etag.clone());
            if t.rapid_file_id.is_none() && !t.ticket.upload_id.is_empty() {
                tracing::warn!(
                    target: "ck_pan123::upload",
                    upload_id = %t.ticket.upload_id,
                    "abort: the orphan multipart session remains server-side (the 123 web \
                     API surface exposes no abort endpoint; server-side quota impact \
                     unknown — tracked for 123-5)"
                );
            }
        }
        // 会话键需要 md5——传输未起时 spool 尚在，可算（失败不阻塞清理）。
        let etag = match etag {
            Some(e) => e,
            None => match self.hash_md5().await {
                Ok(e) => e,
                Err(_) => {
                    let _ = tokio::fs::remove_file(&self.spool).await;
                    return Ok(());
                }
            },
        };
        self.sessions
            .remove(self.rel_path.as_str(), self.written, &etag)
            .await;
        let _ = tokio::fs::remove_file(&self.spool).await;
        Ok(())
    }
}

/// 分片 PUT（传输裸面：仅 UA、无 123pan 头、显式 Content-Length、超时
/// 自适应）+ §5.15 幂等重试：传输错误/5xx 普通类 ≤3、429 限流类 ≤6、
/// `Retry-After` 头优先 clamp 1–60s、指数退避封顶 + 抖动；其余 4xx
/// 终态。
async fn put_part_with_retry(
    http: &reqwest::Client,
    url: &str,
    body: &[u8],
    retry: &RetryConfig,
) -> Result<(), StorageError> {
    let mut limited_retries = 0u32;
    let mut ordinary_retries = 0u32;
    loop {
        let attempt = http
            .put(url)
            .header("content-length", body.len().to_string())
            .body(body.to_vec())
            .timeout(put_timeout(body.len() as u64))
            .send()
            .await;
        match attempt {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 429 || (500..=599).contains(&status) {
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let is_limited = status == 429;
                    let budget = if is_limited {
                        &mut limited_retries
                    } else {
                        &mut ordinary_retries
                    };
                    let max = if is_limited {
                        retry.limited_max
                    } else {
                        retry.ordinary_max
                    };
                    if *budget >= max {
                        return Err(if is_limited {
                            StorageError::RateLimited { retry_after }
                        } else {
                            StorageError::Unavailable(format!("part PUT HTTP {status} persisted"))
                        });
                    }
                    let delay = match retry_after {
                        Some(ra) => ra.clamp(retry.retry_after_min, retry.retry_after_max),
                        None => retry.backoff(*budget),
                    };
                    *budget += 1;
                    tracing::debug!(
                        target: "ck_pan123::upload",
                        status,
                        ?delay,
                        "retryable part PUT: backing off"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(StorageError::Unavailable(format!("part PUT HTTP {status}")));
            }
            Err(e) => {
                if ordinary_retries >= retry.ordinary_max {
                    return Err(StorageError::Unavailable(format!(
                        "part PUT transport: {}",
                        e.without_url()
                    )));
                }
                let delay = retry.backoff(ordinary_retries);
                ordinary_retries += 1;
                tracing::debug!(
                    target: "ck_pan123::upload",
                    ?delay,
                    "part PUT transport failure: backing off"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// writer 入口（lib.rs 的薄委托面）
// ---------------------------------------------------------------------------

/// 解析父目录并逐级隐式创建（trait 契约：写入路径的缺失父目录由驱动
/// 创建）；返回父 cid（pan115 ensure_parents 同形——123 的 mkdir 面）。
async fn ensure_parents(
    client: &Pan123Client,
    root_cid: &str,
    paths: &pathcache::PathCache,
    parent: &RelPath,
) -> Result<String, StorageError> {
    let mut cid = root_cid.to_string();
    for comp in parent.components() {
        let comp = comp.to_string();
        let existing = match paths.get_child(&cid, &comp).await {
            Some(row) => Some(row),
            None => {
                let rows = pathcache::list_all(client, &cid).await?;
                paths.put_dir(&cid, &rows).await;
                paths.get_child(&cid, &comp).await
            }
        };
        match existing {
            Some(row) if row.is_dir() => cid = row.file_id.to_string(),
            Some(_) => return Err(StorageError::NotFound), // 父链上有文件
            None => {
                let fid = client.mkdir(cid.parse().unwrap_or(0), &comp).await?;
                paths.invalidate(&cid).await;
                cid = fid.to_string();
            }
        }
    }
    Ok(cid)
}

/// spool 文件名唯一标签（进程内计数 + 时间；不引 uuid 依赖——pan115
/// 同款）。
fn unique_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}-{n:x}")
}

/// spool 目录解析（`Pan123Params::sessions_dir` 缺省 = 系统临时目录；
/// 生产装配给卷家目录，K21 锚形态——pan115 同款）。
pub(crate) fn default_spool_dir(params: &crate::Pan123Params) -> PathBuf {
    match &params.sessions_dir {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir(),
    }
}

/// 驱动侧写入口（[`Pan123Driver::writer`] 的实现体——lib.rs 保持薄）。
pub(crate) async fn writer(
    driver: &Pan123Driver,
    path: &RelPath,
    hint: &WriteHint,
) -> Result<Box<dyn UploadStager>, StorageError> {
    if path.is_root() {
        return Err(StorageError::Invalid);
    }
    let parent = path.parent().unwrap_or_else(RelPath::root);
    let file_name = path
        .components()
        .last()
        .ok_or(StorageError::Invalid)?
        .to_string();
    // 父目录必须存在或可隐式创建（trait 契约）。
    let parent_cid = ensure_parents(
        driver.client_arc(),
        driver.root_cid(),
        driver.paths_arc(),
        &parent,
    )
    .await?;
    // 目标是目录 → Invalid（写入方不会这样调用，防御契约）。缓存未列过
    // 目标目录时**现列预检**（mkdir/rename 同款冷预检纪律，M-S4 同源）。
    let target_row = match driver.paths_arc().get_child(&parent_cid, &file_name).await {
        Some(row) => Some(row),
        None => {
            let rows = pathcache::list_all(driver.client_arc(), &parent_cid).await?;
            driver.paths_arc().put_dir(&parent_cid, &rows).await;
            driver.paths_arc().get_child(&parent_cid, &file_name).await
        }
    };
    if target_row.is_some_and(|row| row.is_dir()) {
        return Err(StorageError::Invalid);
    }
    let spool_dir = default_spool_dir(driver.params());
    let spool = spool_dir.join(format!("pan123-upload-{}.part", unique_tag()));
    // 防御：spool 父目录缺失即建（生产装配的卷家目录存在；一次幂等
    // create_dir_all 换 K21 目录被外因移除时的可恢复性）。
    tokio::fs::create_dir_all(&spool_dir)
        .await
        .map_err(|e| StorageError::Io(format!("spool dir create: {e}")))?;
    tokio::fs::File::create(&spool)
        .await
        .map_err(|e| StorageError::Io(format!("spool create: {e}")))?;
    Ok(Box::new(Pan123Stager::new(
        driver.client_arc().clone(),
        driver.volume_ref().clone(),
        parent_cid,
        file_name,
        path.clone(),
        spool,
        hint.clone(),
        driver.sessions_arc().clone(),
        driver.retry_config(),
        driver.paths_arc().clone(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §5.6 超时常量：`max(300s, MB×2s)`——pan123-rs 实测值照抄。
    #[test]
    fn put_timeout_is_max_of_base_and_two_secs_per_mib() {
        assert_eq!(put_timeout(0), Duration::from_secs(300));
        assert_eq!(put_timeout(5 * 1024 * 1024), Duration::from_secs(300));
        assert_eq!(put_timeout(64 * 1024 * 1024), Duration::from_secs(300));
        // 150MiB → 300s（恰下界）；200MiB → 400s（超界生效）。
        assert_eq!(put_timeout(150 * 1024 * 1024), Duration::from_secs(300));
        assert_eq!(put_timeout(200 * 1024 * 1024), Duration::from_secs(400));
    }

    /// §5.13 分片定值：5MiB 客户端定值（服务端 SliceSize 16MiB 是上界
    /// 参考）；>10000 片时向上取整到 MiB。
    #[test]
    fn part_size_is_five_mib_with_adaptive_scaling() {
        assert_eq!(part_size(0), PART_MIN);
        assert_eq!(part_size(6 * 1024 * 1024), PART_MIN, "6MiB -> 5+1MiB 两片");
        let fifty_gib = 50_u64 * 1024 * 1024 * 1024;
        let ps = part_size(fifty_gib);
        assert!(
            ps > PART_MIN && ps.is_multiple_of(1024 * 1024),
            "10k-part bound scales up to whole MiBs: {ps}"
        );
        assert!(fifty_gib.div_ceil(ps) <= PART_MAX_COUNT);
    }

    /// §5.13 settle 常量：>64MB 才等 3s（常量采纳，真机未测挂账）。
    #[test]
    fn settle_threshold_and_delay_constants() {
        assert!(!needs_settle(64 * 1024 * 1024));
        assert!(needs_settle(64 * 1024 * 1024 + 1));
        assert_eq!(SETTLE_THRESHOLD, 64 * 1024 * 1024);
        assert_eq!(SETTLE_DELAY, Duration::from_secs(3));
    }

    /// 三元键会话存储：save/load/remove 往返 + 磁盘层跨实例可见。
    #[tokio::test]
    async fn session_store_roundtrips_through_the_disk_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(Some(dir.path().to_path_buf()));
        let rec = UploadSession {
            bucket: "b".into(),
            key: "k".into(),
            upload_id: "u-1".into(),
            storage_node: "n".into(),
            up_file_id: 1_799_000_000_001,
            size: 42,
            parts: vec![1, 2],
        };
        store.save("a/x.bin", 42, "abc", &rec).await;
        // 同键不同内容不串味（三元键）。
        assert!(store.load("a/x.bin", 42, "other-md5").await.is_none());
        assert!(store.load("a/x.bin", 41, "abc").await.is_none());
        // 跨实例（磁盘层回填——崩溃后新进程的形态）。
        let second = SessionStore::new(Some(dir.path().to_path_buf()));
        let loaded = second.load("a/x.bin", 42, "abc").await.expect("disk layer");
        assert_eq!(loaded.upload_id, "u-1");
        assert_eq!(loaded.parts, vec![1, 2]);
        store.remove("a/x.bin", 42, "abc").await;
        // 每实例有独立内存层——remove 的核验用全新实例（磁盘层已清）。
        let third = SessionStore::new(Some(dir.path().to_path_buf()));
        assert!(third.load("a/x.bin", 42, "abc").await.is_none());
    }

    /// 纯内存形态（sessions_dir = None——测试缺省）也可用。
    #[tokio::test]
    async fn session_store_memory_only_when_dir_is_none() {
        let store = SessionStore::new(None);
        let rec = UploadSession {
            bucket: String::new(),
            key: String::new(),
            upload_id: "u".into(),
            storage_node: String::new(),
            up_file_id: 1,
            size: 0,
            parts: vec![],
        };
        store.save("p", 0, "m", &rec).await;
        assert!(store.load("p", 0, "m").await.is_some());
    }

    /// models::FileEntry 在 file_info（/v2 snake_case 键内层双拼条目）
    /// 形态上的解析（api 层复用条目模型的钉）。
    #[test]
    fn file_entry_parses_the_v2_file_info_shape() {
        let row: crate::models::FileEntry = serde_json::from_str(
            r#"{"FileId":64379791,"FileName":"a.bin","Type":0,"Size":12582912,
                "Etag":"0123abcd","S3KeyFlag":"4006416717-0","Trashed":false,
                "UpdateAt":"2026-09-20T16:30:00+08:00","CreateAt":"2026-09-20T16:30:00+08:00"}"#,
        )
        .expect("file_info row parses");
        assert_eq!(row.file_id, 64379791);
        assert_eq!(row.size, 12582912);
        assert_eq!(row.etag, "0123abcd");
    }
}
