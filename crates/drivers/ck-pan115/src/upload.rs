//! 上传写路径（Phase 5 / 115-3）：commit-on-close 暂存器 + 全链。
//!
//! 全链（K69.8 真机验证形态）：
//!
//! ```text
//! write(...) 累积到本地 spool（内存阈值后落盘临时文件）
//!   ↓ close()
//! SHA1 全量 + preid（前 128KiB）
//!   ↓
//! /open/upload/init               → status==2 秒传命中（终态）
//!   （sign_key/sign_check 出现 → 算区间 SHA1 → 回带重发 init；
//!    K69.2：用户级挑战，常态路径）
//!   ↓ status==1
//! /open/upload/get_token          → STS
//!   ↓
//! PutObject（单分片）或 Initiate?sequential + UploadPart×N
//!   ↓
//! Complete(+callback 头) / Put(callback 头)
//!   ↓
//! get_info 复核远端 size（硬纪律 5：0 字节 bug 的唯一持久修复）
//! ```
//!
//! 暂存策略：**spool 到本地临时文件**（而非全内存）——115 的上传需要
//! 全量 SHA1 与 preid，且 OSS 分片要重复读同一数据；流式直传需要
//! 「先知道 hash 再传」的两遍读，本地 spool 是唯一无网络浪费的形态
//! （PFCS「到齐即传」同源约束：115 的 init 会话锁定 block_list）。
//!
//! commit-on-close（断言①）：close 前目标路径对 list/stat 不可见——
//! 天然成立（115 的对象只在 complete/callback 后才存在）。
//!
//! Drop 语义（resume 能力）：Drop 保留上传会话（uploadId + 已传分片
//! 表，供差集续传）；显式 `abort` 清会话 + 删 spool（不留远端垃圾
//! ——未完的 OSS multipart 不是可见对象，但会话记录会被清除）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cloudkit_storage::{Entry, EntryId, EntryKind, RelPath, StorageError, UploadStager, WriteHint};
use sha1::{Digest, Sha1};

use crate::api::{InitResp, Pan115Client};
use crate::oss::{self, OssCtx};
use crate::{pathcache, Pan115Driver};

/// preid = 前 128 KiB 的 SHA1（K69.6/115-plus-desktop local.rs 同值）。
const PREID_LEN: u64 = 128 * 1024;
/// OSS 分片下限（5 MiB——OSS 协议约束）与 10000 片上限。
const PART_MIN: u64 = 5 * 1024 * 1024;
const PART_MAX_COUNT: u64 = 10_000;

/// 上传会话记录（resume 差集面；内存 + 可选落盘）。
///
/// 键 = `path|size|sha1`（**三元键**——同路径同大小不同内容不得误复用
/// 会话，baidu K7 教训同源）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadSession {
    pub pick_code: String,
    pub target: String,
    pub bucket: String,
    pub object: String,
    /// 已传分片：part_number → (etag, size)。
    pub parts: Vec<(u32, String, i64)>,
    pub upload_id: Option<String>,
    /// callback 材料（JSON 字符串对；K69.8）。
    #[serde(default)]
    pub callback: Option<(String, String)>,
}

/// 会话存储（baidu SessionStore 同形：内存优先 + 可选磁盘层）。
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

    fn key(path: &str, size: u64, sha1: &str) -> String {
        format!("{path}|{size}|{sha1}")
    }

    fn file_path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|root| {
            let mut h = Sha1::new();
            h.update(key.as_bytes());
            let hex = format!("{:x}", h.finalize());
            root.join("pan115_state")
                .join("sessions")
                .join(format!("{hex}.json"))
        })
    }

    /// 取会话（内存优先；miss 且有磁盘层则读文件回填——跨进程腿）。
    pub async fn load(&self, path: &str, size: u64, sha1: &str) -> Option<UploadSession> {
        let key = Self::key(path, size, sha1);
        if let Some(rec) = self.map.lock().unwrap().get(&key).cloned() {
            return Some(rec);
        }
        let file = self.file_path(&key)?;
        let text = tokio::fs::read_to_string(&file).await.ok()?;
        let rec: UploadSession = serde_json::from_str(&text).ok()?;
        self.map.lock().unwrap().insert(key, rec.clone());
        Some(rec)
    }

    /// 记会话（写内存 + 有磁盘层时原子落盘）。
    pub async fn save(&self, path: &str, size: u64, sha1: &str, rec: &UploadSession) {
        let key = Self::key(path, size, sha1);
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

    /// 清会话（abort / 秒传终态后调用）。
    pub async fn remove(&self, path: &str, size: u64, sha1: &str) {
        let key = Self::key(path, size, sha1);
        self.map.lock().unwrap().remove(&key);
        if let Some(file) = self.file_path(&key) {
            let _ = tokio::fs::remove_file(&file).await;
        }
    }
}

/// 上传暂存器（[`UploadStager`] 实现；commit-on-close）。
pub struct Pan115Stager {
    /// 驱动侧共享面。
    client: Arc<Pan115Client>,
    volume: cloudkit_storage::VolumeId,
    /// 挂载点（目标父目录 cid）。
    parent_cid: String,
    file_name: String,
    /// 目标卷内路径（Entry 产出用）。
    rel_path: RelPath,
    /// spool 文件（本地临时；Drop 时清理）。
    spool: PathBuf,
    written: u64,
    hint: WriteHint,
    sessions: Arc<SessionStore>,
    /// 调用方的秒传提示（hint.rapid_upload）——**观察面**：115 的秒传
    /// 探测是服务端在 init（SHA1 载荷）里自动完成的，驱动无需额外动
    /// 作；该位进 run_transfer 的 debug 日志（conformance 桩用它取证）。
    rapid: bool,
    /// 显式 abort 标记（Drop 时不留 spool）。
    aborted: bool,
    /// 传输进度（**到齐即传**：写入量达到承诺 size 的那一刻就起
    /// 传输链——115 的 init 会话锁定全量 block_list，视频/网盘侧同款
    /// 「到齐即传」形态；也让 conformance ⑦ 的差集观测点在 close 前
    /// 就成立）。`None` 表示尚未起链。
    transfer: Option<TransferState>,
}

/// 传输链的中途状态（hash→init/resume→get_token→分片；**不含 complete**
/// ——complete 归 close()，那是 commit-on-close 的提交点）。
struct TransferState {
    fileid: String,
    /// init/resume 的 pick_code（会话身份）。
    pick_code: String,
    bucket: String,
    object: String,
    callback: Option<crate::api::UploadCallback>,
    /// `None` = 单分片 PutObject 路径（无 multipart 会话）。
    upload_id: Option<String>,
    /// 已传分片（part_number, etag, size）。
    done: Vec<(u32, String, i64)>,
    /// 单分片路径：对象体已 PUT（close 只需复核）。
    put_done: bool,
    /// 秒传命中（close 只需复核）。
    rapid_file_id: Option<String>,
}

impl Pan115Stager {
    /// 构造（writer 面调用；spool 文件即刻创建——write 的到达数据全部
    /// 落在这里，close 时才知道全貌）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Arc<Pan115Client>,
        volume: cloudkit_storage::VolumeId,
        parent_cid: String,
        file_name: String,
        rel_path: RelPath,
        spool: PathBuf,
        hint: WriteHint,
        sessions: Arc<SessionStore>,
    ) -> Self {
        let rapid = hint.rapid_upload;
        Pan115Stager {
            client,
            volume,
            parent_cid,
            file_name,
            rel_path,
            spool,
            written: 0,
            hint,
            sessions,
            rapid,
            aborted: false,
            transfer: None,
        }
    }

    /// 传输链（到齐即传与 close 共用的同一段）：hash → init/resume →
    /// get_token → PutObject / multipart 分片。**不 complete**。
    ///
    /// 失败语义：错误原样返回；调用方（write/close）决定上抛或留待
    /// close 重试（会话里已落的分片是续传资产）。
    async fn run_transfer(&mut self) -> Result<(), StorageError> {
        let size = self.written;
        tracing::debug!(
            target: "ck_pan115::upload",
            rapid_hint = self.rapid,
            size,
            "starting the transfer chain (rapid detection is server-side)"
        );
        let (fileid, preid) = self.hashes().await?;
        let target = format!("U_1_{}", self.parent_cid);
        let session_key = self.rel_path.as_str().to_string();
        let session = self.sessions.load(&session_key, size, &fileid).await;

        // ---- 会话复用臂：/open/upload/resume（返回同一 object/uploadId
        //      上下文——115 的续传端点；失败回退完整 init，
        //      115-plus-desktop api.rs:328-346 同款顺序）
        if let Some(rec) = session.clone() {
            if let Ok(resp) = self
                .client
                .upload_resume(size as i64, &target, &fileid, &rec.pick_code)
                .await
            {
                if resp.object == rec.object {
                    let sts = self.client.get_token().await?;
                    let http = self.client.http();
                    let ctx = OssCtx {
                        endpoint: sts.endpoint.clone(),
                        bucket: resp.bucket.clone(),
                        object: resp.object.clone(),
                        access_key_id: sts.access_key_id.clone(),
                        access_key_secret: sts.access_key_secret.clone(),
                        security_token: sts.security_token.clone(),
                    };
                    let callback = resp.callback.clone().or_else(|| {
                        rec.callback
                            .as_ref()
                            .map(|(c, v)| crate::api::UploadCallback {
                                callback: c.clone(),
                                callback_var: v.clone(),
                            })
                    });
                    let upload_id = rec.upload_id.clone();
                    let mut done = rec.parts.clone();
                    if let Some(uid) = upload_id.as_deref() {
                        // 远端真值对账（本地表只是提示；崩溃后的手滑
                        // 场景自愈——ListParts 以 OSS 为准）。
                        match oss::list_parts(http, &ctx, uid).await {
                            Ok(parts) => {
                                done = parts
                                    .iter()
                                    .map(|p| (p.part_number, p.etag.clone(), p.size))
                                    .collect();
                            }
                            Err(e) if e.is_no_such_upload() => {
                                let id = oss::initiate_multipart(http, &ctx)
                                    .await
                                    .map_err(oss_to_storage)?;
                                self.transfer = Some(TransferState {
                                    fileid,
                                    pick_code: resp.pick_code.clone(),
                                    bucket: resp.bucket.clone(),
                                    object: resp.object.clone(),
                                    callback,
                                    upload_id: Some(id),
                                    done: Vec::new(),
                                    put_done: false,
                                    rapid_file_id: None,
                                });
                                return self.upload_missing_parts().await;
                            }
                            Err(e) => return Err(oss_to_storage(e)),
                        }
                    }
                    self.transfer = Some(TransferState {
                        fileid,
                        pick_code: resp.pick_code.clone(),
                        bucket: resp.bucket.clone(),
                        object: resp.object.clone(),
                        callback,
                        upload_id,
                        done,
                        put_done: false,
                        rapid_file_id: None,
                    });
                    if self
                        .transfer
                        .as_ref()
                        .is_some_and(|t| t.upload_id.is_some())
                    {
                        return self.upload_missing_parts().await;
                    }
                    // 会话无 uploadId（单分片形态）：整对象重传（幂等）。
                    return self.put_object_path().await;
                }
            }
            // resume 不可用：清陈旧会话，走完整 init。
            self.sessions.remove(&session_key, size, &fileid).await;
        }

        // ---- 完整 init（含二次认证循环；K69.2 常态路径，上限 3 轮）
        let mut init = self
            .client
            .upload_init(
                &self.file_name,
                size as i64,
                &target,
                &fileid,
                &preid,
                None,
                None,
                None,
            )
            .await?;
        let mut rounds = 0;
        while needs_secondary_auth(&init) && rounds < 3 {
            rounds += 1;
            let sign_check = init.sign_check.clone().unwrap_or_default();
            let sign_key = init.sign_key.clone().unwrap_or_default();
            let (start, end) = parse_sign_check(&sign_check)?;
            let bytes = self.spool_range(start, end - start + 1).await?;
            let mut h = Sha1::new();
            h.update(&bytes);
            let sign_val = format!("{:X}", h.finalize());
            init = self
                .client
                .upload_init(
                    &self.file_name,
                    size as i64,
                    &target,
                    &fileid,
                    &preid,
                    Some(&init.pick_code),
                    Some(&sign_key),
                    Some(&sign_val),
                )
                .await?;
        }

        // 秒传命中（status==2）：零数据上传 —— 记状态，close 复核收工。
        if init.status == 2 {
            let file_id = init.file_id.clone().ok_or_else(|| {
                StorageError::Unavailable("upload/init: rapid hit without file_id".to_string())
            })?;
            self.transfer = Some(TransferState {
                fileid,
                pick_code: init.pick_code.clone(),
                bucket: String::new(),
                object: String::new(),
                callback: None,
                upload_id: None,
                done: Vec::new(),
                put_done: true,
                rapid_file_id: Some(file_id),
            });
            return Ok(());
        }
        if init.status != 1 {
            return Err(StorageError::Unavailable(format!(
                "upload/init: unexpected status {} (secondary-auth rounds used: {rounds})",
                init.status
            )));
        }
        let bucket = init.bucket.clone().ok_or_else(|| {
            StorageError::Unavailable("upload/init: status=1 without bucket".to_string())
        })?;
        let object = init.object.clone().ok_or_else(|| {
            StorageError::Unavailable("upload/init: status=1 without object".to_string())
        })?;
        let sts = self.client.get_token().await?;
        let part_size = Self::part_size(size);
        let multipart = size > part_size;
        let upload_id = if multipart {
            let http = self.client.http();
            let ctx = OssCtx {
                endpoint: sts.endpoint.clone(),
                bucket: bucket.clone(),
                object: object.clone(),
                access_key_id: sts.access_key_id.clone(),
                access_key_secret: sts.access_key_secret.clone(),
                security_token: sts.security_token.clone(),
            };
            Some(
                oss::initiate_multipart(http, &ctx)
                    .await
                    .map_err(oss_to_storage)?,
            )
        } else {
            None
        };
        self.transfer = Some(TransferState {
            fileid,
            pick_code: init.pick_code.clone(),
            bucket,
            object,
            callback: init.callback.clone(),
            upload_id,
            done: Vec::new(),
            put_done: false,
            rapid_file_id: None,
        });
        if multipart {
            self.upload_missing_parts().await
        } else {
            self.put_object_path().await
        }
    }

    /// OSS ctx（从给定 transfer 状态构造——close 的 complete 臂在
    /// `take()` 之后使用）。
    async fn oss_ctx_for(&self, t: &TransferState) -> Result<OssCtx, StorageError> {
        let sts = self.client.get_token().await?;
        Ok(OssCtx {
            endpoint: sts.endpoint.clone(),
            bucket: t.bucket.clone(),
            object: t.object.clone(),
            access_key_id: sts.access_key_id.clone(),
            access_key_secret: sts.access_key_secret.clone(),
            security_token: sts.security_token.clone(),
        })
    }

    /// OSS ctx（从当前 transfer 状态构造；STS 由调用方先行取好或此处
    /// 重取——各臂统一走这里避免漂移）。
    async fn oss_ctx(&self) -> Result<OssCtx, StorageError> {
        let sts = self.client.get_token().await?;
        let t = self.transfer.as_ref().expect("transfer state");
        Ok(OssCtx {
            endpoint: sts.endpoint.clone(),
            bucket: t.bucket.clone(),
            object: t.object.clone(),
            access_key_id: sts.access_key_id.clone(),
            access_key_secret: sts.access_key_secret.clone(),
            security_token: sts.security_token.clone(),
        })
    }

    /// 补齐缺失分片（顺序上传——115 sequential 通道要求）；每片完成即
    /// 落会话（差集续传资产），失败时也落已成功的片。
    async fn upload_missing_parts(&mut self) -> Result<(), StorageError> {
        let size = self.written;
        let part_size = Self::part_size(size);
        let total_parts = size.div_ceil(part_size) as u32;
        for n in 1..=total_parts {
            let already = self
                .transfer
                .as_ref()
                .is_some_and(|t| t.done.iter().any(|(p, _, _)| *p == n));
            if already {
                continue;
            }
            let start = (n as u64 - 1) * part_size;
            let len = part_size.min(size - start);
            let body = self.spool_range(start, len).await?;
            let ctx = self.oss_ctx().await?;
            let http = self.client.http();
            let etag = {
                let t = self.transfer.as_ref().expect("state");
                let uid = t.upload_id.clone().unwrap_or_default();
                match oss::upload_part(http, &ctx, &uid, n, body).await {
                    Ok(etag) => etag,
                    Err(e) => {
                        self.persist_session().await;
                        return Err(oss_to_storage(e));
                    }
                }
            };
            if let Some(t) = self.transfer.as_mut() {
                t.done.push((n, etag, len as i64));
                t.done.sort_by_key(|(p, _, _)| *p);
            }
            self.persist_session().await;
        }
        Ok(())
    }

    /// 单分片路径：PutObject（callback 随行）。
    async fn put_object_path(&mut self) -> Result<(), StorageError> {
        let size = self.written;
        let body = self.spool_range(0, size).await?;
        let ctx = self.oss_ctx().await?;
        let http = self.client.http();
        let callback = self.transfer.as_ref().and_then(|t| t.callback.clone());
        oss::put_object(http, &ctx, body, callback.as_ref())
            .await
            .map_err(oss_to_storage)?;
        if let Some(t) = self.transfer.as_mut() {
            t.put_done = true;
        }
        Ok(())
    }

    /// 会话落盘（已传分片 + 上传上下文）。
    async fn persist_session(&self) {
        let (Some(t), size) = (self.transfer.as_ref(), self.written) else {
            return;
        };
        let rec = UploadSession {
            pick_code: t.pick_code.clone(),
            target: format!("U_1_{}", self.parent_cid),
            bucket: t.bucket.clone(),
            object: t.object.clone(),
            parts: t.done.clone(),
            upload_id: t.upload_id.clone(),
            callback: t
                .callback
                .as_ref()
                .map(|c| (c.callback.clone(), c.callback_var.clone())),
        };
        self.sessions
            .save(self.rel_path.as_str(), size, &t.fileid, &rec)
            .await;
    }

    /// 读回 spool 的区间字节（sign_val 的区间哈希与分片切分共用）。
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

    /// 全量 SHA1 + preid（大写 hex；K69.6）。
    async fn hashes(&self) -> Result<(String, String), StorageError> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&self.spool)
            .await
            .map_err(|e| StorageError::Io(format!("spool open: {e}")))?;
        let mut full = Sha1::new();
        let mut prefix = Sha1::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut read_total: u64 = 0;
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| StorageError::Io(format!("spool read: {e}")))?;
            if n == 0 {
                break;
            }
            full.update(&buf[..n]);
            if read_total < PREID_LEN {
                let take = ((PREID_LEN - read_total) as usize).min(n);
                prefix.update(&buf[..take]);
            }
            read_total += n as u64;
        }
        Ok((
            format!("{:X}", full.finalize()),
            format!("{:X}", prefix.finalize()),
        ))
    }

    /// 分片大小：min 5MiB；超 10000 片时向上取整到 MiB（115-plus-desktop
    /// oss.rs:411-443 同规则）。
    fn part_size(size: u64) -> u64 {
        if size <= PART_MIN {
            return PART_MIN;
        }
        let by_count = size.div_ceil(PART_MAX_COUNT);
        let by_count = by_count.div_ceil(1024 * 1024) * 1024 * 1024;
        PART_MIN.max(by_count)
    }
}

impl Drop for Pan115Stager {
    fn drop(&mut self) {
        // 会话保留（resume 声明）：Drop 只清 spool；未完的 OSS 会话与
        // 已传分片记录留在 sessions（差集续传资产）。显式 abort 才清
        // 会话（abort 里做）。
        if !self.aborted {
            let _ = std::fs::remove_file(&self.spool);
        }
    }
}

#[async_trait]
impl UploadStager for Pan115Stager {
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        // u64 溢出守卫：checked_add 语义（天文数字级累积才可能触发，
        // 形态上仍拒绝而非回绕）。
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
        // 超承诺：立刻失败（baidu 同款——不再拉数据做无用工）。
        if let Some(hinted) = self.hint.size {
            if self.written > hinted {
                return Err(StorageError::Invalid);
            }
        }
        // **到齐即传**（115 的 init 会话锁定全量 block_list——到齐才
        // 起链无浪费；也让 conformance ⑦ 的差集观测点在 close 前成立）：
        // 承诺 size 到齐的那一次 write 之后立刻推传输链。
        if let Some(hinted) = self.hint.size {
            if self.written == hinted && self.transfer.is_none() {
                self.run_transfer().await?;
            }
        }
        Ok(())
    }

    /// 提交（commit-on-close 的提交点）：数据终态 → 传输链（若未起）→
    /// complete → 远端 size 复核。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        // WriteHint 契约：承诺与实际不符 → Invalid（vfs 层据此拒绝）。
        if let Some(hinted) = self.hint.size {
            if self.written != hinted {
                return Err(StorageError::Invalid);
            }
        }
        let size = self.written;
        if self.transfer.is_none() {
            // 无承诺（hint 缺省）或 write 未触发的形态：close 起链。
            self.run_transfer().await?;
        }
        let t = self.transfer.take().expect("transfer state after run");

        // ---- 秒传命中：零数据上传，直接复核收工。
        if let Some(file_id) = t.rapid_file_id.clone() {
            self.sessions
                .remove(self.rel_path.as_str(), size, &t.fileid)
                .await;
            let row = self.resolve_new_row().await?;
            let entry = self.finish_entry(&file_id, size).await?;
            return Ok(Entry {
                mtime: row.upt as f64,
                ..entry
            });
        }

        // ---- 提交点：multipart → Complete(+callback)；单分片 → 已 PUT。
        if let Some(upload_id) = t.upload_id.clone() {
            let ctx = OssCtx {
                endpoint: self.client.get_token().await?.endpoint.clone(),
                bucket: t.bucket.clone(),
                object: t.object.clone(),
                access_key_id: String::new(),
                access_key_secret: String::new(),
                security_token: String::new(),
            };
            let _ = ctx; // 真实 ctx 由 oss_ctx 统一构造（避免空 STS）
            let ctx = self.oss_ctx_for(&t).await?;
            let http = self.client.http();
            let parts: Vec<(u32, String)> =
                t.done.iter().map(|(n, e, _)| (*n, e.clone())).collect();
            oss::complete_multipart(http, &ctx, &upload_id, &parts, t.callback.as_ref())
                .await
                .map_err(oss_to_storage)?;
            self.sessions
                .remove(self.rel_path.as_str(), size, &t.fileid)
                .await;
        }
        // 单分片路径：对象已在 run_transfer 里 PUT（put_done），此处无
        // 额外动作——complete-on-close 的提交点即 close 本身。

        // ---- 复核（硬纪律 5：远端 size 与本地必须一致）+ 会话收尾。
        let row = self.resolve_new_row().await?;
        let entry = self.finish_entry(&row.fid, size).await?;
        Ok(Entry {
            mtime: row.upt as f64,
            ..entry
        })
    }

    /// 放弃暂存：清会话 + 删 spool（远端无可见垃圾——未完的 OSS
    /// multipart 不是对象）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        self.aborted = true;
        // 会话键需要 sha1——spool 尚在，可算（失败则不阻塞清理）。
        if let Ok((fileid, _)) = self.hashes().await {
            let size = self.written;
            self.sessions
                .remove(self.rel_path.as_str(), size, &fileid)
                .await;
        }
        let _ = tokio::fs::remove_file(&self.spool).await;
        Ok(())
    }
}

impl Pan115Stager {
    /// complete 后定位新文件行（父目录 list 轮询；可见性延迟容忍 5s；
    /// 行带 upt 供 Entry.mtime 填充）。
    async fn resolve_new_row(&self) -> Result<crate::api::ListRow, StorageError> {
        for attempt in 0..10u32 {
            let rows = pathcache::list_all(&self.client, &self.parent_cid).await?;
            if let Some(row) = rows.iter().find(|r| r.fname == self.file_name) {
                return Ok(row.clone());
            }
            if attempt < 9 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        Err(StorageError::Unavailable(format!(
            "upload complete: {} not visible in the target dir after 5s",
            self.file_name
        )))
    }

    /// size 复核 + Entry 产出。
    async fn finish_entry(&self, file_id: &str, expected_size: u64) -> Result<Entry, StorageError> {
        let info = self.client.get_info(file_id).await?;
        let remote_size = info.size_byte.max(0) as u64;
        if remote_size != expected_size {
            return Err(StorageError::Unavailable(format!(
                "upload size mismatch: local {expected_size} vs remote {remote_size} \
                 (the zero-byte upload bug class — refusing to report success)"
            )));
        }
        Ok(Entry {
            id: EntryId::new(
                self.volume.clone(),
                cloudkit_storage::BackendHandle::new(crate::encode_handle(
                    file_id,
                    &info.pick_code,
                    "",
                )),
            ),
            path: self.rel_path.clone(),
            kind: EntryKind::File,
            size: remote_size,
            mtime: 0.0,
        })
    }
}

/// 二次认证触发判定（两仓库形态并集：status∈{6,7,8} 或 sign_key+
/// sign_check 双出现——K69.2；115-plus-desktop 用后者、OpenList 用前者）。
fn needs_secondary_auth(init: &InitResp) -> bool {
    matches!(init.status, 6..=8) || (init.sign_key.is_some() && init.sign_check.is_some())
}

/// `"start-end"`（闭区间，含端点；K69.2）→ (start, end)。
fn parse_sign_check(s: &str) -> Result<(u64, u64), StorageError> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| StorageError::Unavailable(format!("sign_check not a range: {s:?}")))?;
    let start: u64 = a
        .trim()
        .parse()
        .map_err(|_| StorageError::Unavailable(format!("sign_check start not numeric: {s:?}")))?;
    let end: u64 = b
        .trim()
        .parse()
        .map_err(|_| StorageError::Unavailable(format!("sign_check end not numeric: {s:?}")))?;
    if end < start {
        return Err(StorageError::Unavailable(format!(
            "sign_check inverted range: {s:?}"
        )));
    }
    Ok((start, end))
}

/// OSS 错误 → StorageError（K69.8 分类：NoSuchUpload 真死、429/5xx 可
/// 重试、其余按状态码归一）。
fn oss_to_storage(e: oss::OssError) -> StorageError {
    if e.retryable() {
        return StorageError::RateLimited { retry_after: None };
    }
    if e.is_no_such_upload() {
        return StorageError::NotFound;
    }
    StorageError::Unavailable(format!("OSS {} {}: {}", e.verb, e.status, e.body_head))
}

/// 打开写暂存器（[`Pan115Driver::writer`] 调用面）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_writer(
    client: &Arc<Pan115Client>,
    volume: &cloudkit_storage::VolumeId,
    root_cid: &str,
    paths: &pathcache::PathCache,
    path: &RelPath,
    hint: &WriteHint,
    sessions: &Arc<SessionStore>,
    spool_dir: &Path,
) -> Result<Box<dyn UploadStager>, StorageError> {
    let parent = path.parent().unwrap_or_else(RelPath::root);
    let file_name = path
        .components()
        .last()
        .ok_or(StorageError::Invalid)?
        .to_string();
    // 父目录必须存在或可隐式创建（trait：写入路径的缺失父目录由驱动
    // 隐式创建）——此处复用 mkdir 语义的探路：解析 + 逐级 create。
    let parent_cid = ensure_parents(client, root_cid, paths, &parent, &file_name).await?;
    // 目标是目录 → Invalid（写入方不会这样调用，防御契约）。
    if let Some(row) = paths.get_child(&parent_cid, &file_name).await {
        if row.fc == "0" {
            return Err(StorageError::Invalid);
        }
    }
    let spool = spool_dir.join(format!("pan115-upload-{}.part", unique_tag()));
    tokio::fs::File::create(&spool)
        .await
        .map_err(|e| StorageError::Io(format!("spool create: {e}")))?;
    Ok(Box::new(Pan115Stager::new(
        client.clone(),
        volume.clone(),
        parent_cid,
        file_name,
        path.clone(),
        spool,
        hint.clone(),
        sessions.clone(),
    )))
}

/// 解析父目录并逐级隐式创建（trait 契约：写入路径的缺失父目录由驱动
/// 创建）；返回父 cid。
async fn ensure_parents(
    client: &Arc<Pan115Client>,
    root_cid: &str,
    paths: &pathcache::PathCache,
    parent: &RelPath,
    _file_name: &str,
) -> Result<String, StorageError> {
    let comps: Vec<String> = parent.components().map(str::to_string).collect();
    let mut cid = root_cid.to_string();
    for comp in comps {
        let existing = match paths.get_child(&cid, &comp).await {
            Some(row) => Some(row),
            None => {
                let rows = pathcache::list_all(client, &cid).await?;
                paths.put_dir(&cid, &rows).await;
                paths.get_child(&cid, &comp).await
            }
        };
        match existing {
            Some(row) if row.fc == "0" => cid = row.fid,
            Some(_) => return Err(StorageError::NotFound), // 父链上有文件
            None => {
                let fid = client.mkdir(&cid, &comp).await?;
                paths.invalidate(&cid).await;
                cid = fid;
            }
        }
    }
    Ok(cid)
}

/// spool 文件名唯一标签（进程内计数 + 时间；不引 uuid 依赖）。
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

/// spool 目录解析（`Pan115Params::sessions_dir` 缺省 = 系统临时目录；
/// 生产装配给卷家目录，K21 锚形态）。
pub(crate) fn default_spool_dir(params: &crate::Pan115Params) -> PathBuf {
    match &params.sessions_dir {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir(),
    }
}

/// 驱动侧写入口（[`Pan115Driver::writer`] 的实现体——lib.rs 保持薄）。
pub(crate) async fn writer(
    driver: &Pan115Driver,
    path: &RelPath,
    hint: &WriteHint,
) -> Result<Box<dyn UploadStager>, StorageError> {
    if path.is_root() {
        return Err(StorageError::Invalid);
    }
    let spool_dir = default_spool_dir(driver.params_ref());
    open_writer(
        driver.client_arc(),
        driver.volume_ref(),
        driver.root_cid(),
        driver.paths(),
        path,
        hint,
        driver.sessions_arc(),
        &spool_dir,
    )
    .await
}
