//! CloudTransport 面（Batch B3b 段一）行为钉死：baidu 的 transport 面是
//! StorageDriver 的薄壳 + **整文件 4 并发上传**（K 计划 B3b 补齐 PCFS
//! api.go:440-479 的 4 并发形态；stager 流式路径的串行契约不变）。
//!
//! 观测面：
//! - receipt（K5/K11）：`first_msg_id == fs_id`（句柄即 fs_id，跨 rename
//!   稳定）、`chunk_msg_ids == [fs_id]`（单容器单 chunk——K11 簿记：
//!   upload persist 计数 chunk_count=1、chunks 行 msg_id 与主字段同值，
//!   与 rebuild 契约一致）、`uploaded_bytes == 字节数`；
//! - 整文件路径：mock superfile2 收满全量分片（partseq 齐集）+ create
//!   组装 → fs_id；
//! - open/open_range：fs_id → 下载链路（dlink + 4MiB 有界窗口）字节等；
//! - delete_remote：warm 句柄缓存命中路径（上传顺带填充 → 删除零 list
//!   流量，无全树扫描）；
//! - connect：quota 轻量探活。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver, BaiduTransport};
use cloudkit_storage::transport::{ByteStream, CloudTransport, RemoteHandle, UploadJob};
use cloudkit_storage::vpath::RelPath as VPath;
use cloudkit_storage::{Capabilities, RelPath, StorageDriver, StorageError};
use futures_util::StreamExt;

use common::{MockBaidu, CHUNK_4M, MOCK_ROOT, XPAN_FILE};

/// 三分片载荷（4+4+2 MiB——跨分片边界、非整块尾）。
fn three_part_payload() -> Vec<u8> {
    common::pattern_bytes(CHUNK_4M * 2 + 2 * 1024 * 1024)
}

/// mock 后端 + 驱动 + transport 装配（卷根已播种）。
async fn setup() -> (MockBaidu, BaiduTransport) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect");
    (mock, BaiduTransport::new(driver))
}

/// 单块计划 UploadJob（baidu 分块归驱动 4MiB superfile，transport 计划
/// 不驱动分片——plan 与字节一致性由 upload 面校验）。
fn job(rel: &str, local_path: &std::path::Path, size: u64) -> UploadJob {
    UploadJob {
        rel_path: VPath::new(rel).expect("合法 vpath"),
        local_path: local_path.to_path_buf(),
        size,
        chunk_count: 1,
        chunk_size: size.max(1),
    }
}

/// 源文件写到 OS 临时目录（transport upload 的 local_path）。
fn source_file(tag: &str, data: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("ck-baidu-tf-{}-{}.part", std::process::id(), tag));
    std::fs::write(&p, data).expect("写源文件失败");
    p
}

fn handle_for(job: &UploadJob, first_msg_id: i64, total: u64) -> RemoteHandle {
    RemoteHandle {
        first_msg_id,
        chunk_msg_ids: vec![first_msg_id],
        total_size: total,
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

/// method=list 请求数（delete warm-cache 观测：扫描面流量）。
fn list_requests(mock: &MockBaidu) -> usize {
    mock.recorded()
        .iter()
        .filter(|r| {
            r.http_method == "GET" && r.path == XPAN_FILE && r.query.contains("method=list")
        })
        .count()
}

/// 整文件上传：mock 收满全量分片（partseq 齐集）→ create fs_id ==
/// receipt.first_msg_id（K5）。
#[tokio::test]
async fn upload_whole_file_receipt_is_fs_id() {
    let (mock, transport) = setup().await;
    let data = three_part_payload();
    let src = source_file("whole", &data);
    let j = job("/tf/big.bin", &src, data.len() as u64);

    transport.connect().await.expect("connect 探活");
    let receipt = transport.upload(&j).await.expect("upload");

    // K5：receipt.first_msg_id = fs_id（与 StorageDriver 面 stat 句柄一致）。
    assert_eq!(
        receipt.chunk_msg_ids,
        vec![receipt.first_msg_id],
        "K11：单容器单 chunk 簿记——chunks 行 msg_id = fs_id 与主字段同值"
    );
    assert_eq!(receipt.uploaded_bytes, data.len() as u64);
    let entry = transport
        .driver()
        .stat(&RelPath::new("tf/big.bin").expect("合法卷内路径"))
        .await
        .expect("stat");
    assert_eq!(
        entry.id.handle.as_str(),
        receipt.first_msg_id.to_string(),
        "K5：句柄 = fs_id 十进制字符串"
    );

    // 整文件路径全量分片齐集（4 并发 worker 的完成面——mock 收到即完成，
    // 不做真并发断言，但 partseq 必须一个不少）。
    let uploadids = mock.uploadids();
    let seqs = mock.session_partseqs(uploadids.last().expect("至少一个会话"));
    let mut want: Vec<i64> = (0..3).collect();
    let mut got = seqs.clone();
    want.sort();
    got.sort();
    assert_eq!(got, want, "全部 partseq 齐集：{seqs:?}");
    assert_eq!(mock.bytes_received_total(), data.len() as u64);
    assert_eq!(entry.size, data.len() as u64);

    // open 回读字节等（fs_id → 下载链路）。
    let handle = handle_for(&j, receipt.first_msg_id, receipt.uploaded_bytes);
    let got = read_all(transport.open(&handle).await.expect("open"))
        .await
        .expect("读回");
    assert_eq!(got.len(), data.len());
    assert_eq!(got, data);
}

/// upload_stream 同路：流缓冲到 staging 文件后走整文件路径（帧边界对
/// transport 不透明——任意切帧不影响结果）。
#[tokio::test]
async fn upload_stream_same_path() {
    let (mock, transport) = setup().await;
    let data = three_part_payload();
    let src = source_file("stream", &data);
    let j = job("/tf/stream.bin", &src, data.len() as u64);

    // 奇数帧边界（5MB + 余量——刻意不对齐 4MiB 分片界）。
    let split = 5 * 1024 * 1024;
    let frames: Vec<Result<bytes::Bytes, StorageError>> = vec![
        Ok(bytes::Bytes::copy_from_slice(&data[..split])),
        Ok(bytes::Bytes::copy_from_slice(&data[split..])),
    ];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(frames));
    let receipt = transport
        .upload_stream(&j, stream)
        .await
        .expect("upload_stream");
    assert!(receipt.first_msg_id > 0, "K5：fs_id 而非 0 占位");
    assert_eq!(
        receipt.chunk_msg_ids,
        vec![receipt.first_msg_id],
        "K11：upload_stream 同路同 receipt 形态"
    );
    assert_eq!(receipt.uploaded_bytes, data.len() as u64);

    let handle = handle_for(&j, receipt.first_msg_id, receipt.uploaded_bytes);
    let got = read_all(transport.open(&handle).await.expect("open"))
        .await
        .expect("读回");
    assert_eq!(got, data);
    assert!(!mock.uploadids().is_empty());
}

/// open_range 半开窗口：精确窗口/越界钳制/start>=size 空流。
#[tokio::test]
async fn open_range_half_open_window() {
    let (_mock, transport) = setup().await;
    let data = common::pattern_bytes(1_000_003); // <1 分片
    let src = source_file("range", &data);
    let j = job("/tf/r.bin", &src, data.len() as u64);
    let receipt = transport.upload(&j).await.expect("upload");
    let handle = handle_for(&j, receipt.first_msg_id, receipt.uploaded_bytes);

    let got = read_all(
        transport
            .open_range(&handle, 100, 200)
            .await
            .expect("精确窗口"),
    )
    .await
    .expect("读回");
    assert_eq!(got, data[100..300]);

    let got = read_all(
        transport
            .open_range(&handle, 1_000_000, u64::MAX / 2)
            .await
            .expect("钳制窗口"),
    )
    .await
    .expect("读回");
    assert_eq!(got, data[1_000_000..]);

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

/// 空文件：block_list=`[EMPTY_MD5]`（0 字节 wire 真形，2026-09-23 真网
/// errno=2 实测）+ 零分片 + create 直接收尾。
#[tokio::test]
async fn upload_empty_file_roundtrip() {
    let (_mock, transport) = setup().await;
    let src = source_file("empty", b"");
    let j = job("/tf/empty.bin", &src, 0);
    let receipt = transport.upload(&j).await.expect("upload");
    assert_eq!(receipt.uploaded_bytes, 0);
    assert!(receipt.first_msg_id > 0);
    let got = read_all(
        transport
            .open(&handle_for(&j, receipt.first_msg_id, 0))
            .await
            .expect("open"),
    )
    .await
    .expect("读回");
    assert!(got.is_empty());
}

/// delete_remote warm 缓存命中路径：上传顺带填句柄缓存 → 删除零 list
/// 流量（无全树扫描）→ 条目真删（stat NotFound + 再删 NotFound）。
#[tokio::test]
async fn delete_remote_warm_cache_hit() {
    let (mock, transport) = setup().await;
    let data = common::pattern_bytes(1024);
    let src = source_file("del", &data);
    let j = job("/tf/del.bin", &src, data.len() as u64);
    let receipt = transport.upload(&j).await.expect("upload");
    let handle = handle_for(&j, receipt.first_msg_id, receipt.uploaded_bytes);

    let mark = list_requests(&mock);
    transport
        .delete_remote(&handle)
        .await
        .expect("delete_remote");
    assert_eq!(
        list_requests(&mock),
        mark,
        "warm 缓存命中：删除解析零 list 流量（无扫描）"
    );

    // 真删：StorageDriver 面 stat NotFound；幂等再删 NotFound。
    let rel = RelPath::new("tf/del.bin").expect("合法卷内路径");
    assert_eq!(
        transport.driver().stat(&rel).await.err(),
        Some(StorageError::NotFound)
    );
    assert_eq!(
        transport.delete_remote(&handle).await.err(),
        Some(StorageError::NotFound)
    );
}

/// connect = quota 轻量探活：正常 Ok；后端拒绝（注入 111）→ 错误上抛。
#[tokio::test]
async fn connect_quota_probe_surfaces_backend_errors() {
    let (mock, transport) = setup().await;
    transport.connect().await.expect("connect 探活");
    mock.inject_errno(111);
    let err = transport.connect().await.err();
    assert!(
        matches!(err, Some(StorageError::Unauthorized { .. })),
        "探活失败须按映射表归一：{err:?}"
    );
}

/// 能力位：mirror StorageDriver 位 + remote_delete=true（K4）。
#[tokio::test]
async fn capabilities_mirror_driver_plus_remote_delete() {
    let (_mock, transport) = setup().await;
    let caps: Capabilities = CloudTransport::capabilities(&transport);
    let mut want = StorageDriver::capabilities(transport.driver());
    want.remote_delete = true;
    assert_eq!(caps, want);
    assert!(caps.remote_delete, "K4：baidu 声明真删");
    assert!(transport.as_inbound().is_none());
    assert!(transport.as_chat().is_none());
}

/// 驱动共享：transport 与 StorageDriver 装配共用一个 Arc<BaiduDriver>。
#[tokio::test]
async fn transport_shares_driver_instance() {
    let driver: Arc<BaiduDriver> = {
        let (mock, _base) = MockBaidu::start().await;
        mock.seed_dir(MOCK_ROOT);
        factory(&mock.params(None)).await.expect("driver")
    };
    let transport = BaiduTransport::new(Arc::clone(&driver));
    assert_eq!(transport.driver().volume().scheme(), "baidu");
    let _returned = transport.into_driver();
}
