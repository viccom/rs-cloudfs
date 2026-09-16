//! 错误码映射契约测试（Phase 5 / 115-1；K69.7 真机采样表逐条钉死）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 115-0 spike 真机采样
//! `examples/pan115_spike/src/{auth,api}.rs` + decisions K69.7）：
//!
//! - **envelope 双形态**：proapi 成功/错误携带**布尔** `state`
//!   （`true`/`false`），passportapi 携带**数字**（`1`/`0`）——`is_ok`
//!   必须两者都认（`state==1 || state==true` 且 `errno==0`）；
//! - **HTTP 200 错误包**：错误响应恒 HTTP 200 + envelope 承载错误
//!   （错误码在 `code` 与/或 `errno`）——HTTP 状态码不可作判据；
//! - **分类表**（classify，按 code 或 errno 任一命中）：
//!   `40199002`（QR 过期）/`40101017`（未确认换 token）/
//!   `40140123`（access_token 格式错）/整个 `401*` 段与 `99` =
//!   token 过期；`770004` = 账号级访问上限；`911` = 需人工验证；
//!   `430004` = 文件不存在；`20130827` = 限流（桌面版代码形态，
//!   未真机复现，一并入表）；
//! - **终态映射**（map_rejection，dispatch 自救之后的 StorageError
//!   归一）：token 过期 → `Unauthorized{recoverable:true}`（client
//!   已刷+重放过一次的兜底）；911 → `Unauthorized{recoverable:false}`
//!   （人工验证 = 重新走授权流的出路，绝不重试）；770004/20130827 →
//!   `RateLimited`；430004 → `NotFound`；未知码 → `Unavailable` 且
//!   载荷保留原始码与消息（R2 可诊断约定，baidu map_errno 同款）。

use ck_pan115::api::{classify, map_rejection, Envelope, ErrKind};
use cloudkit_storage::StorageError;

// ------------------------------------------------------ envelope 双形态 ---

#[test]
fn envelope_accepts_both_the_boolean_and_the_numeric_state_forms() {
    // proapi 成功：state 布尔 true
    let proapi_ok: Envelope =
        serde_json::from_str(r#"{"state":true,"data":{"user_id":42}}"#).expect("proapi ok");
    assert!(proapi_ok.is_ok(), "boolean true + errno absent (default 0)");
    assert_eq!(
        proapi_ok.data.get("user_id").and_then(|v| v.as_i64()),
        Some(42),
        "data must survive parsing"
    );

    // passportapi 成功：state 数字 1
    let passport_ok: Envelope =
        serde_json::from_str(r#"{"state":1,"errno":0,"data":{"access_token":"x"}}"#)
            .expect("passport ok");
    assert!(passport_ok.is_ok(), "numeric 1 is success");

    // 两种错误形态：state:false / state:0
    let proapi_err: Envelope =
        serde_json::from_str(r#"{"state":false,"code":40140123,"message":"token bad"}"#)
            .expect("proapi err");
    assert!(!proapi_err.is_ok());
    assert_eq!(proapi_err.code, 40140123);
    let passport_err: Envelope =
        serde_json::from_str(r#"{"state":0,"errno":770004,"message":"cap"}"#)
            .expect("passport err");
    assert!(!passport_err.is_ok());
    assert_eq!(passport_err.errno, 770004);

    // state 数字 2 / 其他值 = 不是成功（未知形态按错误处理）
    let odd: Envelope = serde_json::from_str(r#"{"state":2}"#).expect("odd state");
    assert!(!odd.is_ok());

    // errno != 0 即便 state 成功形态也非成功（防御：errno 是权威位）
    let errno_set: Envelope =
        serde_json::from_str(r#"{"state":true,"errno":911}"#).expect("errno set");
    assert!(!errno_set.is_ok());
}

#[test]
fn envelope_carries_the_files_face_count_sibling() {
    // ufile/files 的顶层兄弟字段 count（总行数）——分页终止规则的依据。
    let env: Envelope =
        serde_json::from_str(r#"{"state":true,"count":42,"data":[{"fid":"1"},{"fid":"2"}]}"#)
            .expect("files page");
    assert!(env.is_ok());
    assert_eq!(env.count, 42, "the count sibling must parse");
}

#[test]
fn http_200_is_not_a_success_signal_by_itself() {
    // K69.7：错误包恒 HTTP 200——判据只能是 envelope。用一个真机采样
    // 形态的错误包（HTTP 200 携带）验证 envelope 侧给出错误分类所需的
    // 全部字段。
    let body = r#"{"state":false,"code":40140123,"message":"invalid access token"}"#;
    let env: Envelope = serde_json::from_str(body).expect("parse the 200 error body");
    assert!(!env.is_ok());
    assert_eq!(classify(env.code, env.errno), ErrKind::TokenExpired);
}

// ---------------------------------------------------------- 分类表逐条 ---

#[test]
fn classify_covers_every_sampled_code() {
    // token 过期族：401* 段（code 或 errno 任一位）+ 99
    for code in [40199002i64, 40101017, 40140123, 40100000, 40199999, 99] {
        assert_eq!(
            classify(code, 0),
            ErrKind::TokenExpired,
            "code {code} must classify as token-expired"
        );
        assert_eq!(
            classify(0, code),
            ErrKind::TokenExpired,
            "errno {code} must classify as token-expired"
        );
    }

    // 账号级访问上限（K69.3：跨端点族整账号封锁）
    assert_eq!(classify(770004, 0), ErrKind::AccountRateLimited);
    assert_eq!(classify(0, 770004), ErrKind::AccountRateLimited);

    // 人工验证（未真机复现，桌面版形态入表）
    assert_eq!(classify(911, 0), ErrKind::HumanVerify);
    assert_eq!(classify(0, 911), ErrKind::HumanVerify);

    // 文件不存在（SDK 侧）
    assert_eq!(classify(430004, 0), ErrKind::NotFound);
    assert_eq!(classify(0, 430004), ErrKind::NotFound);

    // 限流（桌面版形态 20130827，K69.3 注记一并入表）
    assert_eq!(classify(20130827, 0), ErrKind::RateLimited);
    assert_eq!(classify(0, 20130827), ErrKind::RateLimited);

    // 未知码 → Rejected（终态映射给 Unavailable 保留原码）
    assert_eq!(classify(47002, 0), ErrKind::Rejected);
    assert_eq!(classify(0, 0), ErrKind::Rejected);
}

// -------------------------------------------------------- 终态映射表 ---

#[test]
fn map_rejection_routes_each_kind_to_its_storage_error() {
    // token 过期：dispatch 已刷+重放过一次后的兜底 → recoverable:true
    assert_eq!(
        map_rejection(40140123, 0, "whatever"),
        StorageError::Unauthorized { recoverable: true }
    );

    // 人工验证：不重试、可行动上抛（重新走授权/完成人工验证）
    assert_eq!(
        map_rejection(911, 0, "verify required"),
        StorageError::Unauthorized { recoverable: false }
    );

    // 账号级访问上限（纯终态：dispatch 拦截路径会带 retry_after=封锁窗）
    assert_eq!(
        map_rejection(770004, 0, "cap reached"),
        StorageError::RateLimited { retry_after: None }
    );

    // 桌面版限流码同形态
    assert_eq!(
        map_rejection(0, 20130827, "slow down"),
        StorageError::RateLimited { retry_after: None }
    );

    // 文件不存在
    assert_eq!(
        map_rejection(430004, 0, "no such file"),
        StorageError::NotFound
    );

    // 未知码：Unavailable 载荷保留原始码与后端消息（R2）
    match map_rejection(47002, 0, "odd backend reply") {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("47002"), "keeps the raw code: {detail}");
            assert!(
                detail.contains("odd backend reply"),
                "keeps the backend message: {detail}"
            );
        }
        other => panic!("unknown code must map to Unavailable, got {other:?}"),
    }
}

#[test]
fn map_rejection_payloads_never_echo_the_data_member() {
    // R3 防御钉：错误消息只拼 state/code/errno/message——token 端点成功
    // 响应的 data 内含新凭据，错误文本绝不能带 data（spike auth.rs
    // Envelope::ok 的同款裁决）。
    let err = map_rejection(47002, 0, "boom");
    match err {
        StorageError::Unavailable(detail) => {
            assert!(
                !detail.contains("access_token") && !detail.contains("refresh_token"),
                "no token field names in payloads: {detail}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
