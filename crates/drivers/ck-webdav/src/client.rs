//! 薄客户端骨架（Phase 7 / WD1a）。
//!
//! 构造面本批落地：reqwest Client 构建（rustls + connect 超时 +
//! `accept_invalid_certs` 开洞的一次性 warn——计划 §8-D3）+ 基地址与
//! 认证协商状态（[`AuthState`]）的就位。**动词面（PROPFIND/GET/PUT/
//! MKCOL/DELETE/MOVE/PROPPATCH/OPTIONS）全部占位**——重试白名单、
//! 超时分层的作用点、206 校验、401 协商重放是 WD2 的接线面（各方法
//! 的 `TODO(wd2)` 锚）。
//!
//! ## 代理语义（与 baidu/pan123 的 `no_proxy` 直连相反）
//!
//! **不 no_proxy**：本仓 http 层有代理世界观（telegram 必须走代理），
//! WebDAV 是用户自备服务器——用 reqwest 默认（尊重系统代理 env），
//! 由用户环境决定直连或代理。自签内网 NAS 场景经
//! `webdav_accept_invalid_certs` 开洞（D3）。
//!
//! ## 超时分层（计划 §4.5-7）
//!
//! connect 15s / 控制面（PROPFIND/MKCOL/MOVE/DELETE/PROPPATCH/
//! OPTIONS）30s / 窗口 GET 120s（8 MiB 有界窗口 → 超时安全——WD2
//! 窗口流的常量消费点在这里）。

use std::time::Duration;

use url::Url;

use cloudkit_storage::StorageError;

use crate::auth::AuthState;
use crate::config::WebdavParams;

/// TCP/TLS 连接建立预算（分层之一；rs-f4ss 骨架同值）。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 控制面动词预算（PROPFIND/MKCOL/MOVE/DELETE/PROPPATCH/OPTIONS）。
#[allow(dead_code)] // WD1a 骨架：WD2 动词面的 per-request 超时消费点
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// 窗口 GET 预算（8 MiB 有界窗口上的读超时——慢滴流注入桩的 WD2
/// 回归面）。
#[allow(dead_code)] // WD1a 骨架：WD2 窗口流的读超时消费点
pub(crate) const WINDOW_TIMEOUT: Duration = Duration::from_secs(120);

/// WebDAV 薄客户端：单 reqwest Client（池内并发，D6）+ 基地址 +
/// 认证协商状态。
///
/// - 语义：动词面在（必要时协商的）会话上执行（WD2 接线）；
/// - 错误：对外统一 [`StorageError`]（reqwest 错误是类型化的
///   `is_connect`/`is_timeout`/`is_decode`——映射表住 client.rs 与
///   driver.rs，无字符串信标清单需求，计划 §4.1 注记）；
/// - 并发：全方法可并发调用（`&self`）；auth 状态经 tokio Mutex 单
///   一来源（nc 单调性的载体）；
/// - 生命周期：HTTP client 无连接态；nonce 会话的过期/重协商在 WD2
///   的 401 路径自理。
pub(crate) struct WebdavClient {
    /// 共享 HTTP 面（池内并发；connect 超时已烙进 builder）。
    #[allow(dead_code)] // WD1a 骨架：动词面 WD2 接线后进入使用
    http: reqwest::Client,
    /// 规范化基地址（尾斜杠形态；子路径即卷根）。
    #[allow(dead_code)] // WD1a 骨架：动词面 WD2 接线后进入使用
    base: Url,
    /// 认证协商状态（D1 状态机；nc 单调自守的单一来源）。
    #[allow(dead_code)] // WD1a 骨架：401 协商路径 WD2 接线后进入使用
    auth: tokio::sync::Mutex<AuthState>,
}

impl WebdavClient {
    /// 构造（纯本地：建 reqwest Client，不碰网络——惰性连接归
    /// reqwest 池）。
    ///
    /// `accept_invalid_certs = true` 时一次性 warn（D3 开洞必须可
    /// 观察——doctor 与启动日志共用该通道）。
    pub(crate) fn new(params: &WebdavParams) -> Result<Self, StorageError> {
        if params.accept_invalid_certs {
            tracing::warn!(
                target: "ck_webdav::client",
                url = %params.url,
                "webdav_accept_invalid_certs is enabled: TLS certificates are NOT verified for \
                 this volume (self-signed NAS escape hatch) — do not enable this on untrusted \
                 networks"
            );
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .danger_accept_invalid_certs(params.accept_invalid_certs)
            .build()
            .map_err(|error| {
                StorageError::Unavailable(format!("building the webdav http client: {error}"))
            })?;
        Ok(WebdavClient {
            http,
            base: params.url.clone(),
            auth: tokio::sync::Mutex::new(AuthState::None),
        })
    }

    // ---- 动词面（WD2 接线；每方法的重试白名单归属与怪癖锚见各 doc）---
}

// 动词面占位（WD1a 骨架）：WD2 手搓桩批接线后进入使用——重试白名单/
// 超时分层/206 校验/401 协商重放的作用点全在这里。
#[allow(dead_code)]
impl WebdavClient {
    /// PROPFIND（Depth `0`/`1`）→ multistatus 体文本（WD2 解析进
    /// [`crate::xml`]）。
    ///
    /// 重试白名单：幂等可重试（§4.5-10）。集合腿 URL 恒带尾斜杠
    /// （附录 C ⑧——`collection_url`）。
    pub(crate) async fn propfind(
        &self,
        _path: &str,
        _depth: Depth,
    ) -> Result<String, StorageError> {
        // TODO(wd2): 手搓桩批接线（auth 协商 + 重试白名单 + 207 解析）。
        Err(StorageError::Unsupported)
    }

    /// stat 语义的 PROPFIND Depth 0 → 单条目投影（404 → NotFound）。
    pub(crate) async fn stat(
        &self,
        _path: &str,
    ) -> Result<crate::xml::PropfindEntry, StorageError> {
        // TODO(wd2): 读路径批接线。
        Err(StorageError::Unsupported)
    }

    /// 窗口 GET（`Range: bytes=a-b` 半开映射到闭区间头）→ 窗口字节。
    ///
    /// 206 `Content-Range` 校验 + 200 全量截断回退（§4.5-6）+ 416 EOF
    /// 语义（§4.4）在 WD2 接线。永不自动重试路径之外：GET 幂等可重试。
    pub(crate) async fn get_range(
        &self,
        _path: &str,
        _start: u64,
        _end_inclusive: u64,
    ) -> Result<bytes::Bytes, StorageError> {
        // TODO(wd2): 窗口流批接线。
        Err(StorageError::Unsupported)
    }

    /// PUT（带 Content-Length 的整体上传——D5；`.part` 暂存件腿）。
    /// **永不自动重试**（§4.5-10）。
    pub(crate) async fn put(&self, _path: &str, _body: bytes::Bytes) -> Result<(), StorageError> {
        // TODO(wd2): 写路径批接线（WD3 stager 消费）。
        Err(StorageError::Unsupported)
    }

    /// MKCOL（恒带尾斜杠——附录 C ⑥/⑧；rclone 已存在 201 幂等陷阱由
    /// 驱动 stat 预检吸收，WD3）。**永不自动重试**。
    pub(crate) async fn mkcol(&self, _path: &str) -> Result<(), StorageError> {
        // TODO(wd2): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// DELETE（集合腿恒带尾斜杠——附录 C ⑦）。**永不自动重试**。
    pub(crate) async fn delete(&self, _path: &str) -> Result<(), StorageError> {
        // TODO(wd2): 读路径批接线。
        Err(StorageError::Unsupported)
    }

    /// MOVE（恒显式 `Overwrite` 头 + 绝对 `Destination`——附录 C ⑤/⑩）。
    /// **永不自动重试**。
    pub(crate) async fn move_(
        &self,
        _from: &str,
        _to: &str,
        _overwrite: bool,
    ) -> Result<(), StorageError> {
        // TODO(wd2): 写路径批接线。
        Err(StorageError::Unsupported)
    }

    /// PROPPATCH（D2 降级后 generic 不写 mtime——仅作动词面占位保留，
    /// 驱动面无调用方；WD3 复核后若无消费面则随批移除）。**永不自动
    /// 重试**。
    pub(crate) async fn proppatch(&self, _path: &str) -> Result<(), StorageError> {
        // TODO(wd3): conformance 批复核去留。
        Err(StorageError::Unsupported)
    }

    /// OPTIONS（doctor 探活 + Class 1 能力发现；幂等可重试）。
    pub(crate) async fn options(&self) -> Result<(), StorageError> {
        // TODO(wd2): doctor probe 接线（WD4）。
        Err(StorageError::Unsupported)
    }
}

/// PROPFIND 深度（`0` = stat 语义，`1` = 列目录语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // WD1a 骨架：变体在 WD2 动词面接线后构造
pub(crate) enum Depth {
    Zero,
    One,
}

impl Depth {
    /// 头值形态。
    #[allow(dead_code)] // WD1a 骨架：WD2 动词面接线后进入使用
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Depth::Zero => "0",
            Depth::One => "1",
        }
    }
}
