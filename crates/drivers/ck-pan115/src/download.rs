//! downurl 直链缓存 + CDN 有界窗口流读（Phase 5 / 115-2）。
//!
//! K69.4 实测约束（真机，2026-09-16）：
//!
//! - **UA 逐字节绑定**：`/open/ufile/downurl` 取链时的 UA 必须与随后
//!   CDN GET 的 UA 完全一致（错配恒 403，双向实证）；UA **形态**本身
//!   不约束（browser/spike/空 UA 三态全通）——驱动用恒定模块 UA
//!   （[`crate::api::UA`]）取链与下载，绑定自然成立。
//! - **CDN 403 = 限流**（先退避重试，再判定）；`401/410` 才是直链
//!   过期（重取直链一次）。
//! - **HEAD 探测**可用（`accept-ranges: bytes`、`etag` 存在且
//!   **etag = 文件 MD5**——完整性旁证，非本层消费面）。
//! - **Range 206**：必须校验返回码 206 与 `Content-Range` 起始偏移
//!   吻合（服务器忽略 Range 返 200 时按全量读会写偏——SFTP/baidu 同款
//!   纪律）。
//!
//! 缓存形态（baidu `DlinkCache` 先例）：pick_code → (url, fetched_at)；
//! TTL 缺省 30 分钟（K69.4：115 的直链 TTL 未实测，取保守下界；baidu
//! 的 60min 实证值不通用）。条目上界 [`DLINK_CAP`]——超限先清过期、
//! 仍满则驱逐最旧（K79 Q1）。取链一次一链，缓存命中零网络。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cloudkit_storage::{ByteStream, StorageError};
use futures_core::Stream;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::api::{Pan115Client, UA};

/// 直链缓存 TTL（K69.4 未实测 115 的 TTL——保守 30min；命中后任何
/// 403/401/410 都触发一次重取，缓存错值自愈）。
const DLINK_TTL: Duration = Duration::from_secs(30 * 60);

/// 直链缓存条目上界（K79 Q1，pathcache `DIR_CAP`/M-S5 同族）：长期
/// 挂载进程扫库大量文件时的内存上界——新键超上界时先清过期、仍满
/// 则驱逐最旧。
const DLINK_CAP: usize = 1024;

/// CDN 分片窗口：4 MiB（baidu 有界分片先例——115 未声明窗口约束，
/// 该值只是流控粒度，不涉协议）。
const WINDOW: u64 = 4 * 1024 * 1024;

/// CDN 403 退避梯度（限流形态；每次 1s→2s→4s，超限重取直链）。
const CDN_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// pick_code → 直链的 TTL 缓存。
#[derive(Default)]
pub struct DlinkCache {
    entries: RwLock<HashMap<String, (String, Instant)>>,
}

impl DlinkCache {
    pub fn new() -> Self {
        DlinkCache::default()
    }

    /// 命中且未过期 → 直链；否则 None（调用方取链）。
    pub async fn get(&self, pick_code: &str) -> Option<String> {
        let entries = self.entries.read().await;
        entries
            .get(pick_code)
            .filter(|(_, at)| at.elapsed() < DLINK_TTL)
            .map(|(url, _)| url.clone())
    }

    pub async fn insert(&self, pick_code: &str, url: &str) {
        let mut entries = self.entries.write().await;
        // 新键且超上界：先清过期条目（TTL 到点的读取面本就 miss，
        // 这里只是顺势回收），仍满则驱逐最旧。同键覆盖不进此分支
        // ——时间戳刷新 = 「最近用过」。
        if !entries.contains_key(pick_code) && entries.len() >= DLINK_CAP {
            let now = Instant::now();
            entries.retain(|_, (_, at)| now.duration_since(*at) < DLINK_TTL);
            if entries.len() >= DLINK_CAP {
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(k, _)| k.clone())
                {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(pick_code.to_string(), (url.to_string(), Instant::now()));
    }

    /// 失效一个键（重取前的显式清理形态；403/401/410 自愈路径调用）。
    pub async fn invalidate(&self, pick_code: &str) {
        let mut entries = self.entries.write().await;
        entries.remove(pick_code);
    }
}

/// 打开 `[start, end)` 的 CDN 流（有界窗口顺序拉取）。
///
/// 调用方（driver.rs `reader`）已完成句柄→(pick_code, size, isdir) 的
/// 解析与 range 钳制；本函数只管「直链 + CDN 字节流」。
pub(crate) async fn open_range(
    client: &std::sync::Arc<Pan115Client>,
    cache: &std::sync::Arc<DlinkCache>,
    pick_code: &str,
    size: u64,
    start: u64,
    end: u64,
) -> Result<ByteStream, StorageError> {
    if start >= end || start >= size {
        // 空窗口/越界起点：空流（conformance ② 声明形态，baidu 同款）。
        let (_tx, rx) = mpsc::channel::<Result<Bytes, StorageError>>(1);
        return Ok(Box::pin(ReceiverStream { rx })); // sender 即 drop：空流
    }
    let url = match cache.get(pick_code).await {
        Some(url) => url,
        None => {
            let url = fetch_dlink(client, pick_code).await?;
            cache.insert(pick_code, &url).await;
            url
        }
    };
    // 首帧前发一次 HEAD 探测（accept-ranges 证据；K69.4 实测可用）。
    // 探测失败不致命（老链路可能禁 HEAD）——降级为直接 Range GET，
    // 206 校验仍在。
    match head_probe(client, &url).await {
        Ok(true) | Err(_) => {}
        Ok(false) => {
            // 服务器显式声明不支持 Range：仅整读窗口可用——此处为
            // range 语义下的事实错误（驱动声明 range_read=true）。
            return Err(StorageError::Unavailable(
                "CDN reported no accept-ranges support (range_read capability would be false)"
                    .to_string(),
            ));
        }
    }
    let (tx, rx) = mpsc::channel::<Result<Bytes, StorageError>>(2);
    let client = client.clone();
    let cache_handle = cache.clone();
    let url = url.clone();
    let pc = pick_code.to_string();
    tokio::spawn(async move {
        let mut pos = start;
        while pos < end {
            let win_end = (pos + WINDOW).min(end); // 半开
            match get_window(&client, &url, pos, win_end).await {
                Ok(bytes) => {
                    // get_window 拒空体/越窗（M9）——此处只可能拿到非空
                    // 且 ≤ 窗长的体；少给非空按实收推进（续窗自洽）。
                    let n = bytes.len() as u64;
                    if tx.send(Ok(bytes)).await.is_err() {
                        return; // 消费方丢弃
                    }
                    pos += n;
                }
                Err(StorageError::RateLimited { .. }) => {
                    // CDN 403（限流形态）：退避后重取直链一次，再失败
                    // 即上报（CDN 侧限流与 API 限流不同池）。
                    let mut ok = false;
                    for delay in CDN_BACKOFF {
                        tokio::time::sleep(delay).await;
                        if let Ok(fresh) = fetch_dlink(&client, &pc).await {
                            cache_handle.insert(&pc, &fresh).await;
                            if let Ok(bytes) = get_window(&client, &fresh, pos, win_end).await {
                                if tx.send(Ok(bytes.clone())).await.is_err() {
                                    return;
                                }
                                // M9：按**实收字节**推进（旧 `pos += 窗长` 在
                                // 自愈 GET 短给时跳过未收字节 = 静默跳字节）。
                                pos += bytes.len() as u64;
                                ok = true;
                                break;
                            }
                        }
                    }
                    if !ok {
                        let _ = tx
                            .send(Err(StorageError::RateLimited { retry_after: None }))
                            .await;
                        return;
                    }
                }
                Err(StorageError::NotFound) => {
                    // 直链过期（401/410）：失效缓存 + 重取直链 + 重试当前
                    // 窗口一次；重取或重试失败即上报（30min TTL 只在 open
                    // 时检查——中途回 401/410 由这里兜底，模块文档声明
                    // 的自愈语义）。
                    cache_handle.invalidate(&pc).await;
                    let healed = async {
                        let fresh = fetch_dlink(&client, &pc).await?;
                        cache_handle.insert(&pc, &fresh).await;
                        get_window(&client, &fresh, pos, win_end).await
                    }
                    .await;
                    match healed {
                        Ok(bytes) => {
                            let n = bytes.len() as u64;
                            if tx.send(Ok(bytes)).await.is_err() {
                                return;
                            }
                            pos += n;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });
    Ok(Box::pin(ReceiverStream { rx }))
}

/// mpsc 接收端 → [`futures_core::Stream`] 适配（ByteStream 产生面；
/// ck-baidu download.rs 同款手写——不引 tokio-stream）。
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

/// 取直链（缓存未命中/失效路径）。UA = 模块恒定值——取链与 Get 同源
/// 即满足逐字节绑定（K69.4）。
async fn fetch_dlink(client: &Pan115Client, pick_code: &str) -> Result<String, StorageError> {
    client.downurl(pick_code, UA).await
}

/// HEAD 探测：`accept-ranges: bytes` → true；服务器无该头 → false；
/// 网络类失败 → Err（调用方降级放行）。
async fn head_probe(client: &Pan115Client, url: &str) -> Result<bool, StorageError> {
    let resp = client
        .http()
        .head(url)
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .map_err(|e| StorageError::Unavailable(format!("CDN HEAD: {}", e.without_url())))?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        return Err(StorageError::Unavailable(format!(
            "CDN HEAD: HTTP {}",
            resp.status().as_u16()
        )));
    }
    Ok(resp
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("bytes")))
}

/// 拉一个 `[start, end)` 窗口：Range GET + 206/Content-Range 校验。
///
/// 返回码语义（K69.4）：403 → `RateLimited`（CDN 限流，退避路径）；
/// 401/410 → `NotFound`（直链过期——流循环失效缓存并重取直链，M-S3）；
/// 其余非 200/206 或 Content-Range 不吻合 → `Unavailable`。
async fn get_window(
    client: &Pan115Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<Bytes, StorageError> {
    let resp = client
        .http()
        .get(url)
        // 半开 → 闭区间（HTTP 语义）
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start, end - 1),
        )
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .map_err(|e| StorageError::Unavailable(format!("CDN GET: {}", e.without_url())))?;
    let status = resp.status();
    if status.as_u16() == 403 {
        return Err(StorageError::RateLimited { retry_after: None });
    }
    if status.as_u16() == 401 || status.as_u16() == 410 {
        // 直链过期（auth/寿命形态，K69.4）：`NotFound` 作为流循环的
        // 「失效缓存 + 重取直链」信号——本函数只在 CDN 面产出该变体，
        // 与 403（限流→退避梯度）分道（M-S3：曾直接归 Unavailable
        // 杀死长流）。
        return Err(StorageError::NotFound);
    }
    if !status.is_success() {
        return Err(StorageError::Unavailable(format!(
            "CDN GET: HTTP {}",
            status.as_u16()
        )));
    }
    if status.as_u16() != 206 {
        return Err(StorageError::Unavailable(format!(
            "CDN GET ignored the Range header (HTTP {} — expected 206)",
            status.as_u16()
        )));
    }
    // Content-Range 起始偏移必须吻合（写偏防线）。
    let content_range = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let expect = format!("bytes {start}-{}/", end - 1);
    if !content_range.starts_with(&expect) {
        return Err(StorageError::Unavailable(format!(
            "CDN Content-Range mismatch: got {content_range:?}, expected prefix {expect:?}"
        )));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| StorageError::Unavailable(format!("CDN body: {}", e.without_url())))?;
    // M9 窗口长度防线（pan123 M6 形态同款纪律）：只拦「越窗多给」与
    // 「空体」——
    // - 越窗多给 → 整窗拒绝（多余字节透传后按窗拼接即错位 = 数据
    //   损坏面）；
    // - 空体（期望非零）→ 拒（旧「读流静默 break」= 零信号截断）；
    // - 少给非空保留（续窗自洽：消费方按实收字节推进 pos）。
    let expected = end - start;
    if body.len() as u64 > expected {
        return Err(StorageError::Unavailable(format!(
            "CDN 206 body exceeds the requested window: got {} bytes, expected {} \
             (Content-Range {content_range:?})",
            body.len(),
            expected,
        )));
    }
    if body.is_empty() {
        return Err(StorageError::Unavailable(
            "CDN returned an empty 206 body".to_string(),
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// K79 Q1：容量上限——超出 DLINK_CAP 时最旧条目被驱逐、新近条目
    /// 保留；同键覆盖刷新时间戳（覆盖本身不触发驱逐）——pathcache
    /// `DIR_CAP`（M-S5）同族。
    #[tokio::test]
    async fn dlink_cache_caps_and_evicts_the_oldest_entry() {
        let cache = DlinkCache::new();
        for i in 0..DLINK_CAP {
            cache
                .insert(&format!("pc{i}"), &format!("http://x/{i}"))
                .await;
        }
        assert_eq!(
            cache.get("pc0").await.as_deref(),
            Some("http://x/0"),
            "满 cap：全量存活（不提前驱逐）"
        );
        // 同键覆盖 = 刷新时间戳：pc0 从最旧变为最新。
        cache.insert("pc0", "http://x/0-fresh").await;
        // 超上界再插一条：最旧（此刻是 pc1）被驱逐。
        cache
            .insert(&format!("pc{}", DLINK_CAP), "http://x/new")
            .await;
        assert_eq!(
            cache.get("pc0").await.as_deref(),
            Some("http://x/0-fresh"),
            "覆盖刷新过的键在驱逐中存活"
        );
        assert_eq!(cache.get("pc1").await, None, "超上界时最旧条目被驱逐");
        assert!(
            cache.get(&format!("pc{}", DLINK_CAP)).await.is_some(),
            "新键照常落缓存"
        );
        assert!(
            cache.get(&format!("pc{}", DLINK_CAP - 1)).await.is_some(),
            "新近条目保留"
        );
    }
}
