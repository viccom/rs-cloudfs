//! 下载器：dlink 缓存（K8）+ 4MiB 有界 Range 分片流（K9；Batch B2）。
//!
//! 黄金参照：spike §5 dl-try 矩阵 + 附录 B TTL 实测（形态源
//! `examples/baidu_spike/src/api.rs:377-431`）。
//!
//! ## CDN 下载三约束（违反任一 → 403 error_code=31326）
//!
//! 1. netdisk 族 UA——client 构造恒定（client.rs `UA`）；
//! 2. **有界** Range ≤4MiB（无 Range/开放区间/超界全拒）——本模块的
//!    分片窗口恒有界 `bytes=s-e` 且 span ≤4MiB；
//! 3. dlink `expires` 过期不可救（追加 access_token 无效——重取 dlink
//!    才可恢复）。
//!
//! ## 两段 fallback（spike §5：直连与「追加 access_token」两态都出现过）
//!
//! 403 → 同 URL 追加 access_token 重试一次 → 仍 403 → 重取 dlink
//! （失效缓存）→ 新链直连 → 仍 403 → 新链追加 token → 仍败 → 终态
//! `Unauthorized { recoverable: true }`（31326 的映射表语义）。
//!
//! ## dlink 缓存（K8）
//!
//! 按 fs_id 缓存 `(url, expires_at)`，TTL = `dlink_ttl_secs`（缺省 3600s
//! ——实测下界 ≥56min 的保守值）；TTL 内重复读不重取；过期/403 兜底
//! 重取。并发读者共享缓存（短临界区 Mutex，无 await 持锁）。
//!
//! ## 流形态
//!
//! 最简**顺序**逐分片实现（mock 无延迟，顺序足够；4 并发预取的收益面
//! 在真机吞吐——B3b transport_face 演进时按需优化）：后台任务按窗口
//! 序拉取，经有界 mpsc 供帧——对上层是单一 [`ByteStream`]，Range>4MiB
//! 的窗口拼接全程透明（conformance ②/⑧）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;
use tokio::sync::mpsc;

use cloudkit_storage::{ByteStream, EntryId, Range, StorageError, VolumeId};

use crate::api;
use crate::client::BaiduClient;
use crate::upload::CHUNK;

/// dlink 缓存（fs_id → (url, expires_at)；driver 与流任务经 Arc 共享）。
pub(crate) struct DlinkCache {
    map: Mutex<HashMap<String, (String, Instant)>>,
    ttl: Duration,
}

impl DlinkCache {
    pub(crate) fn new(ttl: Duration) -> Self {
        DlinkCache {
            map: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 取未过期 dlink（过期项惰性剔除）。
    fn get(&self, fs_id: &str) -> Option<String> {
        let mut map = self.map.lock().unwrap();
        if let Some((url, expires)) = map.get(fs_id) {
            if *expires > Instant::now() {
                return Some(url.clone());
            }
        }
        map.remove(fs_id);
        None
    }

    /// 写缓存（TTL 从 now 起算）。
    fn insert(&self, fs_id: &str, url: &str) {
        let expires = Instant::now() + self.ttl;
        self.map
            .lock()
            .unwrap()
            .insert(fs_id.to_string(), (url.to_string(), expires));
    }
}

/// 打开读取流（driver.rs 委派）。
///
/// - 句柄 = fs_id（K5）：他卷 → `NotFound`；不可解析 → `Invalid`；
///   指向目录 → `Invalid`（trait 契约）；
/// - `range = None` 整读 `[0, size)`；`Some` 半开语义：end 越界钳制到
///   EOF，`start >= size` / 空窗口 → 空流（conformance ② 声明形态）；
/// - fs_id → path/size 经 meta 直查（dlink 签发按 path，spike §5）。
pub(crate) async fn open_range(
    client: &Arc<BaiduClient>,
    cache: &Arc<DlinkCache>,
    volume: &VolumeId,
    id: &EntryId,
    range: Option<Range>,
) -> Result<ByteStream, StorageError> {
    if id.volume != *volume {
        return Err(StorageError::NotFound); // 他卷句柄（trait 契约）
    }
    let fs_id: i64 = id
        .handle
        .as_str()
        .parse()
        .map_err(|_| StorageError::Invalid)?;
    let remote = api::meta_by_fs_id(client, &fs_id.to_string()).await?; // -9 → NotFound
    if remote.isdir != 0 {
        return Err(StorageError::Invalid); // 目录不可读
    }
    let size = remote.size.max(0) as u64;
    let (start, end) = match range {
        None => (0u64, size),
        Some(r) => (r.start, r.end.unwrap_or(size).min(size)),
    };
    if start >= size || end <= start {
        // 空窗口/越界起点：空流（harness empty_range_yields_empty_stream=true）。
        let (_tx, rx) = mpsc::channel::<Result<Bytes, StorageError>>(1);
        return Ok(Box::pin(ReceiverStream { rx })); // sender 即 drop：空流
    }
    let fs_key = fs_id.to_string();
    let dlink = match cache.get(&fs_key) {
        Some(url) => url,
        None => {
            let url = client.fetch_dlink(&remote.path).await?;
            cache.insert(&fs_key, &url);
            url
        }
    };
    // 分片后台任务：按 4MiB 有界窗口顺序拉取，mpsc 按序供帧。
    let (tx, rx) = mpsc::channel::<Result<Bytes, StorageError>>(2);
    let client = client.clone();
    let cache = cache.clone();
    let path = remote.path.clone();
    tokio::spawn(async move {
        let mut url = dlink;
        let mut cur = start;
        while cur < end {
            let win_end = (cur + CHUNK as u64).min(end) - 1; // 闭区间终
            match fetch_window(&client, &cache, &fs_key, &path, &mut url, cur, win_end).await {
                Ok(bytes) => {
                    cur = win_end + 1;
                    if tx.send(Ok(bytes)).await.is_err() {
                        break; // 消费方已放弃
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });
    Ok(Box::pin(ReceiverStream { rx }))
}

/// 拉取单窗口 `[start, win_end]`（闭区间；span ≤4MiB），带两段 fallback。
///
/// `url` 是当前 dlink（可变——第三段 fallback 重取后更新，供后续窗口
/// 复用新链）。
async fn fetch_window(
    client: &Arc<BaiduClient>,
    cache: &Arc<DlinkCache>,
    fs_key: &str,
    path: &str,
    url: &mut String,
    start: u64,
    win_end: u64,
) -> Result<Bytes, StorageError> {
    let expect = (win_end - start + 1) as usize;
    // 第一段：直连；第二段：同 URL 追加 access_token。
    if let Some(bytes) = cdn_get(client, url, start, win_end, false, expect).await? {
        return Ok(bytes);
    }
    if let Some(bytes) = cdn_get(client, url, start, win_end, true, expect).await? {
        return Ok(bytes);
    }
    // 第三段：重取 dlink（旧链彻底不可用——过期链 token 救不了）→ 新链
    // 两段再试。
    let fresh = client.fetch_dlink(path).await?;
    cache.insert(fs_key, &fresh);
    *url = fresh.clone();
    if let Some(bytes) = cdn_get(client, &fresh, start, win_end, false, expect).await? {
        return Ok(bytes);
    }
    if let Some(bytes) = cdn_get(client, &fresh, start, win_end, true, expect).await? {
        return Ok(bytes);
    }
    // 两段 fallback 全败：31326 映射语义（重取 dlink/追 token 可救的
    // 都已试过——cdn 链路级鉴权故障）。
    Err(StorageError::Unauthorized { recoverable: true })
}

/// 单次 CDN GET：有界 Range + 期望长度校验。
///
/// `Ok(None)` = 403（fallback 信号）；`Ok(Some)` = 206 且长度匹配；
/// `Err` = 传输/协议错误（非 403/206 的状态、长度不符）。
async fn cdn_get(
    client: &Arc<BaiduClient>,
    url: &str,
    start: u64,
    win_end: u64,
    append_token: bool,
    expect: usize,
) -> Result<Option<Bytes>, StorageError> {
    let mut target = reqwest::Url::parse(url)
        .map_err(|e| StorageError::Unavailable(format!("dlink url parse: {e}")))?;
    if append_token {
        // 同 URL 追加 access_token（query 形态——dlink 常驻 query 已存在，
        // append_pair 不破坏既有参数）。
        let token = client.tokens.read().await.access.clone();
        target.query_pairs_mut().append_pair("access_token", &token);
    }
    let resp = client
        .stream
        .get(target)
        .header("Range", format!("bytes={start}-{win_end}"))
        .send()
        .await
        .map_err(|e| {
            // without_url：CDN URL query 含签名/token（R3）。
            StorageError::Unavailable(format!("cdn transport: {}", e.without_url()))
        })?;
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN {
        return Ok(None); // 403/31326 族：fallback 信号
    }
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        let snippet: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect();
        return Err(StorageError::Unavailable(format!(
            "cdn unexpected http {status}: {snippet}"
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| StorageError::Unavailable(format!("cdn body: {}", e.without_url())))?;
    if bytes.len() != expect {
        return Err(StorageError::Unavailable(format!(
            "cdn window length mismatch: got {} expect {expect}",
            bytes.len()
        )));
    }
    Ok(Some(bytes))
}

/// mpsc 接收端 → [`futures_core::Stream`] 适配（ByteStream 产生面）。
struct ReceiverStream {
    rx: mpsc::Receiver<Result<Bytes, StorageError>>,
}

impl Stream for ReceiverStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}
