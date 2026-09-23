//! 三步曲上传（precreate → superfile2 → create）+ K7 会话表（Batch B2）。
//!
//! wire 形态黄金参照：spike `examples/baidu_spike/src/api.rs:187-370` +
//! PCFS `drivers/baidu/api.go:440-600`（分歧以 spike 实抓为准）。
//!
//! ## 真网 31363 实证与「到齐即传」策略（2026-09-08 B2 返工裁决）
//!
//! 干净探针（MSYS_NO_PATHCONV + netdisk UA）实证百度**会话模型**：
//! precreate 一次性锁定 `(path, size, block_list)`；create 的 block_list
//! 必须**原样重申** precreate 锁定的声明——不一致 → **errno=31363**。
//! superfile2 则接受任意 partseq（含未声明分片）。由此「write 首块即传
//! （precreate 部分声明）」不可行：部分声明会话在 create 必然 31363。
//! spike 未踩此坑因其全量算 md5 后才 precreate。
//!
//! 裁决落点（stager 流式策略）：
//!
//! - **write「到齐即传」**：数据未到齐（无 hint 或累计字节 < hint.size）
//!   时 write **不做任何网络动作**（本地 staging + 满块 md5 增量计算）；
//!   到齐（hint.size 已知且累计 == size）时 write 返回前完成：precreate
//!   （**全量** block_list，rtype=3）→ 串行 superfile2 传全部满块 → 位图
//!   落盘；
//! - **尾块归 close**：不满 4MiB 的最后一片在 close 时传（到齐时若无
//!   尾块则 close 无补传）；
//! - **create 重申会话列表**：close 的 create 用 precreate 时锁定的
//!   block_list（存于 stager，装备时装载），**不是重算**——31363 免疫；
//! - **无 hint.size**：全缓冲至 close（close 时 size=实际字节，precreate
//!   全量）——真实调用面（WebDAV PUT / CLI copy）均带 Content-Length。
//!
//! ## 0 字节 wire 真形（2026-09-23 主会话活 token 真网探针，Phase 8-B）
//!
//! size=0 的会话形态：precreate 声明 `block_list=[EMPTY_MD5]`（空串
//! MD5；**空数组 → errno=2** 真网实测）→ 零 superfile2（无分片可传，
//! 服务端视声明块在位）→ create 原样重申同形。落点：[`finalize_tail`]
//! 空缓冲分支（stager 流式路径，两处定型调用点覆盖 hinted/无承诺与
//! 「打开即 close」）+ [`upload_whole_file`] 空数据构造分支（transport
//! 整文件路径）+ 上传循环的 EMPTY_MD5 免传跳过。加密空件不经此路
//! （容器 44/50B 非零载荷，正常三步曲）。
//!
//! ## 串行上传取舍（write 同步落定的代价）
//!
//! **到齐的 write() 返回时，全部满块必须已上传完成**（superfile2
//! error_code=0）且位图已落盘——到齐后 drop，已传满块确定可复用、
//! 重建 driver 差集恰补缺失（`tests/upload_resume.rs` 钉死此确定性）。
//! 因此 **stager 内满块上传是串行的**（每满块在 write 内 await 完成），
//! 不并 4 worker：PCFS api.go:440-479 的 4 并发形态是「整文件已知」
//! 路径的吞吐优化，与流式 stager 的确定性契约冲突。「4 并发」语义的
//! 落点：B3b transport_face 的整文件路径（后续批次）。
//!
//! ## 会话生命周期（K7）
//!
//! - 会话定位 = (path, size)，内容一致性由恢复时 block_md5 逐片校验
//!   （防同 path+size 不同内容误复用）；落盘 `<sessions_dir>/
//!   baidu_state/sessions/<hash>.json`（hash = 定位键摘要），**随分片
//!   完成即刻原子落盘**（tmp + rename）；
//! - 会话 block_md5 恒为**全量列表**（到齐/close 才 precreate——31363
//!   裁决后不存在部分声明形态）；
//! - 恢复三路：查会话表 → 命中则**探活**（重发一个缺失分片，
//!   error_code≠0 = 死）→ 活则差集补传 / 死则新 precreate 整体重传；
//!   未命中 → 正常 precreate。**重 precreate 不是恢复手段**（spike
//!   §3.2：同参重发返回新 uploadid + 全量列表）；
//! - **abort/drop 不清会话表**（服务端分片 + 本地位图是可复用资产）；
//!   仅 create 成功或 block_md5 校验不符时作废；
//! - `sessions_dir = None` 时纯内存会话表——单进程内（同 driver）的
//!   drop → 再 writer 恢复仍成立。
//!
//! ## 探活期间的 drain 冻结
//!
//! 恢复会话探活未验证前（`probe_pending`）**不 drain 已处理块**：探活
//! 撞死需要全量重传（含此前按位图跳过的块），块数据必须还在缓冲内。
//! 探活成功（或新 precreate 会话）后恢复正常增量 drain。由此保证：
//! 探活失败必发生在 drained=0 时刻，全量重传数据完备。恢复会话满块
//! 全在位图时（无缺失满块），探活由 close 的尾块上传承担——数据因
//! 冻结仍在缓冲，撞死后 close 内重装备全量重传仍数据完备。

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use md5::{Digest, Md5};

use cloudkit_storage::{
    BackendHandle, Entry, EntryId, EntryKind, RelPath, StorageError, UploadStager, VolumeId,
    WriteHint,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::api;
use crate::client::BaiduClient;
use crate::driver::HandleCache;

/// 分片尺寸（4MiB；spike §2/§6——上/下载统一有界边界）。
pub(crate) const CHUNK: usize = 4 * 1024 * 1024;

/// 0 字节块的 MD5 = 空串 MD5，服务端拒绝空数组 block_list（2026-09-23
/// 真网 errno=2 实测）。0 字节上传的 precreate/create 必须声明
/// `block_list=[EMPTY_MD5]`（真网三步探针钉死：precreate errno=0 +
/// return_type=1 → 零 superfile2 → create 原样重申 → 落盘可见）；
/// 服务端视该块免传（无分片可传），驱动侧同步跳过 superfile2。
pub const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";

/// transport 面整文件上传的并发 worker 数（B3b；PCFS api.go:440-479 的
/// 4 并发吞吐形态——stager 流式路径不并发的取舍见模块文档「串行上传
/// 取舍」节，两条路径各守其契约）。
pub(crate) const UPLOAD_WORKERS: usize = 4;

/// 会话目录相对形态（K7 契约：`<sessions_dir>/baidu_state/sessions/`）。
const SESSIONS_REL: [&str; 2] = ["baidu_state", "sessions"];

/// K7 会话记录（落盘 JSON 形态；含 path/size/block_md5/uploadid/完成位图）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    /// 后端绝对路径。
    pub(crate) path: String,
    /// 会话认定的最终字节数（precreate size 参数）。
    pub(crate) size: u64,
    /// precreate 时锁定的**全量**分片 md5 列表（到齐/close 才 precreate
    /// ——31363 裁决后不存在部分声明形态；create 必须原样重申此列表）。
    pub(crate) block_md5: Vec<String>,
    pub(crate) uploadid: String,
    /// 已上传完成（superfile2 error_code=0）的分片索引集。
    pub(crate) done: Vec<u64>,
}

/// 会话表（内存 + 可选磁盘双层；driver 与 stager 经 Arc 共享）。
///
/// 定位键 = `"{path}|{size}"`（内存 map 键与磁盘文件名摘要的输入）；
/// block_md5 不入定位键——precreate 时刻的分片列表可能只是部分（后续
/// 数据未到），无法构成恢复查找键；内容一致性由恢复时的逐片 md5
/// 校验保证（见 [`BaiduStager`] 文档「恢复三路」）。
#[derive(Clone, Default)]
pub(crate) struct SessionStore {
    map: Arc<Mutex<HashMap<String, SessionRecord>>>,
    dir: Option<PathBuf>,
}

impl SessionStore {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        SessionStore {
            map: Arc::new(Mutex::new(HashMap::new())),
            dir,
        }
    }

    fn key(path: &str, size: u64) -> String {
        format!("{path}|{size}")
    }

    /// 定位键 → 落盘文件路径（`<dir>/baidu_state/sessions/<md5hex>.json`）。
    fn file_path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|root| {
            root.join(SESSIONS_REL[0])
                .join(SESSIONS_REL[1])
                .join(format!("{}.json", md5_hex(key.as_bytes())))
        })
    }

    /// 取会话：内存优先，miss 且有磁盘层则读文件回填（跨进程恢复腿）。
    pub(crate) async fn load(&self, path: &str, size: u64) -> Option<SessionRecord> {
        let key = Self::key(path, size);
        if let Some(rec) = self.map.lock().unwrap().get(&key) {
            return Some(rec.clone());
        }
        let file = self.file_path(&key)?;
        let raw = tokio::fs::read_to_string(&file).await.ok()?;
        let rec: SessionRecord = serde_json::from_str(&raw).ok()?;
        self.map.lock().unwrap().insert(key, rec.clone());
        Some(rec)
    }

    /// 写会话：内存 + 磁盘（原子：tmp + rename；`baidu_state/sessions/`
    /// 目录随需创建）。
    pub(crate) async fn put(&self, rec: SessionRecord) {
        let key = Self::key(&rec.path, rec.size);
        self.map.lock().unwrap().insert(key.clone(), rec.clone());
        if let Some(file) = self.file_path(&key) {
            if let Some(parent) = file.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let body = serde_json::to_string(&rec).unwrap_or_default();
            // 原子写：同目录 tmp + rename（Windows rename 覆盖已存在目标）。
            let tmp = file.with_extension(format!(
                "json.tmp-{}-{}",
                std::process::id(),
                TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ));
            if tokio::fs::write(&tmp, body).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &file).await;
            }
        }
    }

    /// 作废会话（create 成功 / 探活判死 / md5 校验不符）：内存 + 磁盘。
    pub(crate) async fn remove(&self, path: &str, size: u64) {
        let key = Self::key(path, size);
        self.map.lock().unwrap().remove(&key);
        if let Some(file) = self.file_path(&key) {
            let _ = tokio::fs::remove_file(&file).await;
        }
    }
}

/// tmp 文件名去重计数器（多 stager 同 key 并发写的碰撞窗口隔离）。
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// MD5 hex（分片 md5：4MiB 边界对齐内容逐片计算，非全文件 MD5）。
pub(crate) fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 三步曲 stager（B2；「到齐即传」——2026-09-08 真网 31363 返工裁决）。
///
/// ## 状态机（write → close / abort / drop）
///
/// - `write`：数据入缓冲 + 满块 md5 增量计算（纯本地）；**数据到齐**
///   （hint.size 已知且累计字节 == size）时在返回前完成：装备会话
///   （恢复三路或新 precreate 全量 block_list，见下）→ 缺失满块逐块
///   superfile2（串行，见模块文档取舍）→ 位图落盘 → drain。**未到齐
///   零网络动作**（31363：precreate 需一次性锁定全量 block_list）；
/// - `close`：size 承诺校验（不符 → `Invalid`，WriteHint 契约）→ 兜底
///   装备（无 hint 全量定型 / 空文件 / 防御路径）→ 查缺补传含尾块
///   （恢复会话探活未验证场景首个上传兼探活，撞死 → 重装备全量重传）
///   → `create`（rtype=3，**原样重申会话锁定的 block_list**）→ Entry
///   → 会话作废；
/// - `abort` / Drop：弃内存态，**会话表保留**（K7 资产；位图已随分片
///   完成即刻落盘，Drop 无需善后动作）；
/// - 秒传腿（precreate return_type=2）：零 superfile2/create，Entry 由
///   响应 fs_id 直接收尾（官方语义存在但 spike §4 实证此 appkey 桶不
///   触发——路径必须有、不依赖）。
///
/// ## 恢复三路（装备时）
///
/// 1. [`SessionStore::load`] 命中 → 逐片校验会话 block_md5 与新内容已算
///    md5（前缀不一致 → 作废重建）→ 装备旧 uploadid + 位图，**首个
///    缺失分片上传兼探活**（Err = 会话死 → 作废 → 新 precreate + 全量
///    重传）；
/// 2. 未命中 → 隐式建父目录 + `precreate`（rtype=3，全量 block_list）
///    → return_type=2 走秒传收尾，否则装备新 uploadid（位图空）；
/// 3. 之后所有块按位图差集上传（in-done 块零流量复用）。
pub(crate) struct BaiduStager {
    client: Arc<BaiduClient>,
    volume: VolumeId,
    /// 规范化卷根（abs 拼接用——隐式父目录逐级换算）。
    root: String,
    /// 目标卷内路径（close 后 Entry.path）。
    rel: RelPath,
    /// 目标后端绝对路径。
    abs: String,
    hinted_size: Option<u64>,
    sessions: SessionStore,
    /// 未 drain 数据（满块 + 尾块；探活未验证前满块保留其中）。
    buffer: Vec<u8>,
    /// 累计写入字节数。
    written: u64,
    /// 已 drain 的满块数（全局分片索引 = 仍在缓冲内首块索引）。
    drained: u64,
    /// 已算出 md5 的分片（按全局索引序；含定型尾块）。
    block_md5: Vec<String>,
    /// 尾块 md5 是否已计入（数据终态时定型——到齐或无承诺 close）。
    tail_included: bool,
    /// 活跃会话（precreate/恢复装备后）；None = 尚未装备。
    uploadid: Option<String>,
    /// precreate 会话锁定的 block_list（装备时装载：新 precreate = 本次
    /// 全量列表 / 恢复记录 = 记录锁定列表）。close 的 create **原样重申
    /// 此列表**——真网 31363 实证：create 与 precreate 声明不一致被拒，
    /// 故 create 不用重算的 [`Self::block_md5`] 而用会话锁定值。
    session_blocks: Vec<String>,
    /// 已上传完成分片索引集（恢复时装载旧位图）。
    done: BTreeSet<u64>,
    /// 恢复会话的探活待验标记（首个缺失块上传兼探活，成功即验证活；
    /// 期间 drain 冻结，见模块文档）。
    probe_pending: bool,
    /// 秒传命中（precreate return_type=2 的 fs_id；close 直接收尾）。
    rapid_fs_id: Option<i64>,
    /// fs_id → 条目句柄缓存（entry_for 的解析层资产填充点；真网 31300
    /// 裁决——与 driver 共享经 Arc）。
    handles: Arc<HandleCache>,
}

/// writer 打开（driver.rs 委派）：目标已存在目录 → `Invalid`（trait
/// 契约）；会话装备延迟到首次网络动作（恢复校验需要内容 md5，打开时
/// 未知）。
///
/// 已存在目录预检走**父目录 list + path 匹配**（真网 31300 实证驱动，
/// 2026-09-08 第四轮返工——meta&path 停用）；卷根本身不可为 writer 目标
///（恒为已存在目录——父目录 list 对根形态不可用（"/" 的父是自身），
/// 先行短路保形态）。
// 句柄缓存入参后达 8 参数——驱动内部委派函数（唯一调用点 driver.rs
// writer），参数组包结构体反而遮蔽与 driver 字段的一一对应关系，allow。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_writer(
    client: &Arc<BaiduClient>,
    volume: &VolumeId,
    root: &str,
    abs: &str,
    rel: &RelPath,
    hint: &WriteHint,
    sessions: &SessionStore,
    handles: &Arc<HandleCache>,
) -> Result<Box<dyn UploadStager>, StorageError> {
    if rel.is_root() {
        return Err(StorageError::Invalid); // 卷根恒为已存在目录
    }
    // 父目录不存在（list -9）→ 预检放行（ensure_parents 随后逐级创建；
    // 与原 meta 预检的 NotFound 容忍语义一致）。
    if let Ok(entries) = api::list(client, &api::parent_abs(abs)).await {
        handles.put_batch(&entries); // 预检流量顺带填充句柄缓存
        if let Some(hit) = entries.iter().find(|e| e.path == abs) {
            if hit.isdir != 0 {
                return Err(StorageError::Invalid); // 目标路径是已存在目录
            }
        }
    }
    Ok(Box::new(BaiduStager {
        client: client.clone(),
        volume: volume.clone(),
        root: root.to_string(),
        rel: rel.clone(),
        abs: abs.to_string(),
        hinted_size: hint.size,
        sessions: sessions.clone(),
        buffer: Vec::new(),
        written: 0,
        drained: 0,
        block_md5: Vec::new(),
        tail_included: false,
        uploadid: None,
        session_blocks: Vec::new(),
        done: BTreeSet::new(),
        probe_pending: false,
        rapid_fs_id: None,
        handles: handles.clone(),
    }))
}

impl BaiduStager {
    /// 承诺的最终字节数（流式路径的会话 size 参数）。
    fn final_size(&self) -> u64 {
        self.hinted_size.unwrap_or(self.written)
    }

    /// 超量防御：written 超过承诺后不再做任何网络动作（close 终态
    /// `Invalid`；不再推进分片状态，避免索引错位）。
    fn overpromised(&self) -> bool {
        self.hinted_size.is_some_and(|h| self.written > h)
    }

    /// 会话装备（恢复三路或新 precreate；幂等——已装备/秒传直接返回）。
    /// 装备即装载 `session_blocks`（create 重申锚点，31363 免疫）。
    async fn ensure_session(&mut self) -> Result<(), StorageError> {
        if self.uploadid.is_some() || self.rapid_fs_id.is_some() {
            return Ok(());
        }
        let size = self.final_size();
        // 路一：会话表命中 → block_md5 逐片前缀校验 → 装备旧会话。
        if let Some(rec) = self.sessions.load(&self.abs, size).await {
            let consistent = self
                .block_md5
                .iter()
                .zip(rec.block_md5.iter())
                .all(|(new, old)| new == old);
            if consistent {
                self.uploadid = Some(rec.uploadid.clone());
                self.session_blocks = rec.block_md5.clone();
                self.done = rec.done.iter().copied().collect();
                self.probe_pending = true; // 首个缺失块上传兼探活
                return Ok(());
            }
            // block_md5 不符（同 path+size 不同内容）→ 作废重建。
            self.sessions.remove(&self.abs, size).await;
        }
        // 路二/三：新 precreate（隐式建父目录——trait 契约；目录腿
        // isdir=1 与文件腿在 wire 上分流，不干扰三步曲断言）。
        self.ensure_parents().await?;
        let outcome = api::precreate(&self.client, &self.abs, size, &self.block_md5).await?;
        if outcome.return_type == 2 {
            // 秒传腿：云端已有同内容对象，零 superfile2/create 直接收尾。
            self.rapid_fs_id = Some(outcome.fs_id);
            return Ok(());
        }
        self.uploadid = Some(outcome.uploadid);
        self.session_blocks = self.block_md5.clone();
        self.done = BTreeSet::new();
        Ok(())
    }

    /// 隐式建父目录：薄壳委派共享函数 [`ensure_parents`]（B3b 提取——
    /// transport 面整文件路径共用同一预检协议，不重复）。
    async fn ensure_parents(&self) -> Result<(), StorageError> {
        ensure_parents(&self.client, &self.root, &self.rel, &self.handles).await
    }

    /// 上传单个分片（带探活/重建编排）。
    ///
    /// - 恢复会话的首个上传兼**探活**：Err（error_code≠0 或网络失败均按
    ///   死判定，保守）→ 作废旧会话（`uploadid` 置 None）→ 由调用方
    ///   重装备 + 全量重传；
    /// - 已验证活的会话（或新 precreate 会话）上传错误直接上抛。
    async fn upload_block(&mut self, idx: u64, data: Bytes) -> Result<(), StorageError> {
        let uploadid = self.uploadid.clone().expect("upload_block 前必已装备会话");
        let res = self
            .client
            .superfile2(&self.client.pcs_base, &self.abs, &uploadid, idx, data)
            .await;
        match res {
            Ok(()) => {
                self.probe_pending = false; // 探活通过：会话已验证活
                self.done.insert(idx);
                self.persist_session().await;
                Ok(())
            }
            Err(e) if self.probe_pending => {
                // 探活撞死：作废（此刻 drained 必为 0，全量重传数据完备）。
                self.sessions.remove(&self.abs, self.final_size()).await;
                self.probe_pending = false;
                self.uploadid = None;
                self.done.clear();
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// 会话即刻落盘（随分片完成；K7 契约）。秒传/未装备无会话资产。
    async fn persist_session(&self) {
        let Some(uploadid) = &self.uploadid else {
            return;
        };
        self.sessions
            .put(SessionRecord {
                path: self.abs.clone(),
                size: self.final_size(),
                block_md5: self.block_md5.clone(),
                uploadid: uploadid.clone(),
                done: self.done.iter().copied().collect(),
            })
            .await;
    }

    /// 满块 md5 增量补算（每次 write 后推进；纯本地计算，无网络动作）。
    fn compute_full_block_md5s(&mut self) {
        let avail = self.buffer.len() / CHUNK;
        for i in 0..avail {
            let idx = self.drained as usize + i;
            if idx >= self.block_md5.len() {
                let md5 = md5_hex(&self.buffer[i * CHUNK..(i + 1) * CHUNK]);
                self.block_md5.push(md5);
            }
        }
    }

    /// 尾块定型（数据终态时：hinted 到齐或无承诺 close）。定型后
    /// `block_md5` 即最终**全量**形态——precreate 一次性锁定（31363：
    /// 部分声明会话在 create 必然被拒，故定型先于装备）。
    ///
    /// 0 字节特形（[`EMPTY_MD5`] 文档）：空载荷新增空串 MD5 声明块——
    /// 服务端拒空数组 block_list（errno=2）且视该块免传。定型只在数据
    /// 终态发生且此刻 `drained` 恒为 0（到齐路径先定型后 drain），缓冲
    /// 为空即全文件为空。
    fn finalize_tail(&mut self) {
        if self.tail_included || self.rapid_fs_id.is_some() {
            return;
        }
        if self.buffer.is_empty() {
            self.block_md5.push(EMPTY_MD5.to_string());
            self.tail_included = true;
            return;
        }
        let avail = self.buffer.len() / CHUNK;
        if !self.buffer.len().is_multiple_of(CHUNK) {
            let tail = &self.buffer[avail * CHUNK..];
            self.block_md5.push(md5_hex(tail));
            self.tail_included = true;
        }
    }

    /// write 的流式处理（仅 size 承诺路径；无承诺全缓冲至 close）。
    ///
    /// **到齐即传**（31363 裁决）：未到齐（累计字节 < hint.size）直接
    /// 返回——零网络动作；到齐时定型全量 block_md5 → 会话装备（首次，
    /// 含恢复三路）→ 差集上传循环（探活撞死则重装备全量重传）→ drain
    /// （探活冻结期除外，尾块始终保留归 close）。
    async fn flush_streaming(&mut self) -> Result<(), StorageError> {
        self.compute_full_block_md5s();
        let complete = self.hinted_size.is_some_and(|h| self.written == h);
        if !complete {
            return Ok(()); // 未到齐：不装备、不传（31363：不可部分声明）
        }
        self.finalize_tail();
        let avail = (self.buffer.len() / CHUNK) as u64;
        loop {
            self.ensure_session().await?;
            if self.rapid_fs_id.is_some() {
                // 秒传命中：分片零上传（服务端已有同内容对象），数据可弃。
                self.buffer.clear();
                self.drained += avail;
                return Ok(());
            }
            // 差集上传循环（满块；恢复会话首个缺失块兼探活）。
            let mut i: usize = 0;
            while i < avail as usize {
                let seq = self.drained + i as u64;
                if self.done.contains(&seq) {
                    i += 1;
                    continue; // 恢复位图命中：零流量复用
                }
                let data = Bytes::copy_from_slice(&self.buffer[i * CHUNK..(i + 1) * CHUNK]);
                match self.upload_block(seq, data).await {
                    Ok(()) => {
                        i += 1;
                    }
                    Err(e) => {
                        if self.uploadid.is_none() {
                            // 探活撞死已作废：重装备（新 precreate）后
                            // 整轮重来（done 已清空，从首块全量重传）。
                            break;
                        }
                        return Err(e);
                    }
                }
            }
            if self.uploadid.is_none() {
                continue; // 撞死重装备路径：loop 头重走
            }
            // drain（探活冻结期不 drain——probe_pending 仍真时数据保留，
            // 供潜在全量重传；尾块始终保留归 close）。
            if !self.probe_pending {
                self.buffer.drain(..avail as usize * CHUNK);
                self.drained += avail;
            }
            return Ok(());
        }
    }

    /// Entry 构造：薄壳委派共享函数 [`lookup_entry`]（B3b 提取——
    /// transport 面整文件路径共用同一解析，不重复）。
    async fn entry_for(&self, fs_id: i64) -> Result<Entry, StorageError> {
        lookup_entry(
            &self.client,
            &self.volume,
            &self.rel,
            &self.abs,
            &self.handles,
            fs_id,
        )
        .await
    }

    /// close 的统一收尾循环：兜底装备（空文件/无承诺/防御路径——到齐
    /// write 已装备则幂等复用会话）→ 查缺补传（含尾块；恢复会话探活
    /// 未验证场景首个上传兼探活，撞死 → 重装备整轮重来——探活冻结
    /// 保证此刻数据仍在缓冲）。
    async fn finalize_and_upload_remaining(&mut self) -> Result<(), StorageError> {
        loop {
            self.ensure_session().await?;
            if self.rapid_fs_id.is_some() {
                return Ok(()); // 秒传腿：零分片零 create
            }
            // 查缺补传：从 drained 起到全量末（含尾块；span 相对缓冲，
            // 未 drain 的满块与尾块数据均在缓冲内）。
            let mut seq = self.drained;
            while seq < self.block_md5.len() as u64 {
                if self.done.contains(&seq) {
                    seq += 1;
                    continue; // 位图命中（write 已传满块）：零流量复用
                }
                if self.block_md5[seq as usize] == EMPTY_MD5 {
                    // 0 字节声明块：服务端免传（precreate 声明即在位，
                    // 2026-09-23 真网真形）——零 superfile2 直接收尾。
                    seq += 1;
                    continue;
                }
                let start = (seq - self.drained) as usize * CHUNK;
                let end = (start + CHUNK).min(self.buffer.len());
                let data = Bytes::copy_from_slice(&self.buffer[start..end]);
                match self.upload_block(seq, data).await {
                    Ok(()) => {
                        seq += 1;
                    }
                    Err(e) => {
                        if self.uploadid.is_none() {
                            // 探活撞死已作废：重装备（新 precreate）整轮
                            // 重来（done 已清空，drained 因探活冻结必为 0，
                            // 全量重传数据完备）。
                            break;
                        }
                        return Err(e);
                    }
                }
            }
            if self.uploadid.is_some() || self.rapid_fs_id.is_some() {
                return Ok(());
            }
            // 撞死重装备路径：loop 头重走。
        }
    }
}

#[async_trait]
impl UploadStager for BaiduStager {
    /// 追加数据：本地 staging + 满块 md5 增量计算；**到齐即传**（有 size
    /// 承诺且累计到量时，write 返回前完成全量 precreate + 满块串行上传
    /// + 位图落盘——同步落定契约见模块文档；未到齐零网络动作）。
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.written += data.len() as u64;
        self.buffer.extend_from_slice(data);
        if self.overpromised() {
            return Ok(()); // 超承诺：不再动作，close 终态 Invalid
        }
        if self.hinted_size.is_some() {
            self.flush_streaming().await?;
        }
        // 无承诺：全缓冲至 close（数据终态时全量成型，见 close）。
        Ok(())
    }

    /// 提交：承诺校验 → 无承诺路径全量定型 → 统一收尾（兜底装备 + 尾块/
    /// 查缺补传，探活撞死重装备全量重传）→ create（**原样重申会话锁定
    /// 的 block_list**，31363）→ Entry → 会话作废。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        if let Some(hinted) = self.hinted_size {
            if self.written != hinted {
                // WriteHint 契约：承诺与实际不符（会话资产保留——下次同
                // 承诺量 writer 仍可差集续传已传分片）。
                return Err(StorageError::Invalid);
            }
        }
        let size = self.final_size();
        // 无承诺路径：数据此刻终态（size/分片全可定）——定型全量
        // block_md5，装备与上传由统一收尾循环承担。
        if self.hinted_size.is_none() {
            self.compute_full_block_md5s();
        }
        // 0 字节兜底（wire 真形）：write 从未触发到齐路径的 hinted 空件
        // 腿（writer 打开即 close）也要带 `[EMPTY_MD5]` 声明；其余形态
        // finalize_tail 幂等早退（tail_included / 秒传腿）。
        self.finalize_tail();
        // 统一收尾：兜底装备（空文件 write([]) 从未触发到齐路径/防御）→
        // 查缺补传含尾块 →（秒传腿短路）。
        self.finalize_and_upload_remaining().await?;
        // 秒传腿：precreate return_type=2 已带 fs_id，零收尾调用。
        if let Some(fs_id) = self.rapid_fs_id {
            let entry = self.entry_for(fs_id).await?;
            self.sessions.remove(&self.abs, size).await;
            return Ok(Entry {
                size: self.written,
                ..entry
            });
        }
        self.buffer.clear();
        // create 收尾（rtype=3 覆盖语义）——block_list 用 precreate 会话
        // 锁定的 [`Self::session_blocks`]（原样重申，非重算：31363）。
        let uploadid = self.uploadid.clone().expect("close 前必已装备会话");
        let fs_id = api::create_file(
            &self.client,
            &self.abs,
            &uploadid,
            size,
            &self.session_blocks,
        )
        .await?;
        let entry = self.entry_for(fs_id).await?;
        self.sessions.remove(&self.abs, size).await;
        Ok(entry)
    }

    /// 放弃暂存：弃内存态、**保留会话表**（K7 可复用资产；与 close 成功
    /// 的作废语义相对——`tests/upload_resume.rs` abort 用例钉死）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        self.buffer = Vec::new();
        self.uploadid = None;
        self.done.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 两面共享的自由函数（B3b 提取：stager 流式路径与 transport 整文件路径
// 共用同一协议逻辑，不重复）
// ---------------------------------------------------------------------------

/// 卷内路径 → 后端绝对路径（root 前缀拼接；driver.rs `abs_path` 同源
/// ——stager/transport 面/driver 三处共用本函数，K6 前缀不泄漏 R1）。
pub(crate) fn abs_of(root: &str, rel: &RelPath) -> String {
    if rel.is_root() {
        return root.to_string();
    }
    if root == "/" {
        format!("/{}", rel.as_str())
    } else {
        format!("{root}/{}", rel.as_str())
    }
}

/// 隐式建父目录（**卷根下**逐级，不从后端绝对路径首段建起——/apps 等
/// 前缀非本卷资产）。
///
/// **list 预检**（真网实证 2026-09-08 第五轮）：目录 create 撞已存在
/// ≠ -8——errno=0 成功假象 + `<名>_<时间戳>` 空副本重命名（每次上传
/// 到已有目录路径都产空目录垃圾）。每层先 list 父目录判断该层是否已
/// 存在（是 → 跳过 create；否 → create）；预检 list 结果顺带批量喂
/// 句柄缓存。-8 容错保留为防御语义（真网未观察到）。
pub(crate) async fn ensure_parents(
    client: &Arc<BaiduClient>,
    root: &str,
    rel: &RelPath,
    handles: &Arc<HandleCache>,
) -> Result<(), StorageError> {
    let Some(parent) = rel.parent() else {
        return Ok(()); // 根下文件（rel 无父）：无中间层可建
    };
    if parent.is_root() {
        return Ok(()); // 直接位于卷根：卷根已存在
    }
    let mut prefix = RelPath::root();
    for comp in parent.components() {
        prefix = prefix.join(comp)?;
        let abs = abs_of(root, &prefix);
        let siblings = api::list(client, &api::parent_abs(&abs)).await?;
        handles.put_batch(&siblings); // 预检流量顺带喂句柄缓存
        if siblings.iter().any(|e| e.path == abs) {
            continue; // 该层已存在：零 create 下沉（ghost 免疫）
        }
        match api::create_dir(client, &abs).await {
            Ok(()) => {}
            Err(StorageError::Exists) => {} // 防御（真网实证 errno=0 形态，-8 不触发）
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// fs_id → Entry（父目录 list + fs_id 匹配；真网 31300/31023 实证驱动，
/// 2026-09-08 第四轮返工——meta 端点在此 appkey 下全废，list 即时可见是
/// 主路径；断言① mtime>0 的来源即后端入树时间戳）。
///
/// 顺带批量填充句柄缓存——close/整文件上传产出的新 fs_id 是缓存的
/// 第一批资产来源（transport 面后续 open/delete 的 warm 命中面）。
pub(crate) async fn lookup_entry(
    client: &Arc<BaiduClient>,
    volume: &VolumeId,
    rel: &RelPath,
    abs: &str,
    handles: &Arc<HandleCache>,
    fs_id: i64,
) -> Result<Entry, StorageError> {
    let entries = api::list(client, &api::parent_abs(abs)).await?;
    handles.put_batch(&entries);
    let remote = entries
        .into_iter()
        .find(|e| e.fs_id == fs_id)
        .ok_or(StorageError::NotFound)?;
    Ok(Entry {
        id: EntryId::new(volume.clone(), BackendHandle::new(remote.fs_id.to_string())),
        path: rel.clone(),
        kind: if remote.isdir != 0 {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: remote.size.max(0) as u64,
        mtime: remote.server_mtime as f64,
    })
}

/// transport 面整文件上传（B3b 段一；[`UPLOAD_WORKERS`] 并发 superfile2）。
///
/// 与 stager 流式路径的分工（模块文档「串行上传取舍」的 B3b 兑现）：本
/// 函数是 **close-once 整文件路径**——数据全量在手，吞吐优先；stager
/// 的「到齐 write 同步落定」串行契约（conformance ⑦）不变。会话/位图
/// 沿用 K7 会话表（与 stager 共用同一 [`SessionStore`]——同一
/// `(path, size)` 会话可跨两路径差集续传），**位图随分片完成即刻落盘**。
///
/// 流程（恢复三路的整文件形态）：
///
/// 1. 会话表命中且全量 `block_md5` 一致 → 复用 uploadid + 位图（探活见
///    下）；不一致（同 path+size 不同内容）→ 作废重建；
/// 2. 恢复会话**探活**：首个缺失分片串行上传兼探活，撞死 → 作废 → 新
///    precreate 全量重传（探活错误不外泄——会话死是预期恢复路径，stager
///    同款语义）；满块全在位图 → 无探活，create 即收尾；
/// 3. 新 precreate（隐式建父 + rtype=3 全量声明——31363 裁决）；秒传腿
///    （return_type=2）零分片零 create；
/// 4. 缺失分片 [`UPLOAD_WORKERS`] 并发上传（Semaphore 有界；每分片完成
///    即更新共享位图 + 会话落盘）；任一失败 → abort 余量并上抛（已传
///    分片的位图资产保留在会话表）；
/// 5. `create`（原样重申 precreate 锁定的 block_list，31363）→ fs_id →
///    [`lookup_entry`]（顺带填句柄缓存）→ 会话作废。
pub(crate) async fn upload_whole_file(
    client: &Arc<BaiduClient>,
    volume: &VolumeId,
    root: &str,
    rel: &RelPath,
    data: Bytes,
    sessions: &SessionStore,
    handles: &Arc<HandleCache>,
) -> Result<Entry, StorageError> {
    if rel.is_root() {
        return Err(StorageError::Invalid); // 卷根不可作为上传目标
    }
    let abs = abs_of(root, rel);
    let size = data.len() as u64;
    // 0 字节 wire 真形（2026-09-23 真网实测，[`EMPTY_MD5`] 文档）：
    // `[EMPTY_MD5]` 替代空数组（errno=2）；该块免传（下方探活/差集过滤）。
    let block_md5: Vec<String> = if data.is_empty() {
        vec![EMPTY_MD5.to_string()]
    } else {
        data.chunks(CHUNK).map(md5_hex).collect()
    };

    // 路一：会话表命中 → 全量 md5 一致才复用（整文件在手，比对全量
    // 列表——比 stager 的前缀比对更严）；不一致 → 作废重建。
    let mut uploadid: Option<String> = None;
    let mut done: BTreeSet<u64> = BTreeSet::new();
    let mut rapid_fs_id: Option<i64> = None;
    if let Some(rec) = sessions.load(&abs, size).await {
        if rec.block_md5 == block_md5 {
            uploadid = Some(rec.uploadid.clone());
            done = rec.done.iter().copied().collect();
        } else {
            sessions.remove(&abs, size).await;
        }
    }

    // 探活（仅恢复会话且有缺失分片；首个缺失分片串行上传，stager 语义；
    // EMPTY_MD5 免传块不参与探活——无数据可发）。
    if let Some(uid) = uploadid.clone() {
        if let Some(probe_idx) = (0..block_md5.len() as u64)
            .find(|i| !done.contains(i) && block_md5[*i as usize] != EMPTY_MD5)
        {
            let part = part_slice(&data, probe_idx);
            match client
                .superfile2(&client.pcs_base, &abs, &uid, probe_idx, part)
                .await
            {
                Ok(()) => {
                    done.insert(probe_idx);
                    persist_whole_session(sessions, &abs, size, &block_md5, &uid, &done).await;
                }
                Err(_) => {
                    // 会话死（预期恢复路径）：作废 → 新 precreate 全量重传。
                    sessions.remove(&abs, size).await;
                    uploadid = None;
                    done.clear();
                }
            }
        }
    }

    // 路二/三：新 precreate（隐式建父 + 全量声明；rtype=3 覆盖语义）。
    if uploadid.is_none() && rapid_fs_id.is_none() {
        ensure_parents(client, root, rel, handles).await?;
        let outcome = api::precreate(client, &abs, size, &block_md5).await?;
        if outcome.return_type == 2 {
            rapid_fs_id = Some(outcome.fs_id); // 秒传腿：零分片零 create
        } else {
            uploadid = Some(outcome.uploadid);
            done.clear();
        }
    }

    // 秒传腿收尾：fs_id 直取 Entry（Entry.size 对齐实际上传量，stager
    // rapid 腿同款）。
    if let Some(fs_id) = rapid_fs_id {
        let entry = lookup_entry(client, volume, rel, &abs, handles, fs_id).await?;
        return Ok(Entry { size, ..entry });
    }

    let uploadid = uploadid.expect("装备后必有权柄");
    // 缺失分片并发上传（Semaphore 有界；每分片完成即位图落盘；EMPTY_MD5
    // 免传块服务端视为在位，不列缺失）。
    let missing: Vec<u64> = (0..block_md5.len() as u64)
        .filter(|i| !done.contains(i) && block_md5[*i as usize] != EMPTY_MD5)
        .collect();
    if !missing.is_empty() {
        let done_shared = Arc::new(Mutex::new(done));
        let sem = Arc::new(Semaphore::new(UPLOAD_WORKERS));
        let mut set: JoinSet<Result<u64, StorageError>> = JoinSet::new();
        for idx in missing {
            // 入队即取许可：任务存量恒 ≤ UPLOAD_WORKERS（内存有界——
            // Bytes 切片是引用计数，非拷贝）。
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| StorageError::Unavailable(format!("upload semaphore: {e}")))?;
            let client = Arc::clone(client);
            let abs = abs.clone();
            let uid = uploadid.clone();
            let part = part_slice(&data, idx);
            set.spawn(async move {
                let _permit = permit;
                client
                    .superfile2(&client.pcs_base, &abs, &uid, idx, part)
                    .await?;
                Ok(idx)
            });
        }
        let mut first_err: Option<StorageError> = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(idx)) => {
                    let snapshot = {
                        let mut guard = done_shared.lock().unwrap();
                        guard.insert(idx);
                        guard.clone()
                    };
                    persist_whole_session(sessions, &abs, size, &block_md5, &uploadid, &snapshot)
                        .await;
                }
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    set.abort_all(); // 不再烧流量（已传分片位图已在会话表）
                }
                Err(join_err) => {
                    if first_err.is_none() {
                        first_err = Some(StorageError::Unavailable(format!(
                            "upload worker join: {join_err}"
                        )));
                    }
                    set.abort_all();
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
    }

    // create 收尾：原样重申 precreate 会话锁定的 block_list（31363）。
    let fs_id = api::create_file(client, &abs, &uploadid, size, &block_md5).await?;
    let entry = lookup_entry(client, volume, rel, &abs, handles, fs_id).await?;
    sessions.remove(&abs, size).await;
    Ok(entry)
}

/// 整文件路径的会话即刻落盘（随分片完成；K7 契约——与 stager 的
/// `persist_session` 同一落点，位图形态一致）。
async fn persist_whole_session(
    sessions: &SessionStore,
    abs: &str,
    size: u64,
    block_md5: &[String],
    uploadid: &str,
    done: &BTreeSet<u64>,
) {
    sessions
        .put(SessionRecord {
            path: abs.to_string(),
            size,
            block_md5: block_md5.to_vec(),
            uploadid: uploadid.to_string(),
            done: done.iter().copied().collect(),
        })
        .await;
}

/// 全量缓冲 `data` 的第 `idx` 个 4MiB 分片（Bytes 切片 = 引用计数视图，
/// 并发 worker 零拷贝）。
fn part_slice(data: &Bytes, idx: u64) -> Bytes {
    let start = (idx as usize) * CHUNK;
    let end = ((idx as usize) + 1) * CHUNK;
    data.slice(start..end.min(data.len()))
}
