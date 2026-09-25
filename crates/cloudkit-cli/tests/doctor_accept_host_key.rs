//! `cydrive doctor --accept-host-key` 的两个可测面（2026-09-25 批）：
//!
//! - **接受决策**（纯函数——D2 边界的可测面）：已钉恒拒（替换指纹必须
//!   out-of-band 核验后人工改文件——MITM 绊线）；未钉 + 探针带回服务器
//!   实际指纹 = 可接受；探针其他结论（不可达/认证错/已活）不是接受窗口。
//! - **指纹写回**：UPDATE 控制命令的**同一文件漏斗**（旧文件全漏斗先验
//!   → 单键 overlay → 合并全漏斗 parse+validate → 原子写）。

#![cfg(feature = "sftp")]

use std::path::PathBuf;

use ck_sftp::SftpProbe;
use cloudkit_cli::doctor::{host_key_accept_decision, HostKeyAccept};
use cloudkit_cli::write_sftp_host_fingerprint;

/// 一个合法的未钉指纹 sftp 卷文件（multivolume_ops 同款形态）。
fn sample_volume(dir: &tempfile::TempDir, name: &str, fingerprint: Option<&str>) -> PathBuf {
    let path = dir.path().join(format!("{name}.toml"));
    let pinned = fingerprint
        .map(|v| format!("sftp_host_fingerprint = \"{v}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        &path,
        format!(
            "backend = \"sftp\"\nsftp_host = \"nas.lan\"\nsftp_username = \"u\"\n\
             sftp_password = \"p\"\n{pinned}"
        ),
    )
    .expect("write sample volume");
    path
}

/// 已钉恒拒——即便探针带回别的指纹（D2：指纹变更绝不自动换钉）。
#[test]
fn pinned_volume_is_never_auto_reaccepted() {
    match host_key_accept_decision(
        Some("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        &SftpProbe::HostKeyUnpinned("SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into()),
    ) {
        HostKeyAccept::Refused { reason } => {
            assert!(
                reason.contains("out-of-band") && reason.contains("by hand"),
                "the refusal routes replacement through manual verification: {reason}"
            );
        }
        HostKeyAccept::Acceptable { .. } => {
            panic!("a pinned volume must never auto-reaccept a fingerprint")
        }
    }
}

/// 未钉 + 服务器实际指纹（首握手即捕获，先于认证）= 可接受。
#[test]
fn unpinned_volume_with_server_fingerprint_is_acceptable() {
    match host_key_accept_decision(
        None,
        &SftpProbe::HostKeyUnpinned("SHA256:mpapVT6FjOLWwDsXSv0/8imyC1ERXiUKwzHl2CfoIeQ".into()),
    ) {
        HostKeyAccept::Acceptable { actual } => {
            assert_eq!(actual, "SHA256:mpapVT6FjOLWwDsXSv0/8imyC1ERXiUKwzHl2CfoIeQ")
        }
        HostKeyAccept::Refused { reason } => panic!("an unpinned probe is acceptable: {reason}"),
    }
}

/// 探针失败（不可达/认证错/指纹变更）都不是接受窗口。
#[test]
fn probe_failures_are_not_an_acceptance_window() {
    for probe in [
        SftpProbe::Unreachable("ssh transport to nas.lan:22 failed".into()),
        SftpProbe::AuthFailed("bad password".into()),
        SftpProbe::HostKeyChanged {
            expected: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            actual: "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into(),
        },
        SftpProbe::Alive,
    ] {
        match host_key_accept_decision(None, &probe) {
            HostKeyAccept::Refused { .. } => {}
            HostKeyAccept::Acceptable { .. } => {
                panic!("{probe:?} must not open an acceptance window")
            }
        }
    }
}

/// 写回走全漏斗：指纹落盘、既有键保全、重读合法。
#[test]
fn write_pins_the_fingerprint_through_the_full_funnel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = sample_volume(&dir, "sftp-plain", None);
    write_sftp_host_fingerprint(&path, "SHA256:mpapVT6FjOLWwDsXSv0/8imyC1ERXiUKwzHl2CfoIeQ")
        .expect("the pin writes");
    let spec = cloudkit_core::config::load_volume_config(&path).expect("the file reloads");
    assert_eq!(
        spec.settings.sftp_host_fingerprint.as_deref(),
        Some("SHA256:mpapVT6FjOLWwDsXSv0/8imyC1ERXiUKwzHl2CfoIeQ")
    );
    assert_eq!(spec.settings.sftp_host.as_deref(), Some("nas.lan"));
    assert_eq!(spec.settings.sftp_username.as_deref(), Some("u"));
}

/// 半验证文件绝不重写（UPDATE 先例：loader 会拒的文件不动）。
#[test]
fn write_refuses_a_file_the_loader_would_reject() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("broken.toml");
    std::fs::write(&path, "backend = \"sftp\"\nthis is not toml {{{\n").expect("write broken");
    let err = write_sftp_host_fingerprint(&path, "SHA256:x")
        .expect_err("a broken volume file must be refused");
    assert!(
        err.contains("nothing was written"),
        "the refusal is explicit about the no-write outcome: {err}"
    );
}
