//! transport 面（CloudTransport）行为测试——复审 H1 修复批（2026-09-25）。
//!
//! 两个钉子（sftp-review ① 判例同型）：
//!
//! - **store_bytes / upload_stream 的错误路径显式 abort**：webdav 的
//!   stash 在 writer 打开时已执行（覆盖写场景旧对象 → `.ckwd-*.old`），
//!   裸 Drop stager 会把旧版本困在 list 过滤的残件上、final 丢空——
//!   错误必须经 `abort()` 复位现场后再上抛；
//! - **upload_stream 流式面本身**：上传队列 aead_v2 加密路径**无条件**
//!   走 `CloudTransport::upload_stream`（trait 缺省 `Unsupported` 无降级
//!   ——upload_queue 的 v2 腿没有回退路径），六宽面驱动里 webdav 曾是
//!   唯一缺口：缺失即加密卷上传全灭。

mod stub;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ck_webdav::{parse_from_map, WebdavDriver, WebdavTransport};
use cloudkit_storage::transport::{ByteStream, CloudTransport, UploadJob};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::StorageError;
use stub::{spawn_stub, AuthMode, Knobs, StubHandle, StubStyle, Vfs};

fn transport(handle: &StubHandle) -> WebdavTransport {
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), handle.url.clone());
    let params = parse_from_map(&map).expect("test params parse");
    WebdavTransport::new(Arc::new(
        WebdavDriver::new(params).expect("driver constructs"),
    ))
}

/// 单块 UploadJob（chunk 计划对 webdav 无意义——面不消费，契约仍取
/// plan 与字节一致，ck-local 同款形态）。
fn job(vpath: &str, local_path: &Path, size: u64) -> UploadJob {
    UploadJob {
        rel_path: VPath::new(vpath).expect("合法 vpath"),
        local_path: local_path.to_path_buf(),
        size,
        chunk_count: 1,
        chunk_size: size.max(1),
    }
}

/// 覆盖写 + 写错误的现场恢复断言面（三条错误腿共用）：旧版本逐字节
/// 回位 + 无 `.ckwd-` 暂存残件。
async fn assert_old_version_restored(handle: &StubHandle) {
    assert_eq!(
        handle.take("/f.bin").as_deref(),
        Some(b"old-version".as_slice()),
        "旧版本必须逐字节回位（abort 复位 stash——裸 Drop 会把它困在 .old）"
    );
    for path in handle.snapshot().keys() {
        assert!(!path.contains(".ckwd-"), "无暂存残件: {path}");
    }
}

/// H1① 钉：upload（store_bytes 面）写错误后旧版本回位——`write?` 早退
/// 的裸 Drop 会把 writer 打开时已 stash 的旧版本困在 `.ckwd-*.old`
/// （final 丢空，重试耗尽后文件对卷消失）。
#[tokio::test]
async fn upload_write_error_aborts_and_restores_the_stashed_old_version() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"old-version");
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await;
    let t = transport(&handle);
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.bin");
    std::fs::write(&src, b"0123456789").expect("write src");
    // 承诺 5 字节、实际给 10 → write 超 hint 承诺 → Invalid（sftp hint 契约）。
    let err = t
        .upload(&job("/f.bin", &src, 5))
        .await
        .expect_err("over-plan upload must fail");
    assert!(
        matches!(err, StorageError::Invalid),
        "hint overrun surfaces Invalid: {err:?}"
    );
    assert_old_version_restored(&handle).await;
    handle.shutdown().await;
}

/// H1② 钉：upload_stream 流式面就位——三帧流整链上传 + K6 receipt 形态
/// （trait 缺省形态下本腿 = Unsupported，红即缺口实证）。
#[tokio::test]
async fn upload_stream_roundtrip() {
    let handle = spawn_stub(
        Vfs::new(),
        AuthMode::None,
        Knobs::default(),
        StubStyle::rclone(),
    )
    .await;
    let t = transport(&handle);
    let data: Vec<u8> = (0..9_999u32).map(|i| (i % 251) as u8).collect();
    // 三帧流（模拟加密分块帧边界——对 transport 是不透明字节序列）。
    let frames: Vec<Result<bytes::Bytes, StorageError>> = vec![
        Ok(bytes::Bytes::copy_from_slice(&data[..4_000])),
        Ok(bytes::Bytes::copy_from_slice(&data[4_000..8_000])),
        Ok(bytes::Bytes::copy_from_slice(&data[8_000..])),
    ];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
    let receipt = t
        .upload_stream(
            &job("/s.bin", Path::new("provenance-only"), data.len() as u64),
            stream,
        )
        .await
        .expect("upload_stream");
    assert_eq!(receipt.first_msg_id, 0, "K6：msg_id 恒 0 占位（不语义化）");
    assert_eq!(
        receipt.chunk_msg_ids,
        vec![0],
        "K11：单 chunk 占位——chunk 簿记与 rebuild 契约一致"
    );
    assert_eq!(receipt.uploaded_bytes, data.len() as u64);
    assert_eq!(
        handle.take("/s.bin").as_deref(),
        Some(data.as_slice()),
        "流字节整链落地"
    );
    handle.shutdown().await;
}

/// H1②+①：超计划流（流字节 > job.size）→ 拒绝（不静默截断）+ abort
/// 恢复旧版本。
#[tokio::test]
async fn upload_stream_over_plan_aborts_and_restores_the_stashed_old_version() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"old-version");
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await;
    let t = transport(&handle);
    let frames: Vec<Result<bytes::Bytes, StorageError>> =
        vec![Ok(bytes::Bytes::from(vec![7u8; 100]))];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
    let err = t
        .upload_stream(&job("/f.bin", Path::new("x"), 50), stream)
        .await
        .expect_err("over-plan stream must fail");
    assert!(
        matches!(err, StorageError::Unavailable(_)),
        "超计划流拒绝为 Unavailable: {err:?}"
    );
    assert_old_version_restored(&handle).await;
    handle.shutdown().await;
}

/// H1②+①：流中帧错误（帧泵 `frame?` 早退点）→ abort 恢复旧版本。
#[tokio::test]
async fn upload_stream_frame_error_aborts_and_restores_the_stashed_old_version() {
    let mut vfs = Vfs::new();
    vfs.seed_file("/f.bin", b"old-version");
    let handle = spawn_stub(vfs, AuthMode::None, Knobs::default(), StubStyle::rclone()).await;
    let t = transport(&handle);
    let frames: Vec<Result<bytes::Bytes, StorageError>> = vec![
        Ok(bytes::Bytes::from_static(b"partial")),
        Err(StorageError::Io("boom: stream died mid-upload".to_string())),
    ];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
    let err = t
        .upload_stream(&job("/f.bin", Path::new("x"), 10), stream)
        .await
        .expect_err("frame error must fail the upload");
    assert!(
        matches!(err, StorageError::Io(_)),
        "帧错误如实上抛: {err:?}"
    );
    assert_old_version_restored(&handle).await;
    handle.shutdown().await;
}
