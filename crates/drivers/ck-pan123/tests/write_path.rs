//! 写路径桩回放测试（Phase 6 / 123-3）——upload stager 的七步提交链、
//! Reuse/5060/duplicate=2、resume 差集、size 校验、空文件与 abort 矩阵
//! （`stub_common` 假 123pan API + 假 presigned PUT 接收端）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照 = 123-0 写路径腿真机事实
//! + 任务 A–D 规格）：
//!
//! - **七步严格序**：upload_request(/b/，首请求不带 duplicate) →
//!   s3_list_upload_parts → s3_repare（官方拼写/大写 StorageNode/半开
//!   区间——桩逐键校验，错形 400 可见）→ 逐分片裸 PUT（超时
//!   max(300s, MB×2s)）→ 再 list 确认 → s3_complete →
//!   upload_complete/v2 全量 body（isMultipart:true 恒真——错形桩静默
//!   不入库，驱动以 file_info 缺失揭出）；`seq` 时序断言整链顺序；
//! - **Reuse 优先于 5060**：同 etag+size 命中 → 零分片零流量直接入库
//!   （真 FileId 在 data.Info）；
//! - **5060 → duplicate:2 重发**（D4：2=同 FileId 原地覆盖；**绝不发
//!   1**——dup1 的 name(1).ext 副本形态只在桩侧真相建模）；
//! - **resume 差集**：到齐即传 + 每片即落会话；close 失败后重开 writer
//!   同路径同内容 → 同参重发返回同一会话 → list 对账 → **只补缺片**
//!   （PUT 命中差分是能力位⑦的驱动层可观测证据）；
//! - **会话失效**（ListParts NoSuchKey）→ 重走 upload_request 全量重传
//!   恰一次；仍失效 → `Io`（不自陷循环）；
//! - **size 校验**：/v2 回的 file_info.size 与本地 spool 不符 → 报错
//!   不静默（数据完整性纪律）；
//! - **etag 恒真 MD5**：桩对空 etag 回 400「请输入Etag」（真形态）——
//!   全部上传用例通过即钉住「驱动恒发真 MD5」；错 etag 无内容校验
//!   （存储=声称值）由桩存声称 etag 回显体现。
//!
//! 真机未测项（123-5 腿）：空文件链、>64MB sleep 3s、断点续传真形态。

mod stub_common;

use ck_pan123::Pan123Driver;
use cloudkit_storage::{ByteStream, EntryKind, RelPath, StorageDriver, StorageError, WriteHint};
use futures_util::StreamExt;
use stub_common::ApiStub;

fn path(s: &str) -> RelPath {
    RelPath::new(s.trim_start_matches('/')).expect("valid path")
}

async fn stub() -> ApiStub {
    ApiStub::start().await
}

fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(data);
    let out = hasher.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// 伪随机内容（确定性：索引字节流——避免同内容意外命中桩的 Reuse）。
fn content(len: usize, seed: u8) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut x = seed;
    for _ in 0..len {
        x = x.wrapping_mul(31).wrapping_add(17);
        data.push(x);
    }
    data
}

async fn read_all(driver: &Pan123Driver, p: &RelPath) -> Vec<u8> {
    let entry = driver.stat(p).await.expect("stat");
    let stream: ByteStream = driver.reader(&entry.id, None).await.expect("reader");
    let mut out = Vec::new();
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

async fn write_and_close(
    driver: &Pan123Driver,
    p: &RelPath,
    data: &[u8],
    chunk: usize,
) -> Result<cloudkit_storage::Entry, StorageError> {
    let mut stager = driver
        .writer(
            p,
            &WriteHint {
                size: Some(data.len() as u64),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await?;
    for piece in data.chunks(chunk.max(1)) {
        stager.write(piece).await?;
    }
    stager.close().await
}

// --------------------------------------------------- 七步序全链 ---

/// 6MiB（> 5MiB 客户端定值分片 → 2 片：证明驱动按 5MiB 切——服务端
/// SliceSize 16MiB 不采用）：七步时序、分片边界、落库内容逐字节、
/// etag=真 MD5、读路径回读、/a/（mkdir 面）零命中、/v2-less 陷阱端点
/// 零命中。
#[tokio::test]
async fn full_chain_lands_exact_content_in_seven_step_order() {
    let s = stub().await;
    let driver = s.driver();
    let data = content(6 * 1024 * 1024, 0xA7);

    let entry = write_and_close(&driver, &path("f-six.bin"), &data, 1024 * 1024)
        .await
        .expect("close lands the file");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(entry.path, path("f-six.bin"));

    // 七步严格序（PUT 阶段含两片）。
    assert_eq!(
        s.seq(),
        vec![
            "/b:upload_request",
            "/b:list_parts",
            "/b:repare",
            "/put:1",
            "/put:2",
            "/b:list_parts",
            "/b:s3_complete",
            "/b:complete_v2",
        ],
        "the seven-step chain in strict order"
    );
    // 文件面走 /b/（与 mkdir 的 /a/ 前缀分叉）。
    assert_eq!(s.hits("/a/api/file/upload_request"), 0);
    // E4 陷阱端点（无 /v2 的单键完成形态）驱动永不触碰。
    assert_eq!(s.hits("/b:complete_new"), 0);
    assert_eq!(s.hits("/b:complete_v2_silent"), 0);

    // 分片边界 = 5MiB 客户端定值（16MiB 服务端值会是 1 片）——两片 PUT
    // 的命中序列（/put:1 + /put:2）已在上面的 seq 断言中钉住；落库内容
    // 逐字节回读兜底。
    let round = read_all(&driver, &path("f-six.bin")).await;
    assert_eq!(round, data, "read-back round-trips byte for byte");

    // etag 恒真 MD5（错 etag 无内容校验的防线——桩存储声称值并回显）。
    let st = s.state.lock().unwrap();
    let landed = st
        .dirs
        .get("0")
        .unwrap()
        .iter()
        .find(|e| e.name == "f-six.bin")
        .expect("landed");
    assert_eq!(
        landed.etag,
        md5_hex(&data),
        "the claimed etag is the true MD5"
    );
    assert_eq!(landed.data, data);
}

/// 到齐即传：承诺 size 到齐的最后一次 write 后 PUT 阶段已在 close 前
/// 发生（conformance ⑦ 差集观测点的前置形态）。
#[tokio::test]
async fn transfer_starts_when_promised_size_arrives() {
    let s = stub().await;
    let driver = s.driver();
    let data = content(1024 * 1024, 0x31);

    let mut stager = driver
        .writer(
            &path("early.bin"),
            &WriteHint {
                size: Some(data.len() as u64),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await
        .expect("writer");
    stager.write(&data).await.expect("write all");
    assert_eq!(
        s.put_hits_total(),
        1,
        "the single part was PUT before close (transfer-on-arrival)"
    );
    assert_eq!(s.hits("/b:complete_v2"), 0, "commit stays gated on close");
    let entry = stager.close().await.expect("close");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(s.hits("/b:complete_v2"), 1);
}

// --------------------------------------------------- Reuse / 5060 ---

/// Reuse 优先于 5060：同内容（同 etag+size）换名上传 → 零分片零流量
/// 瞬时入库（真 FileId 在 data.Info——桩顶层 FileId 是临时大数）。
#[tokio::test]
async fn reuse_short_circuits_with_zero_part_uploads() {
    let s = stub().await;
    let driver = s.driver();
    let data = content(1024 * 1024, 0x5C);

    write_and_close(&driver, &path("seed.bin"), &data, 256 * 1024)
        .await
        .expect("seed lands");
    let base_puts = s.put_hits_total();
    let base_lists = s.hits("/b:list_parts");
    let base_requests = s.hits("/b:upload_request");

    let entry = write_and_close(&driver, &path("twin.bin"), &data, 256 * 1024)
        .await
        .expect("reuse lands instantly");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(
        s.hits("/b:upload_request"),
        base_requests + 1,
        "one request"
    );
    assert_eq!(
        s.put_hits_total(),
        base_puts,
        "zero parts PUT on a Reuse hit"
    );
    assert_eq!(
        s.hits("/b:list_parts"),
        base_lists,
        "steps 2-7 skipped entirely"
    );
    assert_eq!(s.hits("/b:complete_v2"), 1, "only the seed completed");
    // 换名条目真实可见（真 FileId 生效）。
    let round = read_all(&driver, &path("twin.bin")).await;
    assert_eq!(round, data);
}

/// 5060 → duplicate:2 重发（D4 覆盖语义）：同名 v2 内容 → bare 冲突 →
/// dup2 全量链 → **同 FileId 原地覆盖**（无副本、无 name(1).ext）。
#[tokio::test]
async fn conflict_resends_duplicate_2_and_overwrites_in_place() {
    let s = stub().await;
    let driver = s.driver();
    let v1 = content(1024 * 1024, 0x11);
    let v2 = content(1024 * 1024 + 4096, 0x22);

    let first = write_and_close(&driver, &path("x.bin"), &v1, 512 * 1024)
        .await
        .expect("v1 lands");
    let second = write_and_close(&driver, &path("x.bin"), &v2, 512 * 1024)
        .await
        .expect("v2 overwrites in place");
    // 同 FileId 原地覆盖（真机钉死 a：/v2 完成响应直接回同 FileId）。
    assert_eq!(
        first.id.handle.as_str(),
        second.id.handle.as_str(),
        "duplicate=2 keeps the same FileId in place"
    );

    {
        let st = s.state.lock().unwrap();
        let rows = st.dirs.get("0").unwrap();
        assert_eq!(
            rows.iter().filter(|e| e.name.starts_with("x.bin")).count(),
            1,
            "no name(1).ext copy leaked (duplicate=1 is never sent)"
        );
        let landed = rows.iter().find(|e| e.name == "x.bin").unwrap();
        assert_eq!(landed.data, v2, "in-place content replaced");
    }

    let round = read_all(&driver, &path("x.bin")).await;
    assert_eq!(round, v2);
    assert_eq!(
        driver.stat(&path("x.bin")).await.expect("stat").size,
        v2.len() as u64
    );
}

// --------------------------------------------------- resume 差集 ---

/// resume 差集（能力位⑦的驱动层可观测证据）：第一腿在 s3_complete 处
/// 失败（分片已全部 PUT、会话保留）；重开 writer 同路径同内容 →
/// 到齐即传阶段 list 对账 → **零重传**直接进入提交尾。
#[tokio::test]
async fn resume_after_failed_close_skips_uploaded_parts() {
    let s = stub().await;
    {
        let mut st = s.state.lock().unwrap();
        st.s3_complete_fail_times = 1; // 第一腿的提交点失败
    }
    let driver = s.driver();
    let data = content(6 * 1024 * 1024, 0x93);
    let p = path("resume.bin");

    let err = write_and_close(&driver, &p, &data, 1024 * 1024)
        .await
        .expect_err("the first leg fails at s3_complete");
    assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");
    let puts_after_leg1 = s.put_hits_total(); // 2（两片全传）
    assert_eq!(puts_after_leg1, 2);

    // 第二腿：同路径同内容重开 writer——到齐即传阶段就完成对账（零 PUT）。
    let mut stager = driver
        .writer(
            &p,
            &WriteHint {
                size: Some(data.len() as u64),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await
        .expect("writer");
    stager.write(&data).await.expect("write all");
    assert_eq!(
        s.put_hits_total(),
        puts_after_leg1,
        "the differential: uploaded parts are skipped (server list reconciles)"
    );
    let entry = stager.close().await.expect("second leg completes");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(
        s.put_hits_total(),
        puts_after_leg1,
        "close adds no further PUTs either"
    );
    // 会话保留模型：同参重发返回同一 UploadId（两腿各一次 request）。
    assert_eq!(s.hits("/b:upload_request"), 2);

    let round = read_all(&driver, &p).await;
    assert_eq!(round, data);
}

/// 会话失效（ListParts NoSuchKey——会话已被 complete 消费）：重走
/// upload_request 全量重传**恰一次**后成功。
#[tokio::test]
async fn session_gone_triggers_one_fresh_retransmit() {
    let s = stub().await;
    {
        let mut st = s.state.lock().unwrap();
        st.ghost_next_session = true; // 首会话即刻幽灵化
        st.reissue_fresh_after_ghost = true; // 重发换新会话
    }
    let driver = s.driver();
    let data = content(1024 * 1024, 0x77);

    let entry = write_and_close(&driver, &path("ghost-recover.bin"), &data, 256 * 1024)
        .await
        .expect("recovers via a fresh full retransmit");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(
        s.hits("/b:upload_request"),
        2,
        "re-request happened exactly once"
    );
    // 三次 list：幽灵会话探测 + 新会话对账 + 步骤⑤确认（§5.13 再 list
    // 确认纪律）。
    assert_eq!(s.hits("/b:list_parts"), 3);
    assert_eq!(
        s.put_hits_total(),
        1,
        "the fresh session took the full body"
    );
    let round = read_all(&driver, &path("ghost-recover.bin")).await;
    assert_eq!(round, data);
}

/// 会话失效且重发仍失效 → `Io` 报错（不自陷循环）。
#[tokio::test]
async fn session_gone_twice_is_an_io_error_not_a_loop() {
    let s = stub().await;
    {
        let mut st = s.state.lock().unwrap();
        st.ghost_next_session = true;
        st.reissue_fresh_after_ghost = false; // 重发粘住同一幽灵会话
    }
    let driver = s.driver();
    let data = content(1024 * 1024, 0x88);

    let err = write_and_close(&driver, &path("ghost-loop.bin"), &data, 256 * 1024)
        .await
        .expect_err("refuses to loop");
    assert!(matches!(err, StorageError::Io(_)), "{err:?}");
    assert_eq!(s.put_hits_total(), 0, "nothing was PUT into a dead session");
}

// --------------------------------------------------- 完整性 ---

/// size 校验：/v2 回的 file_info.size 与本地不符 → 报错不静默（数据
/// 完整性纪律——「不符 = 错误」）。
#[tokio::test]
async fn size_mismatch_is_refused_not_swallowed() {
    let s = stub().await;
    {
        let mut st = s.state.lock().unwrap();
        st.v2_size_skew = -1;
    }
    let driver = s.driver();
    let data = content(1024 * 1024, 0x99);

    let err = write_and_close(&driver, &path("skew.bin"), &data, 256 * 1024)
        .await
        .expect_err("size mismatch is fatal");
    match &err {
        StorageError::Unavailable(detail) => {
            assert!(detail.contains("size mismatch"), "{detail}");
        }
        other => panic!("expected a size-mismatch refusal, got {other:?}"),
    }
}

// --------------------------------------------------- 边界 ---

/// 空文件（size=0）：单空分片全链（upload_request size=0 → presign
/// [1,2) → 0 字节 PUT → complete → /v2）——**桩按 pan123-rs/123panNextGen
/// 推断建模，真机未测（123-5 腿）**。
#[tokio::test]
async fn empty_file_roundtrips_via_a_single_empty_part() {
    let s = stub().await;
    let driver = s.driver();

    let stager = driver
        .writer(
            &path("empty.bin"),
            &WriteHint {
                size: Some(0),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await
        .expect("writer");
    let entry = stager.close().await.expect("empty file lands");
    assert_eq!(entry.size, 0);
    assert_eq!(s.put_hits_total(), 1, "one empty part was PUT");
    assert_eq!(s.hits("/b:complete_v2"), 1);
    let round = read_all(&driver, &path("empty.bin")).await;
    assert!(round.is_empty());
}

/// 分片 PUT 的 5xx 幂等重试（§5.15 普通类退避）。
#[tokio::test]
async fn part_put_retries_through_injected_5xx() {
    let s = stub().await;
    {
        let mut st = s.state.lock().unwrap();
        st.put_fail_times = 2; // 首两击 500
    }
    let driver = s.driver();
    let data = content(1024 * 1024, 0xAB);

    let entry = write_and_close(&driver, &path("retry.bin"), &data, 256 * 1024)
        .await
        .expect("retries ride through the injected 5xx");
    assert_eq!(entry.size, data.len() as u64);
    // 1 片内容 + 2 次失败重试 = 3 次命中。
    assert_eq!(s.hits("/put:1"), 3);
    let round = read_all(&driver, &path("retry.bin")).await;
    assert_eq!(round, data);
}

/// 隐式父目录：writer 目标父目录缺失 → 逐级隐式创建（mkdir /a/ 面）。
#[tokio::test]
async fn implicit_parents_are_created_for_the_writer() {
    let s = stub().await;
    let driver = s.driver();
    let data = content(4096, 0xCD);

    let entry = write_and_close(&driver, &path("d1/d2/nested.bin"), &data, 1024)
        .await
        .expect("parents are created implicitly");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(
        s.hits("/a/api/file/upload_request"),
        2,
        "two implicit mkdirs"
    );
    let round = read_all(&driver, &path("d1/d2/nested.bin")).await;
    assert_eq!(round, data);
}

/// 根路径与目录目标 → `Invalid`（契约面）。
#[tokio::test]
async fn writer_refuses_root_and_directory_targets() {
    let s = stub().await;
    let driver = s.driver();
    s.mkdir("0", "adir");

    let err = driver
        .writer(&RelPath::root(), &WriteHint::default())
        .await
        .err()
        .expect("root is not writable");
    assert!(matches!(err, StorageError::Invalid), "{err:?}");
    let err = driver
        .writer(&path("adir"), &WriteHint::default())
        .await
        .err()
        .expect("a directory target is not writable");
    assert!(matches!(err, StorageError::Invalid), "{err:?}");
}

/// WriteHint 契约：承诺 size 与实际不符 → `Invalid`（欠写拒于 close、
/// 超写拒于 write）。
#[tokio::test]
async fn hint_size_mismatch_is_invalid() {
    let s = stub().await;
    let driver = s.driver();

    let mut stager = driver
        .writer(
            &path("short.bin"),
            &WriteHint {
                size: Some(10),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await
        .expect("writer");
    stager.write(&[1, 2, 3]).await.expect("partial write");
    let err = stager.close().await.expect_err("short of the promise");
    assert!(matches!(err, StorageError::Invalid), "{err:?}");

    let mut stager = driver
        .writer(
            &path("over.bin"),
            &WriteHint {
                size: Some(1),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await
        .expect("writer");
    let err = stager.write(&[1, 2]).await.expect_err("over the promise");
    assert!(matches!(err, StorageError::Invalid), "{err:?}");
    stager.abort().await.expect("abort cleans up");
}

/// abort：传输已起（分片已传）后放弃 → 提交族零命中（无 complete/v2），
/// 本地会话与 spool 清理；远端孤儿会话如实记录（123 web API 无已知
/// 释放端点——对照 pan115 K75-2 的 abort_multipart；桩无可观测释放点，
/// 挂账 123-5）。
#[tokio::test]
async fn abort_never_reaches_the_commit_family() {
    let s = stub().await;
    let driver = s.driver();
    let data = content(1024 * 1024, 0xEF);

    let mut stager = driver
        .writer(
            &path("gone.bin"),
            &WriteHint {
                size: Some(data.len() as u64),
                content_hash: None,
                rapid_upload: false,
            },
        )
        .await
        .expect("writer");
    stager.write(&data).await.expect("transfer ran (arrival)");
    assert_eq!(s.put_hits_total(), 1);
    stager.abort().await.expect("abort");
    assert_eq!(s.hits("/b:s3_complete"), 0, "no complete after abort");
    assert_eq!(s.hits("/b:complete_v2"), 0, "no v2 commit after abort");
    // 目标不可见（commit-on-close：未 close 的暂存永不落库）。
    assert!(matches!(
        driver.stat(&path("gone.bin")).await,
        Err(StorageError::NotFound)
    ));
}
