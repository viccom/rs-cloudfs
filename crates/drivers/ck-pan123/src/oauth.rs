//! 123 云盘 web 认证层（Phase 6 / 123-1）——QR 三端点 + 密码 sign_in
//! + 首存持久化。
//!
//! 语义契约（123-0 真机 + 123panNextGen 深读 K76；spike
//! `examples/pan123_spike` 验证形态）：
//!
//! - **QR 三端点**（`login.123pan.com`，恒用 web 头——`loginuuid` +
//!   `app-version: 3` + `platform: web`，无 Bearer）：
//!   `GET /api/user/qr-code/generate` 回 `data.uniID`（双拼 `uniId`）
//!   + `data.url`（编码进二维码的内容——123-4 setup 的扫码引导面）；
//!     `GET /api/user/qr-code/result?uniID=` 轮询——**`code==200` 即确认
//!     态直返 token**（§5.10 唯一非 0 成功码的第二现场），`code==0` 下
//!     按 `data.loginStatus` 0–4 状态机：0 等 / 1 已扫待确认 / 2 拒 /
//!     4 过期；`POST /api/user/qr-code/wx_code`（`{"uniID"}`）回
//!     `data.wxCode`（无人扫码时空串——真机实证）；
//! - **sign_in**（`POST {api_base}/b/api/user/sign_in`，JSON
//!   `{"type":1,"passport","password"}`——明文密码，TLS 面内）成功码
//!   **200**、token 在 `data.token`（90 天；`refresh_token_expire_time`
//!   字段存在但无 refresh 端点佐证——K76.4 无 refresh 裁决）；
//! - **首存持久化**：认证产出 token 即回调 [`TokenStore::save_token`]
//!   （ck-pan115 的「刷新即持久化」在无 refresh 协议下的对应形态——
//!   只在首获时存一次；K13 ConfigTokenStore 桥接在 123-4 装配）。
//!
//! R3：sign_in 载荷含明文密码——错误文本绝不回显请求体（`read_envelope`
//! 的非 JSON 路径不回显 body；`map_rejection` 只拼 code 与后端消息）。

use serde_json::{json, Value};

use cloudkit_storage::StorageError;

use crate::api::read_envelope;

/// 登录 token 持久化回调（K13 形态，ck-pan115 `TokenStore` 的单 token
/// 版本——web API 无 refresh，刷新链不存在，只有首存）。
///
/// 由组合根（123-4 装配批的 ConfigTokenStore 桥接）实现并注入认证
/// 函数：扫码确认/密码登录成功即回调，把 token 回写卷配置键
/// `pan123_token`。驱动内不依赖任何 core 类型（R1）——回调是驱动
/// crate 自有契约。
///
/// 生命周期契约：回调在认证函数的成功路径上同步发生；实现方不得再
/// 回调驱动（防重入），且应自行处理持久化失败（不得静默丢 token）。
pub trait TokenStore: Send + Sync {
    /// 持久化登录 token。
    fn save_token(&self, token: &str);
}

/// `generate` 产物：uniID（轮询键）+ url（二维码内容）。
#[derive(Debug, Clone)]
pub struct QrSession {
    pub uni_id: String,
    pub url: String,
}

/// `result` 一次轮询的归一（loginStatus 0–4 状态机 + code==200 确认态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrPoll {
    /// 0：等扫码——继续轮询。
    Waiting,
    /// 1：已扫码，待手机确认。
    Scanned,
    /// 2：用户拒绝。
    Refused,
    /// 确认（code==200 直返 token；loginStatus 3 语义）。
    Confirmed { token: String },
    /// 4：二维码过期——重走 generate。
    Expired,
}

/// 请求超时：30s（QR 三端点是轻查询；spike 同量级）。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ---------------------------------------------------------------------------
// wire 请求（单次；错误归一 StorageError——R3：错误文本绝不携带凭据值）
// ---------------------------------------------------------------------------

/// `GET {login_base}/api/user/qr-code/generate` → uniID + url。
pub async fn qr_generate(
    http: &reqwest::Client,
    login_base: &str,
) -> Result<QrSession, StorageError> {
    let resp = http
        .get(format!("{login_base}/api/user/qr-code/generate"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("qr-code/generate transport: {}", e.without_url()))
        })?;
    let stage = "qr-code/generate";
    let (_http, env) = read_envelope(resp, stage).await?;
    let data = env.ok(stage)?;
    let uni_id = v_str(&data, &["uniID", "uniId"])
        .filter(|u| !u.is_empty())
        .ok_or_else(|| StorageError::Unavailable(format!("{stage}: data.uniID missing")))?;
    let url = v_str(&data, &["url"]).unwrap_or_default();
    Ok(QrSession { uni_id, url })
}

/// 一次 `GET {login_base}/api/user/qr-code/result?uniID=` 轮询。
///
/// 传输失败上抛 `Unavailable`（调用方的轮询策略决定重试）；
/// `code==200` → [`QrPoll::Confirmed`]（token 缺失按协议异常上抛）；
/// `code==0` → loginStatus 状态机（3 若出现且带 token 同样归
/// Confirmed——防御臂；未知值 `Unavailable`）。
pub async fn qr_poll(
    http: &reqwest::Client,
    login_base: &str,
    uni_id: &str,
) -> Result<QrPoll, StorageError> {
    let resp = http
        .get(format!("{login_base}/api/user/qr-code/result"))
        .timeout(REQUEST_TIMEOUT)
        .query(&[("uniID", uni_id)])
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("qr-code/result transport: {}", e.without_url()))
        })?;
    let stage = "qr-code/result";
    let (_http, env) = read_envelope(resp, stage).await?;
    // 确认态：code==200 直返 token（真机协议形态——§5.10 唯一非 0
    // 成功码的第二现场）；code==0 但 data 已带 token 同样收敛到确认。
    if env.code == 200 || v_str(&env.data, &["token"]).is_some() {
        return Ok(QrPoll::Confirmed {
            token: parse_token(&env.data, stage)?,
        });
    }
    if env.is_ok() {
        let status = v_i64(&env.data, &["loginStatus"]);
        return match status {
            Some(0) | None => Ok(QrPoll::Waiting), // 缺 loginStatus = 等待包
            Some(1) => Ok(QrPoll::Scanned),
            Some(2) => Ok(QrPoll::Refused),
            Some(3) => Ok(QrPoll::Confirmed {
                token: parse_token(&env.data, stage)?,
            }),
            Some(4) => Ok(QrPoll::Expired),
            Some(other) => Err(StorageError::Unavailable(format!(
                "{stage}: unknown loginStatus {other}"
            ))),
        };
    }
    Err(env.to_storage_error(stage))
}

/// `POST {login_base}/api/user/qr-code/wx_code`（`{"uniID"}`）→
/// `data.wxCode`（空串 → `None`——无人扫码的真机实证形态）。
pub async fn qr_wx_code(
    http: &reqwest::Client,
    login_base: &str,
    uni_id: &str,
) -> Result<Option<String>, StorageError> {
    let resp = http
        .post(format!("{login_base}/api/user/qr-code/wx_code"))
        .timeout(REQUEST_TIMEOUT)
        .json(&json!({"uniID": uni_id}))
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("qr-code/wx_code transport: {}", e.without_url()))
        })?;
    let stage = "qr-code/wx_code";
    let (_http, env) = read_envelope(resp, stage).await?;
    let data = env.ok(stage)?;
    Ok(v_str(&data, &["wxCode", "wx_code"]).filter(|c| !c.is_empty()))
}

/// `POST {api_base}/b/api/user/sign_in` `{"type":1,"passport","password"}`
/// → token（90 天）。
///
/// 成功即首存（`store.save_token`）再返回。失败（错密码等）→
/// `map_rejection` 终态（`Unavailable` 保留原码与后端消息）。
pub async fn sign_in(
    http: &reqwest::Client,
    api_base: &str,
    passport: &str,
    password: &str,
    store: Option<&dyn TokenStore>,
) -> Result<String, StorageError> {
    let resp = http
        .post(format!("{api_base}/b/api/user/sign_in"))
        .timeout(REQUEST_TIMEOUT)
        .json(&json!({
            "type": 1,
            "passport": passport,
            "password": password,
        }))
        .send()
        .await
        .map_err(|e| {
            StorageError::Unavailable(format!("sign_in transport: {}", e.without_url()))
        })?;
    let stage = "sign_in";
    let (_http, env) = read_envelope(resp, stage).await?;
    let data = env.auth_ok(stage)?;
    let token = parse_token(&data, stage)?;
    if let Some(store) = store {
        store.save_token(&token);
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// 解析辅助（信封 data 的 tolerant 提取）
// ---------------------------------------------------------------------------

/// data 的字符串字段（双拼键任一命中；数字形态转字符串）。
fn v_str(data: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        match data.get(*key) {
            Some(Value::String(s)) => return Some(s.clone()),
            Some(Value::Number(n)) => return Some(n.to_string()),
            _ => {}
        }
    }
    None
}

/// data 的整数字段（双拼键任一命中）。
fn v_i64(data: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        match data.get(*key) {
            Some(Value::Number(n)) => return n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Some(Value::String(s)) if s.parse::<i64>().is_ok() => return s.parse::<i64>().ok(),
            _ => {}
        }
    }
    None
}

/// 成功 data → token（缺失按协议异常 `Unavailable` 上抛且不回显 data
/// 内容——半截响应可能携带一半凭据，ck-pan115 `parse_token_pair`
/// 同款裁决）。
fn parse_token(data: &Value, stage: &'static str) -> Result<String, StorageError> {
    v_str(data, &["token", "Token"])
        .filter(|t| !t.is_empty())
        .ok_or_else(|| StorageError::Unavailable(format!("{stage}: data.token missing")))
}
