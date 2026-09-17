//! ck-pan115 conformance 套件接入（Phase 5 / 115-4；interfaces §6）。
//!
//! 被测对象 = 全量实现驱动（115-2 读面 + 115-3 写面的产物），后端注入
//! = **假开放平台 + 假 OSS 端点**（hermetic，回环 127.0.0.1:0，无真实
//! 网络；`get_token` 返回的 OSS 端点带 scheme → oss.rs 的 path-style 缝
//! 把对象面也钉在桩上——K69.5「端点可控」的离线形态）。
//!
//! harness 形态声明：
//! - `chunk_size = 5MiB`：**后端分块边界如实报 OSS 分片下限**（115 的
//!   多分片路径自 5MiB 起；断言①的 chunk±1 覆盖与断言⑦的差集上界都
//!   以它计算）；
//! - `delete_missing = NotFound`：查询式幂等声明（115-2 delete 走
//!   `ufile/delete`，不存在 → 430004 → NotFound）；
//! - `empty_range = 空流`：start≥size 直接空流（download.rs open_range
//!   的显式分支，conformance ② 声明形态）；
//! - `error_table` 选码理由（断言⑤注入路径是 **stat**，桩的一次性
//!   错误注入面 `inject_stat_error`——映射经真实协议回放）：
//!   - `430004 → NotFound`：115 的「文件不存在」原生码（K69.7 采样）；
//!   - `770004 → RateLimited`：账号级访问上限——驱动必须报限流而非
//!     Io/Unavailable（D4 的本地硬退避在 dispatch 内已生效，此处钉
//!     映射终点）；**刻意不选** `911`（人工验证）：那是「停止一切」的可
//!     行动形态（fail-fast，不映射为可重试族），其行为由 tests/
//!     errno_mapping.rs 单测钉死，避免把「不可重试」语义塞进
//!     `ErrorReplay` 的可重试矩阵。
//! - `backend_bytes_received`：桩累计收到的 **OSS 对象面字节数**
//!   （UploadPart body + PutObject body 之和）——断言⑦差集比的正是
//!   「真正发给存储后端的字节」。
//!
//! **断言⑦ 与 115 的形态说明**：115 的上传是 spool-then-transfer——
//! `write()` 只落本地 spool，`close()` 才起 hash→init→OSS 全链。断言⑦
//! 的第一段「写满 staged 后 drop」因此**不会**把任何字节发给后端
//! （staged 流量为 0），差集重传的上界检查自然成立。真正证明「只补
//! 缺片」的是本文件下半部分的 `pan115_part_level_diff_resume`（115 特
//! 化用例）：分片级差集——第一轮 close 中断后只补缺失片。

mod stub_common;

use async_trait::async_trait;
use ck_pan115::Pan115Driver;
use cloudkit_storage::conformance::{assert_conforms, ConformanceHarness, ErrorReplay};
use cloudkit_storage::{RelPath, StorageDriver, StorageError, WriteHint};
use stub_common::OsStub;

/// 后端分块边界 = OSS 分片下限 5MiB（upload.rs PART_MIN 的对外声明）。
const CHUNK: u64 = 5 * 1024 * 1024;

struct Pan115Harness {
    /// 桩活到 harness 生命结束。
    stub: OsStub,
    driver: Pan115Driver,
}

impl Pan115Harness {
    async fn new() -> Self {
        let stub = OsStub::start().await;
        let driver = stub.driver();
        Pan115Harness { stub, driver }
    }
}

#[async_trait]
impl ConformanceHarness for Pan115Harness {
    fn driver(&self) -> &dyn StorageDriver {
        &self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    fn delete_missing_yields_not_found(&self) -> bool {
        true
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        true
    }

    fn error_table(&self) -> Vec<ErrorReplay> {
        // 只放**可被「注入恰好一次、随后恢复」契约验证**的码。
        //
        // - `430004 → NotFound`：无状态错误，注入一次即消费；
        // - **刻意不放 `770004`**：账号级上限触发的不是一次性失败而是
        //   D4 的**硬退避窗**（驱动主动进入封锁态，后续调用被本地拒绝
        //   ——这正是设计要的行为）。断言⑤ 的「注入恰好一次，后续 stat
        //   必须成功」与限流语义天然冲突（后续 stat 本就该被拒），强塞
        //   进来只能靠削弱断言通过。770004 → RateLimited{retry_after}
        //   的映射已由 tests/errno_mapping.rs 单测逐形态钉死（含窗口值
        //   来自 limiter 的事实），此处不重复且不歪曲。
        vec![ErrorReplay {
            backend_code: "430004".to_string(),
            expected: StorageError::NotFound,
        }]
    }

    async fn inject_backend_error(&self, backend_code: &str) {
        self.stub.inject_stat_error(backend_code).await;
    }

    async fn backend_bytes_received(&self) -> Option<u64> {
        Some(self.stub.object_bytes_received())
    }
}

/// 断言①–⑥⑧（⑦ 由能力位门控：resume=true 会要求 backend_bytes_received
/// 的严格差集——115 的 spool-then-transfer 形态下第一段流量为 0，上界
/// 检查通过；分片级差集由下方 115 特化用例独立钉死）。
#[tokio::test]
async fn conformance_suite_offline() {
    let harness = Pan115Harness::new().await;
    assert_conforms(&harness).await;
}

// ---------------------------------------------------------------------
// 115 特化：分片级 resume 差集（比断言⑦更强的形态）
// ---------------------------------------------------------------------

/// 分片级差集：12MiB 文件第一轮 close 在第二片后中断（注入 OSS 故障），
/// 第二轮同路径同内容 close 只补缺失片——重传字节 < 总量。
///
/// 这是「声明 resume 即必须真差集」的强证明（断言⑦的通用形态在
/// spool-then-transfer 下测不到片级复用）。
#[tokio::test]
async fn pan115_part_level_diff_resume() {
    let stub = OsStub::start().await;
    let driver = stub.driver();
    let total = 12 * 1024 * 1024u64; // 3 片（5+5+2）
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    let path = RelPath::new("resume/big.bin").expect("path");
    let hint = WriteHint {
        size: Some(total),
        ..WriteHint::default()
    };

    // 让对象面在「第 2 片之后」失败：第 3 片 PUT 返回 500，且该轮不重试
    // （驱动把可重试类原样上抛 → close 失败，会话保留）。
    stub.fail_part_after(2).await;

    let mut st1 = driver.writer(&path, &hint).await.expect("writer 1");
    // 到齐即传：承诺量到齐的那一次 write 推传输链——注入的分片故障
    // 在这里或 close 处浮现（两处都合法，取先到者）。
    let write_res = st1.write(&data).await;
    let err = match write_res {
        Err(e) => e,
        Ok(()) => st1
            .close()
            .await
            .expect_err("the upload must fail mid-parts"),
    };
    assert!(
        matches!(
            err,
            StorageError::RateLimited { .. } | StorageError::Unavailable(_)
        ),
        "the injected OSS failure surfaces as a retryable/temporary class, got {err:?}"
    );
    let first_pass_bytes = stub.object_bytes_received();
    assert!(
        first_pass_bytes >= 2 * 5 * 1024 * 1024,
        "the interrupted pass still delivered the parts it managed before failing"
    );

    // 第二轮：同路径同内容 → 会话复用（uploadId + 已传片）→ 只补缺片。
    stub.clear_part_failure().await;
    let mut st2 = driver.writer(&path, &hint).await.expect("writer 2");
    st2.write(&data).await.expect("write 2");
    let entry = st2.close().await.expect("close after resume");
    assert_eq!(entry.size, total, "resumed upload size");

    let second_pass_bytes = stub.object_bytes_received() - first_pass_bytes;
    assert!(
        second_pass_bytes < total,
        "resume transferred {second_pass_bytes} of {total}: no part reuse happened"
    );
    // 差集的上界：首轮已传的整片（2 片）不重传 → 只补 1 片（5MiB）
    assert!(
        second_pass_bytes <= 5 * 1024 * 1024,
        "the second pass must only carry the missing part (≤5MiB), got {second_pass_bytes}"
    );

    // 逐字节读回（读面与写面串联）
    let got = read_all(driver.reader(&entry.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "resumed object is byte-exact");
}

async fn read_all(mut s: cloudkit_storage::ByteStream) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

// ---------------------------------------------------------------------
// transport 面 + probe（115-4 接线面）
// ---------------------------------------------------------------------

/// transport 面与 StorageDriver 面共享同一后端世界：上传往返、按窗口
/// 读取、删除委派，以及 caps 的 `remote_delete`。
#[tokio::test]
async fn transport_face_shares_the_driver_world() {
    use cloudkit_storage::transport::{CloudTransport, RemoteHandle, UploadJob};

    let stub = OsStub::start().await;
    let driver = std::sync::Arc::new(stub.driver());
    let transport = ck_pan115::Pan115Transport::new(driver.clone());
    let caps = CloudTransport::capabilities(&transport);
    assert!(
        caps.remote_delete,
        "the transport face declares remote_delete"
    );

    // connect 探活（user/info）
    CloudTransport::connect(&transport).await.expect("connect");

    // 上传（upload_stream 路径）；src 目录 OS 级唯一命名（H-T1 同族——
    // 自拼 pid 名在并行/重跑下不撞的论证靠不住，tempfile 一步到位）。
    let payload: Vec<u8> = (0..2048u32).map(|i| (i % 97) as u8).collect();
    let src_dir = tempfile::tempdir().expect("src tempdir").keep();
    std::fs::create_dir_all(&src_dir).expect("dir");
    let src = src_dir.join("payload.bin");
    std::fs::write(&src, &payload).expect("write src");
    let job = UploadJob {
        rel_path: cloudkit_storage::vpath::RelPath::new("/transport.bin").expect("vpath"),
        local_path: src.clone(),
        size: payload.len() as u64,
        chunk_size: payload.len() as u64,
        chunk_count: 1,
    };
    let receipt = CloudTransport::upload(&transport, &job)
        .await
        .expect("upload through the transport face");
    assert_eq!(receipt.uploaded_bytes, payload.len() as u64);

    // 窗口读取（open_range 半开）
    let handle = RemoteHandle {
        path: Some(cloudkit_storage::vpath::RelPath::new("/transport.bin").expect("vpath")),
        total_size: payload.len() as u64,
        first_msg_id: 0,
        chunk_msg_ids: vec![0],
    };
    let stream = CloudTransport::open_range(&transport, &handle, 100, 50)
        .await
        .expect("open_range");
    let got = read_all(stream).await;
    assert_eq!(got, payload[100..150], "the window is byte-exact");

    // 删除（D2 回收站语义的委派）
    CloudTransport::delete_remote(&transport, &handle)
        .await
        .expect("delete");
    let err = driver
        .stat(&cloudkit_storage::RelPath::new("transport.bin").expect("path"))
        .await
        .expect_err("deleted");
    assert!(matches!(err, StorageError::NotFound), "{err:?}");
    let _ = std::fs::remove_dir_all(&src_dir);
}

/// probe 变体分类（H-T2 重写）：Alive / NeedsReauth / RateLimited /
/// Unreachable 四变体全部真实触发——原版的两条腿名实不符（宣称测
/// NeedsReauth 却从未触发；430004 注在 probe 不经过的 ufile_files 面
/// 上，「不误报」断言空转）。注入走 user/info（probe 全链唯一端点），
/// NeedsReauth 腿配 refreshToken 恒败路由（死 refresh_token 形态）。
#[tokio::test]
async fn probe_classifies_alive_and_reauth() {
    let stub = OsStub::start().await;
    let params = stub.params();
    let probe = ck_pan115::probe(&params).await;
    match probe {
        ck_pan115::Pan115Probe::Alive { uid, free, total } => {
            assert_eq!(uid, "1205495");
            assert_eq!(total, Some(201834401841285));
            assert_eq!(free, 125240637825977);
        }
        other => panic!("expected Alive, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_classifies_reauth_rate_limited_and_unreachable() {
    // NeedsReauth：user/info 恒 401*，dispatch 刷新一次即败（refreshToken
    // 路由恒 99）→ Unauthorized{recoverable:true}。
    let stub = OsStub::start().await;
    stub.inject_user_info_error("40101017").await;
    let probe = ck_pan115::probe(&stub.params()).await;
    assert!(
        matches!(probe, ck_pan115::Pan115Probe::NeedsReauth),
        "a dead token pair classifies as NeedsReauth: {probe:?}"
    );

    // RateLimited：user/info 恒 770004（账号级上限——硬退避窗触发）。
    let stub = OsStub::start().await;
    stub.inject_user_info_error("770004").await;
    let probe = ck_pan115::probe(&stub.params()).await;
    assert!(
        matches!(probe, ck_pan115::Pan115Probe::RateLimited { .. }),
        "an account-level cap classifies as RateLimited: {probe:?}"
    );

    // Unreachable：未映射码（555001）→ Rejected → Unavailable 载荷。
    let stub = OsStub::start().await;
    stub.inject_user_info_error("555001").await;
    let probe = ck_pan115::probe(&stub.params()).await;
    assert!(
        matches!(probe, ck_pan115::Pan115Probe::Unreachable { .. }),
        "an unmapped backend error classifies as Unreachable: {probe:?}"
    );
}
