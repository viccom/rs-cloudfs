//! WD4 twin 组合测试（pan115_combo_dispatch.rs 模式——M-I1/M-I2 教训的
//! webdav 等价面）：`webdav` 裁剪组合下，
//!
//! 1. 多卷 dispatch 不误装配——非 webdav 卷各拿各的 K31 可行动文案，
//!    绝不被 webdav 装配链吃掉（M-I2）；webdav 卷在同一组合内走到
//!    **装配 connect 门**（2026-09-25 复审推广：门错误即「进了 webdav
//!    臂」的组合证明——K31 文案已由 dispatch.rs 的 WD1b 腿钉过）；
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

/// 组合中的 webdav 臂（复审后形态）：装配期 connect 门生效——死端口
/// （127.0.0.1:1）确定性拒连，dispatch 递到 webdav 臂的**门错误**（而
/// 非他驱动的 K31 文案——组合不误装配仍然成立）；门失败后
/// run_options 不被填充。
#[tokio::test]
async fn webdav_volume_hits_the_connect_gate_in_the_trimmed_combo() {
    let settings = webdav_settings();
    let spec = combo_spec(settings.clone());
    let mut run_options = RunOptions::default();
    let err = cloudkit_cli::dispatch_unified_backend_volume(
        &spec,
        &settings,
        std::path::Path::new("."),
        &mut run_options,
    )
    .await
    .err()
    .expect("the dead endpoint must fail the webdav assembly");
    let msg = err.to_string();
    assert!(
        msg.contains("the webdav volume is not usable"),
        "the webdav arm ran to its connect gate (not another arm's refusal): {msg}"
    );
    assert_eq!(
        run_options.sync_namespace, None,
        "the gate failure precedes the run_options fill"
    );
}

// ------------------------------------------- env > file（M-I1 等价面）---

/// env > file（M-I1 等价面）：文件半边 lone username 由 env 补齐——
/// 复审后装配带 connect 门，但**凭据解析先于网络**：env 在场 → 装配
/// 走到门（死端口的网络拒），env 摘除 → pair 解析就拒（点名两键）。
/// 两种错误的**形态差**就是「env 值真到达驱动装配」的证明。
#[tokio::test]
async fn env_password_completes_the_credential_pair_at_assembly() {
    let _guard = lock_password_env();

    std::env::set_var("CYDRIVE_WEBDAV_PASSWORD", "env-pw");
    let mut settings = webdav_settings();
    settings.webdav_password = None; // 文件半边失效；username 还在
    let err = build_backend_transport(&settings.clone().with_env_overrides())
        .await
        .err()
        .expect("the dead endpoint fails the gated assembly — but only PAST the pair parse");
    let gated = err.to_string();
    assert!(
        gated.contains("the webdav volume is not usable"),
        "with env the pair parses and the assembly reaches the connect gate: {gated}"
    );

    // 逆命题：env 摘除（set-but-empty 清空语义）→ lone username 拒在
    // 解析层（网络都不碰）并点名两把键（K31 可行动文案）。
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

// ------------------------- run_sync_command namespace 臂（复审后形态）---

/// `run_sync_command` 的 Webdav namespace 臂经 build_backend_transport
/// 取传输（原「纯离线推导 D6」钉随装配 connect 门推翻——2026-09-25）：
/// sync 命令本就需要后端，死共享（与死 sync 端点）下失败**提前到门**、
/// 以可行动文案浮现（"the webdav volume is not usable"），不再深入
/// sync pass 后以逐操作错误困惑。
#[tokio::test]
async fn sync_command_surfaces_the_connect_gate_for_a_dead_share() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut settings = webdav_settings();
    settings.sync_url = Some("http://127.0.0.1:1/".to_string());
    settings.db_path = dir.path().join("meta.db").display().to_string();
    settings.cache_path = dir.path().join("cache").display().to_string();

    let err = run_sync_command(&settings, None)
        .await
        .expect_err("the dead share must fail the sync command");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("the webdav volume is not usable"),
        "the gated assembly surfaces the actionable refusal: {msg}"
    );
    assert!(
        msg.contains("derive the sync namespace"),
        "the failure is attributed to the namespace/assembly step: {msg}"
    );
}
