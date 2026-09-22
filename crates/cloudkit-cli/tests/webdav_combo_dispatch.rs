//! WD4 twin 组合测试（pan115_combo_dispatch.rs 模式——M-I1/M-I2 教训的
//! webdav 等价面）：`webdav` 裁剪组合下，
//!
//! 1. 多卷 dispatch 不误装配——非 webdav 卷各拿各的 K31 可行动文案，
//!    绝不被 webdav 装配链吃掉（M-I2）；webdav 卷在同一组合内正常
//!    装配（K31 文案已由 dispatch.rs 的 WD1b 腿钉过——本文件补的是
//!    组合中的**成功路径**）；
//! 2. env > file 优先序——`CYDRIVE_WEBDAV_PASSWORD` 经
//!    `with_env_overrides` 在装配期补齐凭据对（webdav 无 token 刷新
//!    态，env 覆盖链就是 M-I1 的等价面：env 值必须真到达驱动装配）；
//! 3. `run_sync_command` 的 Webdav namespace 臂离线推导（D6）——
//!    `webdav:<user>@<base>` 稳定形态，服务器不可达只该撞 sync 面。
//!
//! 只在含 `webdav` 且不含 `baidu` 的组合下编译/运行（其余组合编译为
//! 空）；每个用例内部的 per-backend 断言再按各自 feature 细分门控：
//!
//! ```text
//! cargo test -p cloudkit-cli --no-default-features --features webdav \
//!   --test webdav_combo_dispatch
//! ```

#![cfg(all(not(feature = "baidu"), feature = "webdav"))]

use std::sync::{Mutex, MutexGuard};

use cloudkit_cli::{build_backend_transport, run_sync_command, RunOptions};
use cloudkit_core::config::{Backend, CyDriveConfig, VolumeConfig};

// ------------------------------------------------------------- helpers ---

/// 本组合内确定性可达下限的 webdav 卷配置：`127.0.0.1:1` 无监听——
/// 装配离线（D6），任何网络腿都连接拒绝即刻发生（不赌 DNS 形态）。
fn webdav_settings() -> CyDriveConfig {
    CyDriveConfig {
        backend: Backend::Webdav,
        webdav_url: Some("http://127.0.0.1:1/dav".to_string()),
        webdav_username: Some("spike".to_string()),
        webdav_password: Some("pw".to_string()),
        ..CyDriveConfig::default()
    }
}

fn combo_spec(settings: CyDriveConfig) -> VolumeConfig {
    VolumeConfig {
        name: "combo-vol".to_string(),
        file_path: std::path::PathBuf::from("volumes/combo-vol.toml"),
        base_dir: std::path::PathBuf::from("volumes"),
        settings,
        explicit_drive_letter: false,
    }
}

/// 串行化每个触碰 `CYDRIVE_WEBDAV_PASSWORD` 的测试（同一测试二进制
/// 共享一个进程；env 变量会竞态）。守卫在 drop 时摘除变量——panic 路
/// 径也不会污染后续用例。
static WEBDAV_PASSWORD_ENV: Mutex<()> = Mutex::new(());

struct PasswordEnvGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl Drop for PasswordEnvGuard<'_> {
    fn drop(&mut self) {
        std::env::remove_var("CYDRIVE_WEBDAV_PASSWORD");
    }
}

fn lock_password_env() -> PasswordEnvGuard<'static> {
    let guard = WEBDAV_PASSWORD_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    PasswordEnvGuard { _guard: guard }
}

// ------------------------------------------------- M-I2：不误装配 ---

/// 非 webdav 后端在裁剪组合里各拿各的 K31 文案——装配失败必须点名
/// 缺的驱动，且绝不出现 webdav 装配链的痕迹（误装配信号）。
#[tokio::test]
async fn non_webdav_backends_refuse_with_their_own_driver_message() {
    async fn refusal_of(backend: Backend) -> String {
        let settings = CyDriveConfig {
            backend,
            ..CyDriveConfig::default()
        };
        let spec = combo_spec(settings.clone());
        let mut run_options = RunOptions::default();
        match cloudkit_cli::dispatch_unified_backend_volume(
            &spec,
            &settings,
            std::path::Path::new("."),
            &mut run_options,
        )
        .await
        {
            Err(err) => err.to_string(),
            Ok(_) => panic!("{backend:?} volume must refuse without its driver"),
        }
    }

    // baidu 恒缺（文件级门），其余按各自 feature 缺席才有此臂。
    let msg = refusal_of(Backend::Baidu).await;
    assert!(
        msg.contains("baidu") && !msg.contains("webdav"),
        "baidu refusal must be its own K31 message, not a webdav misroute: {msg}"
    );
    #[cfg(not(feature = "local"))]
    {
        let msg = refusal_of(Backend::Local).await;
        assert!(
            msg.contains("local") && !msg.contains("webdav"),
            "local refusal must be its own K31 message: {msg}"
        );
    }
    #[cfg(not(feature = "sftp"))]
    {
        let msg = refusal_of(Backend::Sftp).await;
        assert!(
            msg.contains("sftp") && !msg.contains("webdav"),
            "sftp refusal must be its own K31 message: {msg}"
        );
    }
    #[cfg(not(feature = "pan115"))]
    {
        let msg = refusal_of(Backend::Pan115).await;
        assert!(
            msg.contains("pan115") && !msg.contains("webdav"),
            "pan115 refusal must be its own K31 message: {msg}"
        );
    }
    #[cfg(not(feature = "pan123"))]
    {
        let msg = refusal_of(Backend::Pan123).await;
        assert!(
            msg.contains("pan123") && !msg.contains("webdav"),
            "pan123 refusal must be its own K31 message: {msg}"
        );
    }
}

/// 组合中的成功路径：webdav 卷照常装配（离线，D6）——卷身份与 sync
/// namespace 都是 `webdav:<user>@<base>` 稳定形态，`web_volume` 面同
/// 步填充。
#[tokio::test]
async fn webdav_volume_assembles_in_the_trimmed_combo() {
    let settings = webdav_settings();
    let spec = combo_spec(settings.clone());
    let mut run_options = RunOptions::default();
    let _dispatched = cloudkit_cli::dispatch_unified_backend_volume(
        &spec,
        &settings,
        std::path::Path::new("."),
        &mut run_options,
    )
    .await
    .expect("the webdav arm assembles offline in the trimmed combo");
    let identity = "webdav:spike@http://127.0.0.1:1/dav/";
    assert_eq!(run_options.sync_namespace.as_deref(), Some(identity));
    assert_eq!(run_options.web_volume.as_deref(), Some(identity));
}

// ------------------------------------------- env > file（M-I1 等价面）---

/// 文件半边的凭据缺失（lone username）由 env 补齐——`with_env_overrides`
/// 的值必须真到达驱动装配（装配成功即证明），而不是只停在 config 字段。
#[tokio::test]
async fn env_password_completes_the_credential_pair_at_assembly() {
    let _guard = lock_password_env();

    std::env::set_var("CYDRIVE_WEBDAV_PASSWORD", "env-pw");
    let mut settings = webdav_settings();
    settings.webdav_password = None; // 文件半边失效；username 还在
    let dispatched = build_backend_transport(&settings.clone().with_env_overrides())
        .await
        .expect("the env value completes the pair at assembly time");
    assert_eq!(
        dispatched.volume(),
        "webdav:spike@http://127.0.0.1:1/dav/",
        "the assembled identity carries the file's username + url"
    );

    // 逆命题：env 摘除（set-but-empty 清空语义）→ lone username 拒装配
    // 并点名两把键（K31 可行动文案）。
    std::env::set_var("CYDRIVE_WEBDAV_PASSWORD", "");
    let err = match build_backend_transport(&settings.with_env_overrides()).await {
        Ok(_) => panic!("a lone username must refuse the assembly"),
        Err(err) => err.to_string(),
    };
    assert!(
        err.contains("webdav_username") && err.contains("webdav_password"),
        "the refusal names both keys: {err}"
    );
}

// --------------------------------- run_sync_command namespace 臂（D6）---

/// `run_sync_command` 的 Webdav namespace 臂离线推导：服务器（与 sync
/// 端点都）不可达时，失败落在 sync pass 上，绝不在 webdav 装配/连接上
/// ——namespace `webdav:<user>@<base>` 是纯离线推导（成功路径的值已
/// 由上面的装配断言钉死）。
#[tokio::test]
async fn sync_command_derives_the_webdav_namespace_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut settings = webdav_settings();
    settings.sync_url = Some("http://127.0.0.1:1/".to_string());
    settings.db_path = dir.path().join("meta.db").display().to_string();
    settings.cache_path = dir.path().join("cache").display().to_string();

    let err = run_sync_command(&settings, None)
        .await
        .expect_err("the dead sync endpoint must fail the pass");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("sync pass against"),
        "the failure is the sync pass: {msg}"
    );
    assert!(
        !msg.contains("connecting the webdav backend"),
        "the namespace arm must derive offline (D6) — no webdav connect in the chain: {msg}"
    );
}
