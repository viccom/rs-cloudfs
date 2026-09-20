//! 123-5 真机矩阵（Phase 6；123-4 预置骨架 → 123-5 展开为全矩阵）——
//! **默认 `#[ignore]`**，凭据只经 env，本套之外零真机。
//!
//! 运行形态（照 ck-pan115 live_matrix 先例）：
//!
//! ```text
//! CYDRIVE_PAN123_TEST_TOKEN=<90天token> \
//! CYDRIVE_PAN123_TEST_ROOT=<专用测试目录 folder id> \
//! cargo test -p ck-pan123 --test live_matrix -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 前置注明：
//! - **token**：`cydrive setup`（QR/sign_in）产出，或网页控制台手取；
//!   90 天有效（K76.4 无 refresh——过期重扫）；
//! - **root**：**强烈建议专用测试目录的 folder id**（纯数字）——矩阵
//!   会在卷内建删大量 `cydrive-conformance-*` 条目；缺省 `"0"` =
//!   网盘根（脏数据自担）；K74 教训：测试 root 绝不用 "0"（rebuild/
//!   测试完整目录 trash 语义需要专用根）；
//! - 断言族与离线 conformance 同源（interfaces §6 八条；⑤ 错误映射
//!   与分片级故障注入无真机注入面——离线桩钉死，本套不重复）；
//! - 命名纪律（K72）：stamp 唯一名 + 按轮随机内容；全部真机文件限定
//!   `/_e2e_pan123/` 专用子目录（pan115 live_matrix 作业纪律同源），
//!   收尾清理（trash 语义 = D2，可恢复）；
//! - **流量预算**（D5：免费日额 ≈10GiB）：全矩阵下载量 ≈35MiB——
//!   逐字校验各恰一次（roundtrip 开区间读 + resume 全量读），Range
//!   断言用小窗，并发读窗收窄到 64KiB（原骨架 5MiB 级窗口在 123-5
//!   收窄——干扰无关性断言面不变，流量预算让位给逐字校验）。
//!
//! 三条用例（`--test-threads=1` 下按名字典母序执行 = 复用 → 差集 →
//! 往返：秒传腿的「上传不动余量」断言先于一切下载发生，避开下载计
//! 账的滞后窗口）：
//! 1. [`live_rapid_upload_reuse_hits`]：秒传命中——同内容异名 →
//!    Reuse 分支（close 显著快于全量 + 上传窗内余量零变动 + 窗口
//!    逐字）；
//! 2. [`live_resume_diff_set_server_accounting`]：分片差集——裸 client
//!    预置「3 片只传 1 片」的服务端状态 → 驱动重开同路径 → 同
//!    UploadId（会话文件对账）+ 只补缺片（timing）+ 逐字回读；
//! 3. [`live_roundtrip_ranges_traffic_and_layout`]：上传往返逐字（开
//!    区间 `bytes=100-` 形态）/ Range 中窗 + 尾窗 EOF 钳制 / **下载
//!    余量联动观测**（全量读前后 traffic/check）/ rename 文件与目录
//!    / 分页稳定有序 / 并发读互不干扰。

use std::path::PathBuf;

use ck_pan123::api::{PartsOutcome, UploadRequestOutcome};
use ck_pan123::upload::UploadSession;
use ck_pan123::{Pan123Driver, Pan123Params};
use cloudkit_storage::{EntryKind, Page, PageCursor, Range, RelPath, StorageDriver, WriteHint};
use md5::{Digest, Md5};

/// 专用作业目录名（全部真机文件只在这里出现）。
const E2E_DIR: &str = "_e2e_pan123";

const MIB: u64 = 1024 * 1024;

fn stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("cydrive-conf-{nanos:x}")
}

/// 确定性伪随机载荷（pan115_e2e `pseudo_random` 同款 LCG——64 位状
/// 态）。**不用小种子线性递推**（u8 种子空间 256——首轮真机即与历史
/// 上传撞 etag 触发服务端 Reuse，K72「每轮随机内容」失守）。
fn pattern(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// 每轮随机种子（K72：固定内容会被服务端 etag 去重短路——第二轮
/// upload_request 直接 Reuse，秒传/差集断言全部失真；本套首轮真机
/// 复跑即踩到）。
fn rand_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
}

/// 从 env 组装真机驱动（凭据缺失 → panic 提示如何运行）。
fn live_params() -> Pan123Params {
    live_params_with_sessions(None)
}

/// 同上但带会话目录（resume 腿的会话文件对账面）。
fn live_params_with_sessions(dir: Option<PathBuf>) -> Pan123Params {
    let token = std::env::var("CYDRIVE_PAN123_TEST_TOKEN").unwrap_or_else(|_| {
        panic!(
            "CYDRIVE_PAN123_TEST_TOKEN is required for the live matrix: obtain a token via \
             `cydrive setup` (QR scan or sign_in) and export it; CYDRIVE_PAN123_TEST_ROOT may \
             pin a dedicated test folder id (default 0 = the netdisk root)"
        )
    });
    let root = std::env::var("CYDRIVE_PAN123_TEST_ROOT").unwrap_or_else(|_| "0".to_string());
    Pan123Params {
        token: Some(token),
        root,
        sessions_dir: dir,
        ..Pan123Params::default()
    }
}

async fn read_all(mut s: cloudkit_storage::ByteStream) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = s.next().await {
        out.extend_from_slice(&chunk.expect("chunk"));
    }
    out
}

/// 分页遍历（limit=2 强制翻页；稳定有序断言的取数面）。
async fn walk_paged(driver: &Pan123Driver, base: &RelPath) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = PageCursor::Start;
    loop {
        let listing = driver
            .list(base, Page { limit: 2, cursor })
            .await
            .expect("list");
        out.extend(listing.entries.iter().map(|e| e.path.as_str().to_string()));
        cursor = match listing.next {
            Some(next) => next,
            None => break,
        };
    }
    out
}

/// traffic/check 的余量字节（诊断端点本身不耗下载额度）。
async fn traffic_remain(driver: &Pan123Driver, fid: i64) -> i64 {
    driver
        .client()
        .traffic_check(&[fid])
        .await
        .expect("traffic/check")
        .original_remain_traffic
}

/// Entry 句柄 → 裸 file id（句柄即数字串——upload.rs entry_of 形态）。
fn fid_of(entry: &cloudkit_storage::Entry) -> i64 {
    entry
        .id
        .handle
        .as_str()
        .parse::<i64>()
        .expect("numeric fid")
}

/// 读会话目录里的全部会话记录（resume 腿的差集对账面）。
fn read_sessions(dir: &std::path::Path) -> Vec<UploadSession> {
    let root = dir.join("pan123_state").join("sessions");
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(rec) = serde_json::from_str::<UploadSession>(&text) {
                        out.push(rec);
                    }
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------- 1. 秒传 ---

/// 秒传命中（真机复证桩面结论）：同 etag+size 异名 → 服务端 Reuse 优
/// 先于 5060，零分片零流量直接入库。观测面：
/// - **timing**：B 轮（秒传）的 write+close 显著短于 A 轮（全量 2 片
///   PUT + 七步链）；
/// - **余量零变动**：上传窗（A 已传 + B 秒传，无下载）前后
///   traffic/check 余量相等——上传不耗下载额度的真机钉；
/// - **内容同一性**：B 条目窗口读与源逐字（头/跨片/尾三窗）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_PAN123_TEST_TOKEN (see the module docs)"]
async fn live_rapid_upload_reuse_hits() {
    let driver = ck_pan123::factory(&live_params())
        .await
        .expect("connect the live backend");
    let tag = stamp();
    let base = RelPath::new(&format!("{E2E_DIR}/{tag}")).expect("base path");
    let size = 6 * MIB + 512; // 2 分片（第二片 512B 尾片）
    let data = pattern(size as usize, rand_seed());

    // A 轮：全量上传（timing 基线）。
    let a_path = base.join("reuse-a.bin").expect("path a");
    let t0 = std::time::Instant::now();
    let mut st = driver
        .writer(
            &a_path,
            &WriteHint {
                size: Some(size),
                ..Default::default()
            },
        )
        .await
        .expect("writer a");
    st.write(&data).await.expect("write a");
    let t_full = t0.elapsed();
    let entry_a = st.close().await.expect("close a");
    assert_eq!(entry_a.size, size);

    // 余量稳定性预采样（2 次相等才启用等值断言——防御下载计账滞后
    // 穿透进本窗；本套内秒传腿先于一切下载执行，正常必稳）。
    let fid_a = fid_of(&entry_a);
    let s1 = traffic_remain(&driver, fid_a).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let s2 = traffic_remain(&driver, fid_a).await;
    let stable = s1 == s2;

    // B 轮：同内容异名 → Reuse 分支。
    let b_path = base.join("reuse-b.bin").expect("path b");
    let t1 = std::time::Instant::now();
    let mut st = driver
        .writer(
            &b_path,
            &WriteHint {
                size: Some(size),
                ..Default::default()
            },
        )
        .await
        .expect("writer b");
    st.write(&data).await.expect("write b");
    let t_reuse = t1.elapsed();
    let entry_b = st.close().await.expect("close b");
    assert_eq!(entry_b.size, size, "reuse entry carries the full size");
    assert!(
        t_reuse < t_full,
        "reuse ({t_reuse:?}) must beat the full chain ({t_full:?})"
    );
    eprintln!("[SUMMARY] reuse|full_write_close={t_full:?}|reuse_write={t_reuse:?}|size={size}");

    // 内容同一性：头/跨片边界/尾三窗逐字（小窗——流量预算纪律）。
    let windows = [(0u64, 4096usize), (MIB - 2048, 4096), (size - 4096, 4096)];
    for (off, len) in windows {
        let got = read_all(
            driver
                .reader(
                    &entry_b.id,
                    Some(Range::new(off, Some(off + len as u64)).expect("range")),
                )
                .await
                .expect("window reader"),
        )
        .await;
        assert_eq!(
            got,
            data[off as usize..off as usize + len],
            "reuse window [{off},+{len})"
        );
    }

    // 上传窗余量零变动（上传不耗下载额度——D5 面的真机钉）。
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let s3 = traffic_remain(&driver, fid_of(&entry_b)).await;
    if stable {
        assert_eq!(s3, s1, "an upload-only window must not move the quota");
    } else {
        assert!(s3 <= s2, "quota must not grow");
        eprintln!(
            "[note] pre-samples unstable ({s1} vs {s2}) — equality assert downgraded to a bound"
        );
    }
    eprintln!(
        "[SUMMARY] reuse_traffic|before={s1}|after={s3}|moved={}",
        s1 - s3
    );

    // 清理。
    let dir_entry = driver.stat(&base).await.expect("base entry");
    driver.delete(&dir_entry.id).await.expect("cleanup");
    assert!(driver.stat(&base).await.is_err(), "reuse folder cleaned");
}

// ------------------------------------------------------------- 2. 差集 ---

/// 分片重传差集（resume 真机形态；spike 钉死 c 的驱动层复证）：
/// 裸 client 预置「3 片只传 1 片」的中止态（= spike probe-resume 的
/// 「中止」步——驱动公开面上无法确定性打断中途传输，裸 client 直构
/// 是等价的服务端状态构造器）→ 驱动重开同路径 → 只补缺片。观测面：
/// - **服务端对账**（K74「真 OSS 对账」形态）：中止态 list_parts ==
///   [(1, 5MiB)]；完成后会话被消费（本地会话文件清空）；
/// - **同 UploadId**：writer 的会话文件 upload_id 与裸 client 拿到的
///   ticket.upload_id 相等——re-request 模型经驱动的端到端钉；
/// - **只补缺片**（timing）：补 2 片的 write 显著短于对照全量 3 片
///   的 write（任务书明示的 timing 证据面）；
/// - **逐字回读**：续传产物全量读与源逐字（预算内恰一次）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_PAN123_TEST_TOKEN (see the module docs)"]
async fn live_resume_diff_set_server_accounting() {
    let sdir = tempfile::tempdir().expect("sessions tempdir");
    let params = live_params_with_sessions(Some(sdir.path().to_path_buf()));
    let driver = ck_pan123::factory(&params)
        .await
        .expect("connect the live backend");
    let tag = stamp();
    let base = RelPath::new(&format!("{E2E_DIR}/{tag}")).expect("base path");
    let chunk = 5 * MIB;
    let total = 2 * chunk + 4096; // 3 分片（尾片 4096B）
    let seed = rand_seed();
    let data = pattern(total as usize, seed);

    // 对照组：同尺寸异内容全量上传（3 片 PUT 的 timing 基线；uploads
    // 不耗下载额度）。种子翻转位保证与 data 恒异（同参撞 etag 会让
    // 裸 ticket 直接 Reuse）。
    let control = pattern(total as usize, seed ^ 0x5A5A_5A5A_DEAD_BEEF);
    let c_path = base.join("resume-ctrl.bin").expect("ctrl path");
    let t0 = std::time::Instant::now();
    let mut st = driver
        .writer(
            &c_path,
            &WriteHint {
                size: Some(total),
                ..Default::default()
            },
        )
        .await
        .expect("ctrl writer");
    st.write(&control).await.expect("ctrl write");
    let t_control = t0.elapsed();
    let entry_c = st.close().await.expect("ctrl close");
    assert_eq!(entry_c.size, total);
    let _ = entry_c;

    // 裸 client 预置中止态：upload_request → 只预签名片 1 → PUT 片 1。
    let base_entry = driver.stat(&base).await.expect("base entry");
    let parent = fid_of(&base_entry);
    let etag = format!("{:x}", Md5::digest(&data));
    let name = "resume-diff.bin";
    let ticket = match driver
        .client()
        .upload_request_file(parent, name, total, &etag, None)
        .await
        .expect("raw upload_request")
    {
        UploadRequestOutcome::Ticket(t) => *t,
        UploadRequestOutcome::Rapid { .. } => {
            panic!("expected a fresh Ticket, got a Reuse hit (content already server-side)")
        }
        UploadRequestOutcome::Conflict => panic!("expected a fresh Ticket, got a 5060 conflict"),
    };
    let urls = driver
        .client()
        .s3_repare_presign(&ticket, 1, 2)
        .await
        .expect("presign part 1 only");
    let url = urls.get(&1).expect("presigned url for part 1");
    let http = driver.client().transfer_http().clone();
    let resp = http
        .put(url)
        .header("content-length", chunk.to_string())
        .body(data[..chunk as usize].to_vec())
        .send()
        .await
        .expect("PUT part 1");
    assert!(resp.status().is_success(), "part 1 PUT: {}", resp.status());

    // 服务端对账：中止态恰好在册 [(1, 5MiB)]。
    match driver.client().s3_list_parts(&ticket).await.expect("list") {
        PartsOutcome::Parts(parts) => {
            assert_eq!(
                parts,
                vec![(1, chunk as i64)],
                "the aborted session holds part 1 only"
            );
        }
        PartsOutcome::SessionGone => panic!("fresh session must list its part"),
    }
    eprintln!("[resume] aborted state verified server-side: parts [(1, {chunk})]");

    // 本地会话记录（123-5 真机修正后的驱动模型 = **本地五元组优先**：
    // 同参重发只在零片时复用会话——分片已传后重发恒铸新会话，差集恢复
    // 必须沿本地记录走。真机测试构造「进程死亡后重开」形态：裸 client
    // 预置服务端态 + 这里落等价本地记录 = 驱动自身中断后重启的完整世界）。
    let rpath = base.join(name).expect("resume path");
    {
        let rec = UploadSession {
            bucket: ticket.bucket.clone(),
            key: ticket.key.clone(),
            upload_id: ticket.upload_id.clone(),
            storage_node: ticket.storage_node.clone(),
            up_file_id: ticket.up_file_id,
            size: total,
            parts: vec![1],
        };
        let skey = format!("{}|{total}|{etag}", rpath.as_str());
        let digest = Md5::digest(skey.as_bytes());
        let dir = sdir.path().join("pan123_state").join("sessions");
        tokio::fs::create_dir_all(&dir).await.expect("sessions dir");
        let file = dir.join(format!("{digest:x}.json"));
        tokio::fs::write(&file, serde_json::to_string(&rec).expect("serialize"))
            .await
            .expect("session record write");
    }

    // 驱动重开同路径：本地记录 → 旧 tuple 对账（同 UploadId）→ 只补
    // 2/3 → 每片即落。
    let t1 = std::time::Instant::now();
    let mut st = driver
        .writer(
            &rpath,
            &WriteHint {
                size: Some(total),
                ..Default::default()
            },
        )
        .await
        .expect("resume writer");
    st.write(&data).await.expect("resume write");
    let t_resume = t1.elapsed();
    assert!(
        t_resume < t_control,
        "resuming 2 missing parts ({t_resume:?}) must beat a fresh 3-part chain ({t_control:?})"
    );

    // 会话文件对账（write 后 close 前）：同 UploadId + 三片在册。
    let sessions = read_sessions(sdir.path());
    let rec = sessions
        .iter()
        .find(|s| s.upload_id == ticket.upload_id)
        .unwrap_or_else(|| panic!("no session record for upload_id — sessions: {sessions:?}"));
    assert_eq!(rec.parts, vec![1, 2, 3], "all three parts accounted");
    assert_eq!(rec.size, total);
    eprintln!(
        "[SUMMARY] resume|control_write={t_control:?}|resume_write={t_resume:?}|upload_id=same|parts=[1,2,3]"
    );

    let entry = st.close().await.expect("resume close");
    assert_eq!(entry.size, total);
    // 完成收尾：会话文件清空（finish_local——对照会话在各自 close 时已清）。
    assert!(
        read_sessions(sdir.path()).is_empty(),
        "close must drain the session store"
    );

    // 逐字回读（预算内恰一次全量）。
    let got = read_all(driver.reader(&entry.id, None).await.expect("reader")).await;
    assert_eq!(got, data, "resumed object is byte-exact");

    // 清理。
    let dir_entry = driver.stat(&base).await.expect("base entry");
    driver.delete(&dir_entry.id).await.expect("cleanup");
    assert!(driver.stat(&base).await.is_err(), "resume folder cleaned");
}

// ------------------------------------------------- 3. 往返 + 余量联动 ---

/// 上传往返逐字 + Range 窗口族 + **下载余量联动** + 布局面（rename/
/// 分页/并发/清理）——离线 conformance ①②③⑥⑧ 的真机腿。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_PAN123_TEST_TOKEN (see the module docs)"]
async fn live_roundtrip_ranges_traffic_and_layout() {
    let driver = ck_pan123::factory(&live_params())
        .await
        .expect("connect the live backend");
    let chunk = 5 * MIB;
    let tag = stamp();
    let base = RelPath::new(&format!("{E2E_DIR}/{tag}")).expect("base path");

    // ① 上传往返（跨块 2.4 块；timing 附带记录）。
    let total = 2 * chunk + 4096;
    let data = pattern(total as usize, rand_seed());
    let f = base.join("roundtrip.bin").expect("path");
    let t0 = std::time::Instant::now();
    let mut st = driver
        .writer(
            &f,
            &WriteHint {
                size: Some(total),
                ..Default::default()
            },
        )
        .await
        .expect("writer");
    st.write(&data).await.expect("write");
    let t_upload = t0.elapsed();
    let entry = st.close().await.expect("close");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, total);
    eprintln!("[SUMMARY] roundtrip|upload_write={t_upload:?}|size={total}");

    // 余量联动观测：全量下载前采样。
    let fid = fid_of(&entry);
    let before = traffic_remain(&driver, fid).await;

    // ② 逐字校验用开区间形态（`bytes=100-` 的驱动面：offset 起、EOF
    // 止——一次读同时钉开窗与 EOF 语义；预算内恰此一次全量）。
    let got = read_all(
        driver
            .reader(&entry.id, Some(Range::new(100, None).expect("open range")))
            .await
            .expect("open-ended reader"),
    )
    .await;
    assert_eq!(got.len(), (total - 100) as usize, "open window length");
    assert_eq!(got, data[100..], "byte-exact from offset 100 to EOF");

    // 余量联动观测（**滞后计账——真机实证**）：3s 步进轮询至多 15s 记
    // 录移动量。首轮真机 10.5MiB 下载计数器即动（Δ14,710B），次轮 30s
    // 内不动——计数器异步/粗粒度更新（spike 上午样本同形态：24MiB 下
    // 载 → 数小时后 Δ16.8MB），「下载即时扣减」不成立。硬断言只保不
    // 增（余量单调不减的 sanity 面），移动量作为观测证据打印。
    let mut after = traffic_remain(&driver, fid).await;
    for _ in 0..5 {
        if after < before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        after = traffic_remain(&driver, fid).await;
    }
    assert!(
        after <= before,
        "the remain counter must never grow from a download"
    );
    eprintln!(
        "[SUMMARY] traffic_linkage|before={before}|after={after}|moved_within_15s={}|downloaded={total}",
        before - after
    );

    // ② 中段窗口。
    let s = total / 4;
    let got = read_all(
        driver
            .reader(
                &entry.id,
                Some(Range::new(s, Some(s + 1024)).expect("range")),
            )
            .await
            .expect("range reader"),
    )
    .await;
    assert_eq!(got, data[s as usize..s as usize + 1024]);

    // ② 尾窗 + EOF 钳制（越界上界被钳到文件尾）。
    let got = read_all(
        driver
            .reader(
                &entry.id,
                Some(Range::new(total - 4096, Some(total + chunk)).expect("range")),
            )
            .await
            .expect("clamped reader"),
    )
    .await;
    assert_eq!(got, data[(total - 4096) as usize..], "end clamps to EOF");

    // ⑥ rename 文件 + 目录（同父改名）。
    let f2 = base.join("roundtrip-moved.bin").expect("path");
    driver.rename(&f, &f2).await.expect("rename file");
    assert!(driver.stat(&f).await.is_err(), "old path is gone");
    let moved = driver.stat(&f2).await.expect("new path");
    assert_eq!(moved.size, total);

    let d = base.join("dtree").expect("dir");
    driver.mkdir(&d).await.expect("mkdir");
    let leaf = d.join("leaf.bin").expect("leaf");
    let mut st = driver
        .writer(
            &leaf,
            &WriteHint {
                size: Some(1024),
                ..Default::default()
            },
        )
        .await
        .expect("leaf writer");
    st.write(&pattern(1024, rand_seed()))
        .await
        .expect("leaf write");
    st.close().await.expect("leaf close");
    let d2 = base.join("dtree2").expect("dir2");
    driver.rename(&d, &d2).await.expect("rename dir");
    assert!(driver.stat(&leaf).await.is_err(), "old subtree is gone");
    assert!(
        driver
            .stat(&d2.join("leaf.bin").expect("leaf2"))
            .await
            .is_ok(),
        "subtree moved"
    );

    // ③ list 分页稳定有序（两轮一致）。
    let w1 = walk_paged(&driver, &base).await;
    let w2 = walk_paged(&driver, &base).await;
    assert_eq!(w1, w2, "stable pagination");
    assert!(w1.windows(2).all(|w| w[0] <= w[1]), "sorted");

    // ⑧ 并发读互不干扰（64KiB 小窗——流量预算纪律；干扰无关性断言
    // 面与骨架等价）。
    let (a, b) = futures_util::future::join(
        read_all(
            driver
                .reader(
                    &moved.id,
                    Some(Range::new(MIB, Some(MIB + 64 * 1024)).expect("r")),
                )
                .await
                .expect("c1"),
        ),
        read_all(
            driver
                .reader(
                    &moved.id,
                    Some(Range::new(2 * MIB, Some(2 * MIB + 64 * 1024)).expect("r")),
                )
                .await
                .expect("c2"),
        ),
    )
    .await;
    assert_eq!(a, data[MIB as usize..(MIB + 64 * 1024) as usize]);
    assert_eq!(b, data[(2 * MIB) as usize..(2 * MIB + 64 * 1024) as usize]);

    // ④ 清理：目录递归删除（trash 语义）+ 回读核空。
    let dir_entry = driver.stat(&base).await.expect("base entry");
    driver.delete(&dir_entry.id).await.expect("cleanup");
    assert!(driver.stat(&base).await.is_err(), "matrix folder cleaned");
}
