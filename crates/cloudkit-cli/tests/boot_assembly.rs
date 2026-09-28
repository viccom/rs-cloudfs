//! M15 / K91: boot assembly tolerates a failed volume — K22 「坏卷不伤
//! 兄弟」 extended to the BOOT assembly loop (previously one bad volume's
//! connect/config failure killed the entire multi-volume boot via `?` /
//! `process::exit(1)`). Direction A (负责人 2026-09-28 裁决): the failure
//! is folded into a named [`cloudkit_cli::AssemblyFailure`], the rest of
//! the volumes assemble, and the boot continues without the bad one.
//!
//! The bad volume here is an sftp config pointing at a guaranteed-closed
//! loopback port: with the `sftp` feature compiled the connect is
//! refused instantly (TCP), without it the K31 driver-required error
//! fires — both are assembly failures, so the all-failing test is
//! crop-independent. The \*\*mixed\*\* test needs a volume that really does
//! assemble, which takes a compiled driver: it is gated on `local`
//! (present in the default build and the local crop; the `none` crop has
//! no driver at all, so no good volume can exist there — the CI
//! `feature gates (none)` leg caught exactly that assumption once).

use cloudkit_core::config::load_volumes;

/// A good local volume TOML (root = an existing directory; no
/// `drive_letter` — this test exercises assembly only, not mounting).
/// `local`-gated with the mixed leg it serves: the crop builds have no
/// driver at all, so neither the volume nor this constant exists there.
#[cfg(feature = "local")]
const GOOD_LOCAL_TOML: &str = "backend = \"local\"\nlocal_root = \"root\"\n";

/// A bad sftp volume TOML: every key validates, but the host:port is a
/// loopback port nothing listens on — the assembly connect fails.
const BAD_SFTP_TOML: &str = concat!(
    "backend = \"sftp\"\n",
    "sftp_host = \"127.0.0.1\"\n",
    "sftp_port = 1\n",
    "sftp_username = \"u\"\n",
    "sftp_password = \"p\"\n",
    "sftp_host_fingerprint = \"SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"\n",
);

fn write_volume(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(path, body).expect("write volume toml");
}

/// 方向 A 主腿：坏卷折叠成 `AssemblyFailure`（带名字与原因），好卷照常
/// 装配，boot 信号 = 继续。修复前红：`assemble_volume_injections` 不存在
/// （seam 缺失），且旧行为里坏卷的 Err 直接炸掉整个装配环。
///
/// `local` 门控：本腿需要一个**真能装配**的好卷，而裁剪构建
/// （`--no-default-features`）里一个驱动都没有——那种图下不存在好卷，
/// 该组合由下面的全坏腿覆盖（CI none 腿曾揭出本测试此前的这一假定）。
#[cfg(feature = "local")]
#[tokio::test]
async fn a_failed_volume_is_folded_into_a_named_failure_while_good_volumes_assemble() {
    let dir = tempfile::tempdir().expect("tempdir");
    let volumes_dir = dir.path().join("volumes");
    std::fs::create_dir_all(&volumes_dir).expect("volumes dir");
    std::fs::create_dir_all(dir.path().join("root")).expect("local root exists");
    write_volume(&volumes_dir, "a-good", GOOD_LOCAL_TOML);
    write_volume(&volumes_dir, "b-bad", BAD_SFTP_TOML);

    let volumes = load_volumes(&volumes_dir).expect("load volumes");
    assert_eq!(volumes.len(), 2, "both volume files discovered");

    let (injections, failures, interrupted) =
        cloudkit_cli::assemble_volume_injections(volumes).await;
    assert!(!interrupted, "no Ctrl+C in this scenario");
    assert_eq!(injections.len(), 1, "the good volume assembles");
    assert_eq!(injections[0].0.name, "a-good");
    assert_eq!(
        failures.len(),
        1,
        "the bad volume is tolerated, not fatal: {failures:?}"
    );
    assert_eq!(failures[0].name, "b-bad");
    assert!(
        !failures[0].error.is_empty(),
        "the failure carries a cause for the console announcement"
    );
}

/// 全坏形态：所有卷折叠成失败清单，零注入——下游的零卷 boot（K87）照常
/// 承接（仪表盘是 boot 的产品）。
#[tokio::test]
async fn all_volumes_failing_leaves_an_empty_assembly_with_every_volume_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let volumes_dir = dir.path().join("volumes");
    std::fs::create_dir_all(&volumes_dir).expect("volumes dir");
    write_volume(&volumes_dir, "a-bad", BAD_SFTP_TOML);
    write_volume(&volumes_dir, "b-bad", BAD_SFTP_TOML);

    let volumes = load_volumes(&volumes_dir).expect("load volumes");
    let (injections, failures, interrupted) =
        cloudkit_cli::assemble_volume_injections(volumes).await;
    assert!(!interrupted);
    assert!(injections.is_empty(), "nothing assembles");
    assert_eq!(failures.len(), 2, "both failures are named: {failures:?}");
    assert_eq!(failures[0].name, "a-bad");
    assert_eq!(failures[1].name, "b-bad");
}
