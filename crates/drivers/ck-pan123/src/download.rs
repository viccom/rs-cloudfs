//! download_info 直链缓存 + 三跳解析 + CDN 有界窗口流读（Phase 6 /
//! 123-2；任务 E——本批最难件）。
//!
//! 123-0 真机实证的下载链怪癖（spike ④）：
//!
//! ```text
//! download_info(/a/ 新形态) → data.DownloadUrl = web-pro2 中继 URL
//!   ① 中继页 HTTP 200 text/html 无 href → 从 URL 自身 params= 段
//!      urlsafe-base64 自解码（纯解析服务端自然返回的 URL——非 D5
//!      排除的 auto_redirect 注入链）
//!   ② CDN 域可能回 HTTP 210 + Content-Type:json +
//!      {"code":0,"data":{"redirect_url":...}}（JSON 重定向体）→ 跟
//!      redirect_url
//!   ③ 镜像域 206（Range 逐字节 MATCH；Content-Range 校验）
//! ```
//!
//! 纪律：
//!
//! - **≤3 跳封顶**（[`MAX_HOPS`]——30x Location 头的正常重定向形态也
//!   容忍，同受封顶约束；传输会话 `Policy::none()` 手动跟）；
//! - **dlink 一次性纪律**（§5.14）：**禁止重复 GET 探测**——解析不单独
//!   探测，**首窗口 GET 即解析载体**（206 的落点 URL 直接入缓存），
//!   后续窗口对缓存 URL 单发；缓存命中后 404/410 → 失效缓存 + 重取
//!   直链一次自愈（M-S3 同源）；
//! - **双会话分离**（§5.12）：CDN GET 走裸 transfer client（仅 UA，无
//!   123pan 鉴权头/Cookie）；
//! - **traffic 预检**（§5.7）：取链前 `traffic/check`——
//!   `isTrafficExceeded:true` → `RateLimited`（D5：不绕过，人话指引经
//!   warn）；`isBlocked` 含义未明**不作为阻断依据**（跟踪单挂账）；
//!   download_info 回 5113/5114 → 同样 `RateLimited`（errno 表）；
//! - **Range 206/Content-Range 校验**：起始偏移必须吻合（服务器忽略
//!   Range 回 200 时按全量读会写偏——pan115/baidu 同款纪律）。
//!
//! 缓存形态（baidu/pan115 `DlinkCache` 先例）：file_id → (最终应答
//! URL, fetched_at)；TTL 缺省 **15 分钟**（保守下界——真值未测挂账；
//! baidu 60min 实证值不通用，115 取 30min，123 更保守一档）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use cloudkit_storage::{ByteStream, StorageError};
use futures_core::Stream;
use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::api::Pan123Client;
use crate::models::FileEntry;

/// 直链缓存 TTL（真值未测——保守 15min；命中后 404/410 自愈重取）。
const DLINK_TTL: Duration = Duration::from_secs(15 * 60);
/// CDN 分片窗口：4 MiB（baidu/pan115 有界分片先例——流控粒度）。
const WINDOW: u64 = 4 * 1024 * 1024;
/// 解析跳数封顶（§5.14：最深跟 3 跳——30x 与 210-JSON 都计入）。
const MAX_HOPS: u32 = 3;
/// CDN 403/429 退避梯度（限流形态；每次 1s→2s→4s，超限走重取直链）。
const CDN_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// file_id → 最终应答 URL 的 TTL 缓存。
#[derive(Default)]
pub struct DlinkCache {
    entries: RwLock<HashMap<String, (String, Instant)>>,
}

impl DlinkCache {
    pub fn new() -> Self {
        DlinkCache::default()
    }

    /// 命中且未过期 → URL；否则 None（调用方取链）。
    pub async fn get(&self, file_id: i64) -> Option<String> {
        let entries = self.entries.read().await;
        entries
            .get(&file_id.to_string())
            .filter(|(_, at)| at.elapsed() < DLINK_TTL)
            .map(|(url, _)| url.clone())
    }

    pub async fn insert(&self, file_id: i64, url: &str) {
        let mut entries = self.entries.write().await;
        entries.insert(file_id.to_string(), (url.to_string(), Instant::now()));
    }

    /// 失效一个键（直链死亡自愈路径调用）。
    pub async fn invalidate(&self, file_id: i64) {
        let mut entries = self.entries.write().await;
        entries.remove(&file_id.to_string());
    }
}

/// 单次 GET 的解析产出：终态字节或下一跳 URL。
enum HopOutcome {
    /// 206 窗口命中（字节 + 应答 URL）。
    Final(Bytes),
    /// 30x Location / 210-JSON redirect_url / 200-HTML href → 下一跳。
    Redirect(String),
}

/// 打开 `[start, end)` 的 CDN 流（有界窗口顺序拉取）。
///
/// 调用方（lib.rs `reader`）已完成句柄→FileEntry 的解析与 range 钳制；
/// 本函数只管「traffic 预检 + 直链 + 三跳解析 + CDN 字节流」。首窗口
/// GET 同时是解析载体（dlink 一次性纪律——绝不单独探测 GET）。
pub(crate) async fn open_range(
    client: &Arc<Pan123Client>,
    cache: &Arc<DlinkCache>,
    entry: &FileEntry,
    start: u64,
    end: u64,
) -> Result<ByteStream, StorageError> {
    let size = entry.size.max(0) as u64;
    if start >= end || start >= size {
        // 空窗口/越界起点：空流（conformance ② 声明形态，baidu 同款）。
        let (_tx, rx) = mpsc::channel::<Result<Bytes, StorageError>>(1);
        return Ok(Box::pin(ReceiverStream { rx })); // sender 即 drop：空流
    }
    let end = end.min(size);
    let win_end = (start + WINDOW).min(end);
    // 首窗口：缓存命中直接 GET；未命中（或死亡自愈）走冷解析（traffic
    // 预检 + download_info + 跳链）——GET 本身就是跳链的推进器。
    let (first, _landed_url) = match cache.get(entry.file_id).await {
        Some(url) => match window_get(client, &url, start, win_end).await {
            Ok(HopOutcome::Final(bytes)) => (bytes, url),
            // 缓存 URL 上的中途重定向形态：从该跳继续（跳数预算内）。
            Ok(HopOutcome::Redirect(next)) => {
                follow_hops(client, cache, entry.file_id, next, start, win_end).await?
            }
            // 缓存直链死亡（404/410）：失效 + 冷解析一次（M-S3 自愈）。
            Err(StorageError::NotFound) => {
                cache.invalidate(entry.file_id).await;
                cold_resolve(client, cache, entry, start, win_end).await?
            }
            Err(e) => return Err(e),
        },
        None => cold_resolve(client, cache, entry, start, win_end).await?,
    };
    let (tx, rx) = mpsc::channel::<Result<Bytes, StorageError>>(2);
    let client = Arc::clone(client);
    let cache = Arc::clone(cache);
    let entry = entry.clone();
    tokio::spawn(async move {
        let mut pos = start + first.len() as u64;
        if tx.send(Ok(first)).await.is_err() {
            return; // 消费方丢弃
        }
        while pos < end {
            let win_end = (pos + WINDOW).min(end); // 半开
            match fetch_window(&client, &cache, &entry, pos, win_end).await {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        break; // 服务端提前 EOF：停止（截断流为真）
                    }
                    let n = bytes.len() as u64;
                    if tx.send(Ok(bytes)).await.is_err() {
                        return; // 消费方丢弃
                    }
                    pos += n;
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

/// 窗口获取：直链 GET + 限流退避梯度 + 死亡/重定向自愈（一次）。
async fn fetch_window(
    client: &Arc<Pan123Client>,
    cache: &Arc<DlinkCache>,
    entry: &FileEntry,
    pos: u64,
    win_end: u64,
) -> Result<Bytes, StorageError> {
    if let Some(url) = cache.get(entry.file_id).await {
        for delay in CDN_BACKOFF {
            match window_get(client, &url, pos, win_end).await {
                Ok(HopOutcome::Final(bytes)) => return Ok(bytes),
                Ok(HopOutcome::Redirect(next)) => {
                    // 中途重定向形态：跳过去（follow_hops 内封顶），落地
                    // 则刷新缓存。
                    if let Ok((bytes, _)) =
                        follow_hops(client, cache, entry.file_id, next, pos, win_end).await
                    {
                        return Ok(bytes);
                    }
                    break;
                }
                Err(StorageError::RateLimited { .. }) => {
                    tokio::time::sleep(delay).await;
                }
                Err(StorageError::NotFound) => break, // 直链死亡 → 自愈路径
                Err(e) => return Err(e),
            }
        }
    }
    // 死亡/梯度用尽/缓存过期：失效缓存 + 冷解析一次（窗口即解析载体）。
    cache.invalidate(entry.file_id).await;
    let (bytes, _fresh) = cold_resolve(client, cache, entry, pos, win_end).await?;
    Ok(bytes)
}

/// 冷解析：traffic 预检 + download_info 取中继 URL + params 自解码 +
/// 跳链 GET（首窗口 GET 即解析载体——206 的落点 URL 入缓存）。
async fn cold_resolve(
    client: &Arc<Pan123Client>,
    cache: &Arc<DlinkCache>,
    entry: &FileEntry,
    start: u64,
    win_end: u64,
) -> Result<(Bytes, String), StorageError> {
    // traffic 预检（§5.7）：超额 → RateLimited（D5 不绕过；人话经 warn
    // 已在 errno/dispatch 面给出——5113/5114 走 download_info 的映射）。
    let status = client.traffic_check(&[entry.file_id]).await?;
    if status.is_traffic_exceeded {
        tracing::warn!(
            target: "ck_pan123::download",
            file_id = entry.file_id,
            remain_bytes = status.original_remain_traffic,
            "123pan daily download traffic quota exceeded (D5: not bypassed): traffic resets \
             tomorrow, or a 123pan VIP subscription lifts the cap"
        );
        return Err(StorageError::RateLimited { retry_after: None });
    }
    // download_info（5113/5114 → RateLimited 由 errno 表映射）。
    let relay = client.download_info(entry).await?;
    // 第一跳优先纯解析：URL 自身 params= 段自解码（零网络——dlink
    // 一次性纪律：绝不单独探测 GET）。
    let candidate = decode_relay_params(&relay).unwrap_or(relay);
    follow_hops(client, cache, entry.file_id, candidate, start, win_end).await
}

/// 从 candidate 起 GET 推进跳链（≤[`MAX_HOPS`] 跳）：206 落点 URL 入
/// 缓存并返回字节。
async fn follow_hops(
    client: &Arc<Pan123Client>,
    cache: &Arc<DlinkCache>,
    file_id: i64,
    candidate: String,
    start: u64,
    win_end: u64,
) -> Result<(Bytes, String), StorageError> {
    let mut candidate = candidate;
    let mut hops = 0u32;
    loop {
        match window_get(client, &candidate, start, win_end).await {
            Ok(HopOutcome::Final(bytes)) => {
                cache.insert(file_id, &candidate).await;
                return Ok((bytes, candidate));
            }
            Ok(HopOutcome::Redirect(next)) => {
                hops += 1;
                if hops >= MAX_HOPS {
                    return Err(StorageError::Unavailable(format!(
                        "download resolution exceeded {MAX_HOPS} hops"
                    )));
                }
                candidate = next;
            }
            Err(e) => return Err(e),
        }
    }
}

/// web-pro2 中继 URL 的 `params=` 段自解码（123-0 真机实证形态：
/// `.../download-v2/?params=<urlsafe-base64 的 CDN URL>&is_s3=0`）。
///
/// 纯字符串解析（非 D5 排除的 auto_redirect 注入）；非该形态/解码失败
/// → None（调用方按普通 URL GET——30x/210-JSON 形态兜底）。
pub(crate) fn decode_relay_params(url: &str) -> Option<String> {
    if !url.contains("/download-v2/") {
        return None;
    }
    let params = url.split("params=").nth(1)?.split('&').next()?;
    // urlsafe base64 + 缺失填充
    let std_form = params.replace('-', "+").replace('_', "/");
    let padded = match std_form.len() % 4 {
        2 => format!("{std_form}=="),
        3 => format!("{std_form}="),
        _ => std_form,
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    (s.starts_with("http")).then_some(s)
}

/// 拉一个 `[start, end)` 窗口：Range GET + 解析形态分派。
///
/// 返回码语义：206 → [`HopOutcome::Final`]（Content-Range 起始吻合
/// 校验）；30x+Location / 210+JSON `redirect_url` / 200+HTML href →
/// [`HopOutcome::Redirect`]；403/429 → `RateLimited`（退避路径）；
/// 404/410 → `NotFound`（直链死亡信号）；200 非 HTML → Range 被忽略
/// （写偏防线）；其余 → `Unavailable`。
async fn window_get(
    client: &Pan123Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<HopOutcome, StorageError> {
    let http = client.transfer_http();
    let resp = http
        .get(url)
        // 半开 → 闭区间（HTTP 语义）
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start, end - 1),
        )
        .send()
        .await
        .map_err(|e| StorageError::Unavailable(format!("CDN GET: {}", e.without_url())))?;
    let status = resp.status().as_u16();
    if status == 403 || status == 429 {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(StorageError::RateLimited { retry_after });
    }
    if status == 404 || status == 410 {
        // 直链死亡/寿命形态：`NotFound` 作为「失效缓存 + 重取直链」信号
        // （M-S3 同源裁决）。
        return Err(StorageError::NotFound);
    }
    if (300..400).contains(&status) {
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        return match location {
            Some(next) => Ok(HopOutcome::Redirect(next)),
            None => Err(StorageError::Unavailable(format!(
                "CDN redirect without Location (HTTP {status})"
            ))),
        };
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if status == 206 {
        // Content-Range 起始偏移必须吻合（写偏防线）。
        let content_range = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let expect = format!("bytes {start}-");
        if !content_range.starts_with(&expect) {
            return Err(StorageError::Unavailable(format!(
                "CDN Content-Range mismatch: got {content_range:?}, expected prefix {expect:?}"
            )));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| StorageError::Unavailable(format!("CDN body: {}", e.without_url())))?;
        return Ok(HopOutcome::Final(body));
    }
    // 210（或任何 2xx 非 206）+ JSON 体 = 重定向体形态（spike 实证
    // HTTP 210 + {"code":0,"data":{"redirect_url":...}}）。
    let body = resp
        .text()
        .await
        .map_err(|e| StorageError::Unavailable(format!("CDN body: {}", e.without_url())))?;
    if content_type.contains("json") || body.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            let next = v
                .get("data")
                .and_then(|d| {
                    d.get("redirect_url")
                        .or_else(|| d.get("RedirectUrl"))
                        .or_else(|| d.get("DownloadUrl"))
                        .or_else(|| d.get("downloadUrl"))
                })
                .or_else(|| v.get("redirect_url").or_else(|| v.get("RedirectUrl")))
                .and_then(|u| u.as_str())
                .filter(|s| s.starts_with("http"))
                .map(str::to_string);
            if let Some(next) = next {
                return Ok(HopOutcome::Redirect(next));
            }
        }
        return Err(StorageError::Unavailable(format!(
            "CDN JSON body without a redirect url (HTTP {status})"
        )));
    }
    if content_type.contains("html") || body.starts_with("<!DOCTYPE") || body.starts_with("<html") {
        // 中继 HTML 页：扫整页 href（双引号形态；spike 实证真机页无
        // href——此为防御腿，主路径是 params= 自解码）。
        if let Some(href) = extract_href(&body) {
            return Ok(HopOutcome::Redirect(href));
        }
        return Err(StorageError::Unavailable(
            "CDN relay HTML without a resolvable link".to_string(),
        ));
    }
    if status == 200 {
        return Err(StorageError::Unavailable(
            "CDN GET ignored the Range header (HTTP 200 — expected 206)".to_string(),
        ));
    }
    Err(StorageError::Unavailable(format!("CDN GET: HTTP {status}")))
}

/// HTML 体内首个 `href='...'`/`href="..."` http(s) 链接（spike 的
/// 全页双引号形态扫描）。
fn extract_href(html: &str) -> Option<String> {
    for marker in ["href='", "href=\""] {
        if let Some(pos) = html.find(marker) {
            let start = pos + marker.len();
            let rest = &html[start..];
            let end = marker
                .ends_with('\'')
                .then(|| rest.find('\''))
                .flatten()
                .or_else(|| rest.find('"'));
            if let Some(end) = end {
                let url = &rest[..end];
                if url.starts_with("http") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// mpsc 接收端 → [`futures_core::Stream`] 适配（ByteStream 产生面；
/// ck-baidu/ck-pan115 download.rs 同款手写——不引 tokio-stream）。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// params= 自解码：真机形态（urlsafe + 缺填充）与拒绝形态。
    #[test]
    fn relay_params_decode_covers_the_live_and_rejected_shapes() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let inner = "https://cnc-pro-cd.123pan.cn/a/b?sig=x";
        let b64 = URL_SAFE_NO_PAD.encode(inner);
        let relay = format!("https://web-pro2.123952.com/download-v2/?params={b64}&is_s3=0");
        assert_eq!(decode_relay_params(&relay).as_deref(), Some(inner));

        // 带 padding 的标准 urlsafe 形态同样可解。
        let b64_pad = base64::engine::general_purpose::URL_SAFE.encode(inner);
        assert_eq!(
            decode_relay_params(&format!(
                "https://web-pro2.123952.com/download-v2/?params={b64_pad}"
            ))
            .as_deref(),
            Some(inner)
        );

        // 拒绝形态：非 download-v2 URL / 解码产物非 http。
        assert_eq!(
            decode_relay_params("https://example.com/other?params=xa"),
            None
        );
        assert_eq!(
            decode_relay_params(&format!(
                "https://web-pro2.123952.com/download-v2/?params={}",
                URL_SAFE_NO_PAD.encode("not-a-url")
            )),
            None
        );
    }

    /// href 扫描：双引号/单引号/非 http 拒绝。
    #[test]
    fn href_scan_matches_both_quote_styles() {
        assert_eq!(
            extract_href(r#"<a href="https://m.example/x">x</a>"#).as_deref(),
            Some("https://m.example/x")
        );
        assert_eq!(
            extract_href("<a href='https://m.example/y'>y</a>").as_deref(),
            Some("https://m.example/y")
        );
        assert_eq!(extract_href(r#"<a href="/rel">r</a>"#), None);
        assert_eq!(extract_href("no links"), None);
    }

    /// dlink 缓存 TTL 与失效。
    #[tokio::test]
    async fn dlink_cache_expires_and_invalidates() {
        let cache = DlinkCache::new();
        cache.insert(7, "http://x/7").await;
        assert_eq!(cache.get(7).await.as_deref(), Some("http://x/7"));
        cache.invalidate(7).await;
        assert_eq!(cache.get(7).await, None);
    }
}
