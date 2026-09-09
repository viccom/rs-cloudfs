//! CloudTransport 面（Batch B3b 段一）行为钉死：local 的 transport 面
//! 是 StorageDriver 的薄壳（K2 path 寻址 + K6 msg_id=0 占位）。
//!
//! 观测面（对照 telegram 时代的 transport 契约）：
//! - receipt：`first_msg_id == 0`（K6 不语义化占位）、`chunk_msg_ids == [0]`
//!   （单 chunk 占位——K11 簿记：upload persist 计数 chunk_count=1，与
//!   rebuild 契约一致）、`uploaded_bytes == 文件字节数`；
//! - 句柄寻址：`RemoteHandle.path`（K2）→ 卷内路径；`path = None` 的句柄
//!   不可寻址 → `Invalid`；
//! - 删除幂等形态：telegram 先例（deleted==0 → NotFound）——路径缺失 →
//!   `NotFound` 恒定。

use std::path::Path;
use std::sync::Arc;

use ck_local::{factory, LocalDriver, LocalParams, LocalTransport};
use cloudkit_storage::transport::{
    ByteStream, CloudTransport, RemoteHandle, UploadJob, UploadReceipt,
};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::{Capabilities, Page, RelPath, StorageDriver, StorageError, VolumeId};
use futures_util::StreamExt;

/// 测试载荷（确定性伪随机，非全零防偷懒匹配）。
fn pattern(n: usize) -> Vec<u8> {
    let mut x = 0x2Fu8;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(31).wrapping_add((i as u8).wrapping_add(7));
            x
        })
        .collect()
}

/// 卷根 + LocalTransport 装配。
fn setup() -> (tempfile::TempDir, LocalTransport) {
    let root = tempfile::tempdir().expect("创建临时根目录失败");
    let driver = LocalDriver::new(root.path().to_path_buf()).expect("LocalDriver 构造失败");
    let transport = LocalTransport::new(Arc::new(driver));
    (root, transport)
}

/// 把字节写到根外的一个源文件（transport upload 的 local_path 语义：
/// 本地磁盘上的源文件）。
fn source_file(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> std::path::PathBuf {
    let p = dir.path().join(name);
    std::fs::write(&p, data).expect("写源文件失败");
    p
}

/// 构造单块 UploadJob（chunk 计划对 local 后端无意义——面不消费，但
/// 契约要求 plan 与字节一致，此处取整文件一块）。
fn job(rel: &str, local_path: &Path, size: u64) -> UploadJob {
    UploadJob {
        rel_path: VPath::new(rel).expect("合法 vpath"),
        local_path: local_path.to_path_buf(),
        size,
        chunk_count: 1,
        chunk_size: size.max(1),
    }
}

/// receipt → 句柄（消费方视角：path 承载寻址，K2）。
fn handle_for(job: &UploadJob, receipt: &UploadReceipt) -> RemoteHandle {
    RemoteHandle {
        first_msg_id: receipt.first_msg_id,
        chunk_msg_ids: receipt.chunk_msg_ids.clone(),
        total_size: receipt.uploaded_bytes,
        path: Some(job.rel_path.clone()),
    }
}

async fn read_all(mut stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

/// upload 往返：文件 → transport → receipt（K6 0 占位）→ open 回读字节等。
#[tokio::test]
async fn upload_roundtrip_via_path_handle() {
    let (root, transport) = setup();
    let data = pattern(100_000);
    let src = source_file(&root, "src.bin", &data);
    let j = job("/docs/a/src.bin", &src, data.len() as u64);

    transport.connect().await.expect("connect 探活");
    let receipt = transport.upload(&j).await.expect("upload");
    assert_eq!(receipt.first_msg_id, 0, "K6：msg_id 恒 0 占位（不语义化）");
    assert_eq!(
        receipt.chunk_msg_ids,
        vec![0],
        "K11：单 chunk 占位——chunk 簿记与 rebuild 契约一致"
    );
    assert_eq!(receipt.uploaded_bytes, data.len() as u64);

    // StorageDriver 面同真相：卷内路径出现同尺寸条目。
    let driver = transport.driver();
    let entry = driver
        .stat(&RelPath::new("docs/a/src.bin").expect("合法卷内路径"))
        .await
        .expect("stat");
    assert_eq!(entry.size, data.len() as u64);

    // transport open 回读字节等。
    let handle = handle_for(&j, &receipt);
    let got = read_all(transport.open(&handle).await.expect("open"))
        .await
        .expect("读回");
    assert_eq!(got, data, "open 回读字节等");
}

/// open_range 半开窗口：精确窗口/越界钳制到预算/start>=size 空流。
#[tokio::test]
async fn open_range_half_open_window() {
    let (root, transport) = setup();
    let data = pattern(4_096);
    let src = source_file(&root, "src.bin", &data);
    let j = job("/w.bin", &src, data.len() as u64);
    let receipt = transport.upload(&j).await.expect("upload");
    let handle = handle_for(&j, &receipt);

    // 精确窗口。
    let got = read_all(
        transport
            .open_range(&handle, 100, 200)
            .await
            .expect("精确窗口"),
    )
    .await
    .expect("读回");
    assert_eq!(got, data[100..300]);

    // len 越界钳制到句柄预算（EOF）。
    let got = read_all(
        transport
            .open_range(&handle, 4_000, u64::MAX / 2)
            .await
            .expect("钳制窗口"),
    )
    .await
    .expect("读回");
    assert_eq!(got, data[4_000..]);

    // start >= size：空流（非错误）。
    let got = read_all(
        transport
            .open_range(&handle, data.len() as u64, 10)
            .await
            .expect("空窗口"),
    )
    .await
    .expect("读回");
    assert!(got.is_empty(), "start>=size 空流");
}

/// 空文件上传与回读（0 字节对象的 receipt/open 形态）。
#[tokio::test]
async fn upload_empty_file_roundtrip() {
    let (root, transport) = setup();
    let src = source_file(&root, "empty.bin", b"");
    let j = job("/empty.bin", &src, 0);
    let receipt = transport.upload(&j).await.expect("upload");
    assert_eq!(receipt.uploaded_bytes, 0);
    let got = read_all(
        transport
            .open(&handle_for(&j, &receipt))
            .await
            .expect("open 空文件"),
    )
    .await
    .expect("读回");
    assert!(got.is_empty());
}

/// delete_remote：删除后 open → NotFound；再删（路径已无）→ NotFound
/// （telegram deleted==0 → NotFound 先例沿用）；path=None 句柄 → Invalid。
#[tokio::test]
async fn delete_remote_then_open_not_found() {
    let (root, transport) = setup();
    let data = pattern(64);
    let src = source_file(&root, "src.bin", &data);
    let j = job("/d.bin", &src, data.len() as u64);
    let receipt = transport.upload(&j).await.expect("upload");
    let handle = handle_for(&j, &receipt);

    transport
        .delete_remote(&handle)
        .await
        .expect("delete_remote");
    assert_eq!(
        transport.open(&handle).await.err(),
        Some(StorageError::NotFound),
        "删除后 open → NotFound"
    );
    // 幂等形态：路径已无 → NotFound（不是 Ok）。
    assert_eq!(
        transport.delete_remote(&handle).await.err(),
        Some(StorageError::NotFound)
    );

    // K2 防御：无 path 的句柄在本地后端不可寻址。
    let pathless = RemoteHandle {
        first_msg_id: 0,
        chunk_msg_ids: vec![0],
        total_size: 1,
        path: None,
    };
    assert_eq!(
        transport.open(&pathless).await.err(),
        Some(StorageError::Invalid)
    );
}

/// upload_stream 流式同路：字节来自流（逐帧 stager.write），receipt 与
/// upload 面行为一致（K6 0 占位）。
#[tokio::test]
async fn upload_stream_roundtrip() {
    let (root, transport) = setup();
    let data = pattern(9_999);
    let src = source_file(&root, "src.bin", &data); // provenance only
    let j = job("/s.bin", &src, data.len() as u64);

    // 三帧流（模拟加密分块帧边界——对 transport 是不透明字节序列）。
    let frames: Vec<Result<bytes::Bytes, StorageError>> = vec![
        Ok(bytes::Bytes::copy_from_slice(&data[..4_000])),
        Ok(bytes::Bytes::copy_from_slice(&data[4_000..8_000])),
        Ok(bytes::Bytes::copy_from_slice(&data[8_000..])),
    ];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
    let receipt = transport
        .upload_stream(&j, stream)
        .await
        .expect("upload_stream");
    assert_eq!(receipt.first_msg_id, 0);
    assert_eq!(receipt.chunk_msg_ids, vec![0]);
    assert_eq!(receipt.uploaded_bytes, data.len() as u64);

    let got = read_all(
        transport
            .open(&handle_for(&j, &receipt))
            .await
            .expect("open"),
    )
    .await
    .expect("读回");
    assert_eq!(got, data);
}

/// 超计划流拒绝（流字节数 > job.size → 错误，不静默截断）。
#[tokio::test]
async fn upload_stream_refuses_over_plan() {
    let (root, transport) = setup();
    let data = pattern(100);
    let src = source_file(&root, "src.bin", &data);
    let j = job("/over.bin", &src, 50); // 承诺 50，流给 100

    let frames: Vec<Result<bytes::Bytes, StorageError>> =
        vec![Ok(bytes::Bytes::copy_from_slice(&data))];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
    let err = transport.upload_stream(&j, stream).await.err();
    assert!(err.is_some(), "超计划流必须拒绝");
}

/// connect 探活：成功且不留痕（卷根经 driver list 为空——探针自清理）。
#[tokio::test]
async fn connect_probes_root_and_leaves_no_trace() {
    let (_root, transport) = setup();
    transport.connect().await.expect("connect 探活");
    // 探针文件必须已删（list(根) 空 = 无孤儿）。
    let listing = transport
        .driver()
        .list(&RelPath::root(), Page::all())
        .await
        .expect("list 根");
    assert!(
        listing.entries.is_empty(),
        "connect 探针不留孤儿：{:?}",
        listing
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>()
    );
}

/// 能力位：mirror StorageDriver 位 + remote_delete=true（K4）。
#[tokio::test]
async fn capabilities_mirror_driver_plus_remote_delete() {
    let (_root, transport) = setup();
    let caps: Capabilities = CloudTransport::capabilities(&transport);
    let mut want = StorageDriver::capabilities(transport.driver());
    want.remote_delete = true;
    assert_eq!(caps, want);
    assert!(caps.remote_delete, "K4：local 声明真删");
    // as_inbound/as_chat 默认 None（storage-only 后端免实现）。
    assert!(transport.as_inbound().is_none());
    assert!(transport.as_chat().is_none());
}

/// factory 装配路径（Arc<LocalDriver> → transport 薄壳）。
#[tokio::test]
async fn factory_bootstraps_transport() {
    let root = tempfile::tempdir().expect("创建临时根目录失败");
    let driver = factory(&LocalParams {
        root: root.path().to_path_buf(),
    })
    .await
    .expect("factory");
    assert_eq!(driver.volume().scheme(), "local");
    let transport = LocalTransport::new(driver);
    assert_eq!(
        transport.driver().volume().scheme(),
        "local",
        "VolumeId 透传"
    );
    let _v: &VolumeId = transport.driver().volume();
}
