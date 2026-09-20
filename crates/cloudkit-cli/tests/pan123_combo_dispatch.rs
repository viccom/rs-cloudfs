//! M-I2 回归钉的 pan123 面（Phase 6 / 123-4）：`not(baidu)+pan123` 裁剪
//! 组合下，多卷 dispatch 不得把非 pan123 卷误装配进 pan123 通道（K73
//! 形态——该缺陷类曾发生在 `not(baidu)+pan115` 组合），且 pan123 卷
//! 必须真正路由进 pan123 装配链（无 token → 驱动构造的 Invalid，而非
//! K31 拒绝——证明接线而非占位）。
//!
//! 只在同名 feature 组合下编译/运行（其余组合编译为空）：
//!
//! ```text
//! cargo test -p cloudkit-cli --no-default-features --features pan123 \
//!   --test pan123_combo_dispatch
//! ```

#![cfg(all(not(feature = "baidu"), feature = "pan123"))]

use cloudkit_core::config::{Backend, CyDriveConfig, VolumeConfig};

fn combo_spec() -> VolumeConfig {
    VolumeConfig {
        name: "combo-vol".to_string(),
        file_path: std::path::PathBuf::from("volumes/combo-vol.toml"),
        base_dir: std::path::PathBuf::from("volumes"),
        settings: CyDriveConfig::default(),
        explicit_drive_letter: false,
    }
}

/// baidu 卷在本组合必须拿到 BAIDU_DRIVER_REQUIRED（K31 可行动文案），
/// 而不是被 pan123 装配链吃掉后吐出误导错误。
#[tokio::test]
async fn non_pan123_backends_refuse_with_their_own_driver_message() {
    let mut settings = CyDriveConfig::default();
    settings.backend = Backend::Baidu;
    let spec = combo_spec();
    let mut run_options = cloudkit_cli::RunOptions::default();
    let home = std::path::Path::new(".");

    let msg = match cloudkit_cli::dispatch_unified_backend_volume(
        &spec,
        &settings,
        home,
        &mut run_options,
    )
    .await
    {
        Err(err) => err.to_string(),
        Ok(_) => panic!("a baidu volume must refuse in a build without the baidu driver"),
    };
    assert!(
        !msg.contains("pan123"),
        "misrouted into the pan123 assembly: {msg}"
    );
    assert!(
        msg.contains("baidu"),
        "the refusal must name the baidu driver (K31 actionable text): {msg}"
    );
}

/// pan123 卷在本组合必须真正进入 pan123 装配链：无 token 的配置走到
/// **驱动构造**的 Invalid（`connecting the pan123 backend`），证明 dispatch
/// 臂已从 123-1 占位转为真装配（占位形态会报「not wired yet」）。
/// 不拨号——构造在 `user/info` 连接之前失败。
#[tokio::test]
async fn a_pan123_volume_routes_into_the_real_assembly() {
    let mut settings = CyDriveConfig::default();
    settings.backend = Backend::Pan123; // 无 token（validate 面由 core 钉）
    let spec = combo_spec();
    let mut run_options = cloudkit_cli::RunOptions::default();
    let home = std::path::Path::new(".");

    let msg = match cloudkit_cli::dispatch_unified_backend_volume(
        &spec,
        &settings,
        home,
        &mut run_options,
    )
    .await
    {
        Err(err) => err.to_string(),
        Ok(_) => panic!("a tokenless pan123 volume must refuse (the driver constructor)"),
    };
    assert!(
        msg.contains("pan123"),
        "the refusal must come from the pan123 assembly itself: {msg}"
    );
    assert!(
        !msg.contains("not wired"),
        "the 123-1 placeholder refusal is gone (123-4 wired the dispatch): {msg}"
    );
}
