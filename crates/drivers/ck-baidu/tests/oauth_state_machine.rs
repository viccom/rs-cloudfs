//! OAuth 状态机三态 + 持久化回调（K13；Batch B1 红①）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照：spike api.rs:22-81 实抓 +
//! PCFS client.go:196-203 交叉，分歧以 spike 为准）：
//!
//! - **errno 110**：驱动内刷新（oauth 一次一换）+ 原请求重放一次；重放
//!   成功 → 操作成功，刷新产物（新 access+refresh 对）经 TokenStore
//!   **即刻**持久化——refresh_token 一次一换、旧值即刻作废（spike §1
//!   实证），即便随后的重放失败也不回收；
//! - **errno 110 ×2**（刷新成功但重放仍 110）→ `Unauthorized { true }`；
//! - **errno 111 / -6** → `Unauthorized { false }` 且**零刷新调用**——
//!   上层给重新授权指引，绝不死循环（§7a）。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::{Page, RelPath, StorageDriver, StorageError};

use common::{
    assert_exact_pairs, filter_recorded, parse_urlencoded, MockBaidu, RecordingTokenStore,
    INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN, MOCK_APP_KEY, MOCK_APP_SECRET, MOCK_ROOT,
    OAUTH_TOKEN, XPAN_FILE,
};

async fn setup() -> (MockBaidu, Arc<BaiduDriver>, Arc<RecordingTokenStore>) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    mock.seed_file(&format!("{MOCK_ROOT}/f.txt"), 10, 1757311000);
    let store = RecordingTokenStore::new();
    let params = mock.params(Some(store.clone()));
    let driver = factory(&params)
        .await
        .expect("baidu driver connect（uinfo 取 uid）");
    (mock, driver, store)
}

#[tokio::test]
async fn errno_110_refreshes_once_replays_with_new_token_and_persists() {
    let (mock, driver, store) = setup().await;
    // 使当前 access_token 失效（模拟过期）：下一次业务请求即 110。
    mock.set_access_token("mock-access-rotated-away");

    let listing = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 经「刷新+重放一次」后应成功");
    assert_eq!(listing.entries.len(), 1, "重放成功应返回种子条目");
    assert_eq!(listing.entries[0].path.as_str(), "f.txt");

    // 刷新恰好一次；refresh_token 一次一换（新对与初值均不同）。
    assert_eq!(mock.refresh_count(), 1, "110 → 恰一次 oauth 刷新");
    let (new_access, new_refresh) = mock.current_tokens();
    assert_ne!(new_access, INITIAL_ACCESS_TOKEN);
    assert_ne!(
        new_refresh, INITIAL_REFRESH_TOKEN,
        "refresh_token 一次一换（spike §1）"
    );

    // 持久化回调恰一次，落的是刷新产物（成对，新 access+新 refresh）。
    let calls = store.calls();
    assert_eq!(
        calls.len(),
        1,
        "刷新响应到达即持久化（K13）——恰一次 save_tokens"
    );
    assert_eq!(calls[0].0, new_access, "持久化新 access_token");
    assert_eq!(
        calls[0].1, new_refresh,
        "持久化新 refresh_token（一次一换后的唯一活值）"
    );

    // 业务端恰两次调用：首带旧 token（得 110），重放带新 token。
    let recorded = mock.recorded();
    let list_reqs = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=list"]);
    assert_eq!(list_reqs.len(), 2, "110 → 原请求恰重放一次（不循环）");
    let q0 = parse_urlencoded(&list_reqs[0].query);
    let q1 = parse_urlencoded(&list_reqs[1].query);
    assert!(
        q0.iter()
            .any(|(k, v)| k == "access_token" && v == INITIAL_ACCESS_TOKEN),
        "首次请求带旧 token：{q0:?}"
    );
    assert!(
        q1.iter()
            .any(|(k, v)| k == "access_token" && v == &new_access),
        "重放请求带刷新后的新 token：{q1:?}"
    );

    // oauth 刷新调用 wire 形态（spike api.rs:33-40）：恰四参数。
    let oauth_reqs = filter_recorded(&recorded, "GET", OAUTH_TOKEN, &[]);
    assert_eq!(oauth_reqs.len(), 1, "oauth 端点恰被调用一次");
    assert_exact_pairs(
        &parse_urlencoded(&oauth_reqs[0].query),
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", INITIAL_REFRESH_TOKEN),
            ("client_id", MOCK_APP_KEY),
            ("client_secret", MOCK_APP_SECRET),
        ],
    );
}

#[tokio::test]
async fn errno_110_twice_maps_to_unauthorized_recoverable_true() {
    let (mock, driver, store) = setup().await;
    // 双注入：原始请求 110 → 刷新成功 → 重放仍 110 → 放弃。
    mock.inject_errno(110);
    mock.inject_errno(110);

    let err = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect_err("刷新后重放仍 110 必须报错（不能吞成成功）");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: true },
        "重放仍败 → recoverable:true（K13：再刷一次通常可行）"
    );

    assert_eq!(mock.refresh_count(), 1, "只刷新一次，绝不循环");
    assert_eq!(
        filter_recorded(&mock.recorded(), "GET", XPAN_FILE, &["method=list"]).len(),
        2,
        "业务请求恰两次（原始 + 重放）"
    );
    // 刷新虽已成功，产物仍即刻持久化——重放失败不回收（新 refresh_token
    // 已是唯一活值，丢弃即凭据损失）。
    assert_eq!(
        store.calls().len(),
        1,
        "刷新成功的产物必须已持久化（重放失败不回收，K13 on-arrival 语义）"
    );
}

#[tokio::test]
async fn errno_111_maps_to_unauthorized_recoverable_false_without_refresh() {
    let (mock, driver, _store) = setup().await;
    mock.inject_errno(111);

    let err = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect_err("111 必须报错");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: false },
        "refresh_token 过期 → 需人工重新授权（人话指引，勿重试循环）"
    );
    assert_eq!(mock.refresh_count(), 0, "111 零刷新（绝不死循环）");
    assert!(
        filter_recorded(&mock.recorded(), "GET", OAUTH_TOKEN, &[]).is_empty(),
        "oauth 端点未被调用"
    );
}

#[tokio::test]
async fn errno_minus6_maps_to_unauthorized_recoverable_false_without_refresh() {
    let (mock, driver, _store) = setup().await;
    mock.inject_errno(-6);

    let err = driver
        .list(&RelPath::root(), Page::all())
        .await
        .expect_err("-6 必须报错");
    assert_eq!(
        err,
        StorageError::Unauthorized { recoverable: false },
        "鉴权失败（token/appkey 不匹配类）→ 不可自动恢复"
    );
    assert_eq!(mock.refresh_count(), 0, "-6 零刷新");
    assert!(
        filter_recorded(&mock.recorded(), "GET", OAUTH_TOKEN, &[]).is_empty(),
        "oauth 端点未被调用"
    );
}
