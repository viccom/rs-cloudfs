//! M-I2 回归钉（2026-09-17 深度审查）：`not(baidu)+pan115` 裁剪组合下，
//! 多卷 dispatch 不得把非 pan115 卷误装配进 pan115 通道——该组合曾对
//! 任何 backend 无条件走 `build_pan115_transport_with`，local/sftp/baidu
//! 卷报误导性的 pan115 错误而非各自的 K31 可行动文案。
//!
//! 只在同名 feature 组合下编译/运行（其余组合编译为空）：
//!
//! ```text
//! cargo test -p cloudkit-cli --no-default-features --features pan115 \
//!   --test pan115_combo_dispatch
//! ```

#![cfg(all(not(feature = "baidu"), feature = "pan115"))]

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
/// 而不是被 pan115 装配链吃掉后吐出 token/Invalid 类误导错误。
#[tokio::test]
async fn non_pan115_backends_refuse_with_their_own_driver_message() {
    let settings = CyDriveConfig {
        backend: Backend::Baidu,
        ..CyDriveConfig::default()
    };
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
        !msg.contains("pan115"),
        "misrouted into the pan115 assembly: {msg}"
    );
    assert!(
        msg.contains("baidu"),
        "the refusal must name the baidu driver (K31 actionable text): {msg}"
    );
}
