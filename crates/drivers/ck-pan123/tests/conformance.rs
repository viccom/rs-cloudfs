//! ck-pan123 conformance 套件接入（Phase 6 / 123-4；interfaces §6）。
//!
//! 被测对象 = 全量实现驱动（123-2 读面 + 123-3 写面的产物），后端注入
//! = **假 123pan web API + 假 presigned PUT 接收端**（hermetic，回环
//! 127.0.0.1:0，无真实网络；repare 的 presignedUrls 指桩自身 `/put`——
//! 分片面钉在桩上，123-0「端点可控」的离线形态）。
//!
//! harness 形态声明：
//! - `chunk_size = 5MiB`：**后端分块边界如实报驱动定值**（upload.rs
//!   `PART_MIN`——服务端 SliceSize 16MiB 是上界参考非指令，123panNextGen
//!   形态同源；断言①的 chunk±1 覆盖与断言⑦的差集上界都以它计算）；
//! - `delete_missing = 幂等 Ok`：trash 对查无实体的句柄 code=0、回读
//!   info 查无 → 驱动判「已删」（delete 的幂等面声明——与 pan115 的
//!   NotFound 查询式声明是**两种合法形态之二选一**，interfaces §6④）；
//! - `empty_range = 空流`：start≥size / 空窗口直接空流（download.rs
//!   open_range 的显式分支，conformance ② 声明形态）；
//! - `error_table` 选码理由（断言⑤注入路径 = stat 的**末级新鲜 list**
//!   ——`list_error_inject` 一次性旋钮，映射经真实协议回放）：
//!   - `5113 → RateLimited{retry_after:None}`：每日下载流量限额（D5
//!     裁决的硬错误族——不绕过、人话指引走 warn 通道）；
//!   - `20101 → Unauthorized{recoverable:false}`：list 面未登录码
//!     （K76.4：web API 无 refresh——重扫码是唯一出路，recoverable
//!     恒 false）；envelope 终态错误不重试、不触发本地硬退避窗，
//!     「注入恰好一次、后续恢复」契约与之相容。
//! - `backend_bytes_received`：桩 `/put` 面**累计收到的字节数**（只增
//!   不减——会话被 v2 消费后行删除不影响计数）——断言⑦差集比的是
//!   「真正发给 presigned 接收端的字节」。
//!
//! **断言⑦ 与 123 的形态说明**：123 的上传是「到齐即传」——`write()`
//! 落 spool，写入量达到承诺 size 的那次 write 立刻推传输链（①–⑤，
//! 含全部分片 PUT 与确认 list），close 只做 ⑥⑦。断言⑦第一段「写满
//! staged 后 drop」因此**已经把全部整片发给后端**（drop 只清 spool，
//! 会话保留——resume 声明），第二轮同路径同内容经 re-request 模型拿
//! 回同一 UploadId + list 对账全在册 → 零重传。下方
//! `pan123_part_level_diff_resume` 再钉分片级差集（第一轮中途故障，
//! 第二轮只补缺失片）。

mod stub_common;

use async_trait::async_trait;
use ck_pan123::Pan123Driver;
use cloudkit_storage::conformance::{assert_conforms, ConformanceHarness, ErrorReplay};
use cloudkit_storage::{RelPath, StorageDriver, StorageError, WriteHint};
use stub_common::ApiStub;

/// 后端分块边界 = 驱动分片定值 5MiB（upload.rs `PART_MIN` 的对外声明）。
const CHUNK: u64 = 5 * 1024 * 1024;

struct Pan123Harness {
    /// 桩活到 harness 生命结束。
    stub: ApiStub,
    driver: Pan123Driver,
}

impl Pan123Harness {
    async fn new() -> Self {
        let stub = ApiStub::start().await;
        let driver = stub.driver();
        Pan123Harness { stub, driver }
    }
}

#[async_trait]
impl ConformanceHarness for Pan123Harness {
    fn driver(&self) -> &dyn StorageDriver {
        &self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    /// 幂等 Ok：trash 对查无实体句柄回 code=0、info 回读查无 → 已删。
    fn delete_missing_yields_not_found(&self) -> bool {
        false
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        true
    }

    fn error_table(&self) -> Vec<ErrorReplay> {
        // 只放**可被「注入恰好一次、随后恢复」契约验证**的码（envelope
        // 终态映射——无本地硬退避窗、无重试循环，注入一次即消费）。
        vec![
            ErrorReplay {
                backend_code: "5113".to_string(),
                expected: StorageError::RateLimited { retry_after: None },
            },
            ErrorReplay {
                backend_code: "20101".to_string(),
                expected: StorageError::Unauthorized { recoverable: false },
            },
        ]
    }

    async fn inject_backend_error(&self, backend_code: &str) {
        // 注入面 = stat 的末级新鲜 list（list_new 一次性旋钮）。
        let code: i64 = backend_code.parse().expect("numeric backend code");
        self.stub.state.lock().unwrap().list_error_inject = Some(code);
    }

    async fn backend_bytes_received(&self) -> Option<u64> {
        Some(self.stub.put_bytes_received())
    }
}

/// 断言①–⑧（⑦ 由能力位门控：resume=true → 要求 backend_bytes_received
/// 的严格差集——「到齐即传 + re-request 会话保留」形态下第一段已把整片
/// 发完，第二轮零重传；分片级差集由下方 123 特化用例独立钉死）。
#[tokio::test]
async fn conformance_suite_offline() {
    let harness = Pan123Harness::new().await;
    assert_conforms(&harness).await;
}

// ---------------------------------------------------------------------
// 123 特化：分片级 resume 差集（比断言⑦更强的形态）
// ---------------------------------------------------------------------

/// 分片级差集：15MiB 文件（3 片）第一轮在片 PUT 处中断（注入 5xx 且
/// 重试耗尽），第二轮同路径同内容只补缺失片——重传字节 < 总量。
///
/// 「声明 resume 即必须真差集」的强证明：中断点不在「到齐即传」的
/// 全量完成处，而在分片序列中途（与服务端保留会话的真实交互面）。
#[tokio::test]
async fn pan123_part_level_diff_resume() {
    let stub = ApiStub::start().await;
    let driver = stub.driver();
    let total = 3 * CHUNK; // 3 片整
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    let path = RelPath::new("resume/big.bin").expect("path");
    let hint = WriteHint {
        size: Some(total),
        ..WriteHint::default()
    };

    // 让分片 PUT 全部失败（重试耗尽后上抛——会话保留，spool 保留到
    // Drop；传输链在「到齐即传」的 write 内启动）。
    stub.state.lock().unwrap().put_fail_times = 10;
    let mut st1 = driver.writer(&path, &hint).await.expect("writer 1");
    let err = st1
        .write(&data)
        .await
        .expect_err("the eager transfer must fail while parts are rejected");
    assert!(
        matches!(
            err,
            StorageError::RateLimited { .. } | StorageError::Unavailable(_)
        ),
        "the injected /put failure surfaces as a retryable/temporary class, got {err:?}"
    );
    drop(st1);
    // 第一轮零片到达（PUT 全 5xx——传输面重试 3 次后放弃）。
    let first_pass_bytes = stub.put_bytes_received();
    assert_eq!(first_pass_bytes, 0, "no part may land while /put rejects");

    // 第二轮：同路径同内容 → re-request 同一 UploadId（服务端会话保留）
    // → list 对账零在册 → 全量 3 片重传 + close 收尾。差集正确性的
    // 真正判据在第三段（部分片在册时只补缺片）——见下一用例。
    stub.state.lock().unwrap().put_fail_times = 0;
    let mut st2 = driver.writer(&path, &hint).await.expect("writer 2");
    st2.write(&data).await.expect("write 2");
    let entry = st2.close().await.expect("close after resume");
    assert_eq!(entry.size, total, "resumed upload size");
    let second_pass_bytes = stub.put_bytes_received() - first_pass_bytes;
    assert_eq!(
        second_pass_bytes, total,
        "a zero-part session retransmits all"
    );

    // 逐字节读回（读面与写面串联）。
    let got = read_all(driver.reader(&entry.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "resumed object is byte-exact");
}

/// 部分片在册的差集：第一轮第 1 片落地后中断（第 2 片起失败），第二
/// 轮只补第 2、3 片——重传字节 ≤ 2 片（5MiB 片界对齐）。
#[tokio::test]
async fn pan123_partial_parts_resume_only_missing() {
    let stub = ApiStub::start().await;
    let driver = stub.driver();
    let total = 3 * CHUNK;
    let data: Vec<u8> = (0..total).map(|i| (i % 249) as u8).collect();
    let path = RelPath::new("resume/partial.bin").expect("path");
    let hint = WriteHint {
        size: Some(total),
        ..WriteHint::default()
    };

    // 第 1 片成功、其后全失败（`put_fail_after_parts = 1`：位置旋钮——
    // 桩先落 1 片再断，pan115 `fail_part_after` 同形态）。
    stub.state.lock().unwrap().put_fail_after_parts = 1;
    let mut st1 = driver.writer(&path, &hint).await.expect("writer 1");
    let err = st1
        .write(&data)
        .await
        .expect_err("the transfer must fail after part 1");
    assert!(
        matches!(
            err,
            StorageError::RateLimited { .. } | StorageError::Unavailable(_)
        ),
        "got {err:?}"
    );
    drop(st1);
    let first_pass_bytes = stub.put_bytes_received();
    assert_eq!(first_pass_bytes, CHUNK, "exactly part 1 landed");

    // 第二轮：同参 re-request → 同一 UploadId → list 对账 [1] 在册
    // → 只补 2、3 两片（旋钮解除——位置故障只在第一轮存在）。
    stub.state.lock().unwrap().put_fail_after_parts = 0;
    let mut st2 = driver.writer(&path, &hint).await.expect("writer 2");
    st2.write(&data).await.expect("write 2");
    let entry = st2.close().await.expect("close");
    assert_eq!(entry.size, total, "size");

    let second_pass_bytes = stub.put_bytes_received() - first_pass_bytes;
    assert_eq!(
        second_pass_bytes,
        2 * CHUNK,
        "only parts 2 and 3 are retransmitted (diff-set, not full)"
    );

    let got = read_all(driver.reader(&entry.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "byte-exact after diff resume");
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
// transport 面 + probe（123-4 接线面）
// ---------------------------------------------------------------------

/// transport 面与 StorageDriver 面共享同一后端世界：上传往返、按窗口
/// 读取、删除委派，以及 caps 的 `remote_delete`。
#[tokio::test]
async fn transport_face_shares_the_driver_world() {
    use cloudkit_storage::transport::{CloudTransport, RemoteHandle, UploadJob};

    let stub = ApiStub::start().await;
    let driver = std::sync::Arc::new(stub.driver());
    let transport = ck_pan123::Pan123Transport::new(driver.clone());
    let caps = CloudTransport::capabilities(&transport);
    assert!(
        caps.remote_delete,
        "the transport face declares remote_delete"
    );

    // connect 探活（user/info）
    CloudTransport::connect(&transport).await.expect("connect");

    // 上传（upload_stream 路径；src 目录 OS 级唯一命名——H-T1 同族）。
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

/// probe 变体分类：Alive（含流量余量/vip 透出面）/ NeedsReauth /
/// Unreachable 三变体真实触发。
#[tokio::test]
async fn probe_classifies_alive_with_traffic_face() {
    let stub = ApiStub::start().await;
    let probe = ck_pan123::probe(&stub.params()).await;
    match probe {
        ck_pan123::Pan123Probe::Alive {
            uid,
            free,
            total,
            traffic_remain,
            vip,
        } => {
            assert_eq!(uid, "4006416717");
            assert_eq!(total, Some(2199023255552));
            assert_eq!(free, 2199023255552 - 1073741824);
            // 桩 traffic/check：未超额 + originalRemainTraffic 10735321088。
            assert_eq!(traffic_remain, Some(10735321088));
            assert!(!vip, "the stub account is not VIP");
        }
        other => panic!("expected Alive, got {other:?}"),
    }

    // 流量超额面：isTrafficExceeded=true → 余量报 0（doctor 的硬指引面）。
    stub.state.lock().unwrap().traffic_exceeded = true;
    let probe = ck_pan123::probe(&stub.params()).await;
    match probe {
        ck_pan123::Pan123Probe::Alive {
            traffic_remain: Some(0),
            ..
        } => {}
        other => panic!("exceeded traffic reports zero remain, got {other:?}"),
    }

    // traffic/check 查询失败 → 余量降级 None（探活不因诊断面失败而误报）。
    let stub2 = ApiStub::start().await;
    stub2.state.lock().unwrap().traffic_check_code = Some(555001);
    let probe = ck_pan123::probe(&stub2.params()).await;
    match probe {
        ck_pan123::Pan123Probe::Alive {
            traffic_remain: None,
            ..
        } => {}
        other => panic!("a failed traffic read degrades to None, got {other:?}"),
    }
}

/// NeedsReauth：死 token（user/info 恒 401）；Unreachable：未映射码。
#[tokio::test]
async fn probe_classifies_reauth_and_unreachable() {
    // NeedsReauth：user/info 恒 401（user 面未登录码——无 refresh 可试）。
    let stub = ApiStub::start().await;
    stub.state.lock().unwrap().user_info_code = Some(401);
    let probe = ck_pan123::probe(&stub.params()).await;
    assert!(
        matches!(probe, ck_pan123::Pan123Probe::NeedsReauth),
        "a dead token classifies as NeedsReauth: {probe:?}"
    );

    // Unreachable：未映射码（555001）→ Rejected → Unavailable 载荷。
    let stub = ApiStub::start().await;
    stub.state.lock().unwrap().user_info_code = Some(555001);
    let probe = ck_pan123::probe(&stub.params()).await;
    assert!(
        matches!(probe, ck_pan123::Pan123Probe::Unreachable { .. }),
        "an unmapped backend error classifies as Unreachable: {probe:?}"
    );
}
