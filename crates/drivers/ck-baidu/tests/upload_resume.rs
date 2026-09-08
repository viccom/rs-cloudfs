//! 差集续传 / 会话生命周期（Batch B2 红②；K7 会话表 + 三路恢复）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照：spike §3 resume 实证 +
//! PCFS api.go:440-479 4-worker 形态）：
//!
//! - **会话表落盘**（K7）：`<sessions_dir>/baidu_state/sessions/<hash>.json`
//!   （path/size/block_md5/uploadid/完成位图，随分片完成即刻落盘）；
//!   `sessions_dir=None` 时纯内存会话仍支撑单进程内恢复（conformance ⑦
//!   形态），跨进程恢复由本套件 `Some(dir)` 钉死；
//! - **中断形态**（对齐 conformance ⑦）：stager 写部分数据后**直接 drop**
//!   （不 close 不 abort）——已 staging 完成的分片（4MiB 整块）是可复用
//!   资产，未完成块（含只写了几个字节的开头块）不算完成、必须重传；
//! - **差集**：同 sessions_dir 重建 driver（模拟进程重启）后 writer 恢复
//!   旧会话（旧 uploadid），第二次上传 mock **只收到缺失分片**——探活
//!   分片必须是缺失分片之一（不得重传已完成分片，spike §3
//!   old_uploadid_probe 形态）；
//! - **会话死亡兜底**：旧 uploadid 探活撞非 0 error_code → 整体重传
//!   （新 precreate 新 uploadid + 全量分片）——**重 precreate 不是恢复
//!   手段**（spike §3.2 实证：同参重发返回新 uploadid + 全量列表）；
//! - **abort 不清会话表**：stager 显式 abort 保留上传会话（服务端分片 +
//!   本地位图是可复用资产）；仅 create 成功（或路径/size/block_md5 不
//!   匹配）时会话作废；
//! - **秒传腿**（return_type=2）：precreate 直接返回 fs_id → superfile2/
//!   create 零调用直接收尾（官方语义存在但 spike 实测此 appkey 桶不
//!   触发——代码路径必须有、不依赖）。

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::{EntryKind, RelPath, StorageDriver, StorageError, WriteHint};
use futures_util::StreamExt;

use common::{pattern_bytes, MockBaidu, CHUNK_4M, MOCK_ROOT};

/// K7 会话表 JSON 落盘目录（任务契约形态 `<dir>/baidu_state/sessions/`）。
fn sessions_json_dir(sessions_root: &tempfile::TempDir) -> PathBuf {
    sessions_root.path().join("baidu_state").join("sessions")
}

/// 会话表 JSON 文件计数（K7 落盘形态的观测面）。
fn session_file_count(sessions_root: &tempfile::TempDir) -> usize {
    let dir = sessions_json_dir(sessions_root);
    match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count(),
        Err(_) => 0,
    }
}

/// 构造 driver（sessions_dir 注入——差集/重启腿的 K7 根）。
async fn driver_with_sessions(mock: &MockBaidu, sessions_dir: Option<PathBuf>) -> Arc<BaiduDriver> {
    factory(&mock.params_with(None, sessions_dir, None))
        .await
        .expect("driver connect")
}

/// 全量读回（ByteStream 消费——reader 契约在绿阶段接线）。
async fn read_all(driver: &BaiduDriver, entry: &cloudkit_storage::Entry) -> Vec<u8> {
    let mut stream = driver.reader(&entry.id, None).await.expect("reader 打开");
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(e) => panic!("读流中途错误: {e}"),
        }
    }
    out
}

fn hint_for(len: usize) -> WriteHint {
    WriteHint {
        size: Some(len as u64),
        ..Default::default()
    }
}

#[tokio::test]
async fn full_upload_roundtrips_and_invalidates_session_on_create() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let sessions_root = tempfile::tempdir().expect("sessions tempdir");
    let driver = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;

    // 2.5 分片数据（2×4MiB + 1MiB）→ 3 分片全部上传 → create → Entry。
    let data = pattern_bytes(2 * CHUNK_4M + CHUNK_4M / 4);
    let path = RelPath::new("full.bin").expect("rel path");
    let mut stager = driver
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("writer 打开");
    stager.write(&data).await.expect("write 全量");
    let entry = stager.close().await.expect("close 提交");

    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(entry.path, path);

    // wire 面收口：3 分片都传 + create 成功。
    let partseqs = mock.session_partseqs(&mock.uploadids()[0]);
    assert_eq!(partseqs, vec![0, 1, 2], "全量腿：3 分片都到后端");

    // stat 校验 size/kind（树侧真相）。
    let st = driver.stat(&path).await.expect("stat after close");
    assert_eq!(st.size, data.len() as u64);
    assert_eq!(st.kind, EntryKind::File);

    // 下载读回逐字节相等（上传→下载全链路对账）。
    let got = read_all(&driver, &entry).await;
    assert_eq!(got, data, "roundtrip 逐字节相等");

    // 生命周期契约：create 成功 → 会话作废（会话表清空）。
    assert_eq!(
        session_file_count(&sessions_root),
        0,
        "create 成功后会话表必须清空（会话资产随对象落定而终结）"
    );
}

#[tokio::test]
async fn dropped_stager_resumes_diff_only_after_driver_rebuild() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let sessions_root = tempfile::tempdir().expect("sessions tempdir");

    let data = pattern_bytes(3 * CHUNK_4M);

    // 第一段：写 1 整块 + 3 字节后 drop（conformance ⑦形态的中断——
    // 不 close 不 abort）。块 0 完整（完成）、块 1 只有 3 字节（未完成）。
    let path = RelPath::new("resume.bin").expect("rel path");
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("第一段 writer 打开");
        st1.write(&data[..CHUNK_4M + 3])
            .await
            .expect("第一段 write（1 整块+3字节）");
        drop(st1); // 中断
    }

    // 契约钉死：write 返回时**已写满的块已完成上传**（drop 后立即可观测
    // ——conformance ⑦ 在 drop 后立即取观测点，要求此确定性；「close 时
    // 统一传」或「in-flight 未落定」形态均不可通过差集上界）。
    let uploads = mock.uploadids();
    assert_eq!(uploads.len(), 1, "第一段恰一次 precreate（旧会话锚点）");
    assert_eq!(
        mock.session_partseqs(&uploads[0]),
        vec![0],
        "write 返回时块 0 已到达后端（写满即传、同步落定）"
    );

    // K7 落盘形态：中断后会话表里恰一个会话文件（随分片完成即刻落盘）。
    assert_eq!(
        session_file_count(&sessions_root),
        1,
        "中断后会话表恰一条会话记录（块完成即落盘）"
    );

    // 进程重启模拟：重建 driver（同 sessions_dir、同 mock）→ 差集续传。
    let mark = mock.bytes_received_total();
    let d2 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
    let mut st2 = d2
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("续传 writer 打开");
    st2.write(&data).await.expect("续传 write 全量");
    let entry = st2.close().await.expect("续传 close");
    assert_eq!(entry.size, data.len() as u64);

    // 差集断言：续传恰补块 1+块 2（探活分片是缺失分片之一且计入补传；
    // 不重传已完成块、不重 precreate）。
    assert_eq!(
        mock.uploadids().len(),
        1,
        "差集腿：旧会话恢复，不重 precreate（spike §3.2：重发≠恢复）"
    );
    let retransferred = mock.bytes_received_total() - mark;
    assert_eq!(
        retransferred,
        (2 * CHUNK_4M) as u64,
        "续传恰补传块 1+块 2（差集字节精确断言；块 0 复用、探活不额外重传）"
    );
    assert_eq!(
        mock.session_partseqs(&mock.uploadids()[0]),
        vec![0, 1, 2],
        "旧会话最终分片集 = 全集（块 0 中断前 + 块 1/2 续传补齐）"
    );

    // 收尾校验：内容正确 + create 成功 + 会话作废。
    let got = read_all(&d2, &entry).await;
    assert_eq!(got, data, "续传结果逐字节正确");
    assert_eq!(session_file_count(&sessions_root), 0, "close 后会话作废");
}

#[tokio::test]
async fn dead_session_falls_back_to_full_reupload_with_new_uploadid() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let sessions_root = tempfile::tempdir().expect("sessions tempdir");

    let data = pattern_bytes(3 * CHUNK_4M);
    let path = RelPath::new("dead.bin").expect("rel path");

    // 第一段：1 整块后 drop（旧会话建立 + 块 0 落位图）。
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("第一段 writer");
        st1.write(&data[..CHUNK_4M]).await.expect("第一段 write");
        drop(st1);
    }
    let old_uploadid = mock.uploadids()[0].clone();

    // 注入会话死亡：旧 uploadid 的下一分片（=探活分片）顶非 0 error_code。
    mock.inject_upload_death(&old_uploadid);

    // 重建 driver → 续传：探活撞死 → 整体重传（新 precreate 新 uploadid）。
    let d2 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
    let mut st2 = d2
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("兜底 writer");
    st2.write(&data).await.expect("兜底 write 全量");
    let entry = st2.close().await.expect("兜底 close");
    assert_eq!(entry.size, data.len() as u64);

    // 新会话签发 + 新会话收到全量分片（整体重传）+ create 走新 uploadid。
    let uploads = mock.uploadids();
    assert_eq!(
        uploads.len(),
        2,
        "会话死亡 → 新 precreate（重 precreate ≠ 恢复）"
    );
    assert_ne!(uploads[1], old_uploadid);
    assert_eq!(
        mock.session_partseqs(&uploads[1]),
        vec![0, 1, 2],
        "兜底腿：新会话全量分片重传"
    );

    // 内容正确（整体重传同样必须收尾正确）。
    let got = read_all(&d2, &entry).await;
    assert_eq!(got, data, "兜底重传结果逐字节正确");
}

#[tokio::test]
async fn explicit_abort_keeps_session_for_later_diff_resume() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let sessions_root = tempfile::tempdir().expect("sessions tempdir");

    let data = pattern_bytes(3 * CHUNK_4M);
    let path = RelPath::new("abort.bin").expect("rel path");

    // 显式 abort：不清会话表（上传会话是可复用资产——abort 只放弃本次
    // 暂存提交，不销毁服务端会话分片与本地位图）。
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("writer 打开");
        st1.write(&data[..CHUNK_4M]).await.expect("write 1 块");
        st1.abort().await.expect("显式 abort");
    }
    assert_eq!(
        session_file_count(&sessions_root),
        1,
        "abort 不清会话表（K7 资产保留——与 close 成功的作废语义相对）"
    );

    // abort 后仍可差集续传：重建 driver → 只补 {1,2}。
    let mark = mock.bytes_received_total();
    let d2 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
    let mut st2 = d2
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("abort 后续传 writer");
    st2.write(&data).await.expect("续传 write 全量");
    let entry = st2.close().await.expect("续传 close");
    assert_eq!(entry.size, data.len() as u64);

    let retransferred = mock.bytes_received_total() - mark;
    assert_eq!(
        retransferred,
        (2 * CHUNK_4M) as u64,
        "abort 后差集续传：恰补传块 1+块 2（块 0 复用）"
    );
    let got = read_all(&d2, &entry).await;
    assert_eq!(got, data, "abort 后续传结果逐字节正确");
}

#[tokio::test]
async fn rapid_upload_return_type2_short_circuits_superfile2_and_create() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let driver = driver_with_sessions(&mock, None).await;

    let abs = format!("{MOCK_ROOT}/rapid.bin");
    mock.mark_instant(&abs);

    let data = pattern_bytes(CHUNK_4M / 2);
    let path = RelPath::new("rapid.bin").expect("rel path");
    let mut stager = driver
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("writer 打开（秒传腿）");
    stager
        .write(&data)
        .await
        .expect("write（秒传腿照常 staging）");
    let entry = stager
        .close()
        .await
        .expect("close（precreate return_type=2 直接收尾）");

    // 秒传腿零分片流量：superfile2/create 全零调用，precreate 恰一次。
    assert_eq!(
        mock.superfile2_records().len(),
        0,
        "return_type=2 → superfile2 零调用"
    );
    let file_creates = mock
        .recorded()
        .iter()
        .filter(|r| r.http_method == "POST" && r.path == common::XPAN_FILE)
        .filter(|r| r.query.contains("method=create") && r.body.contains("isdir=0"))
        .count();
    assert_eq!(file_creates, 0, "return_type=2 → create 零调用");
    let precreates = mock
        .recorded()
        .iter()
        .filter(|r| r.query.contains("method=precreate"))
        .count();
    assert_eq!(precreates, 1, "秒传腿恰一次 precreate");

    // 直接收尾形态：Entry 即对象（stat 可见、size 正确）。
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, data.len() as u64);
    let st = driver.stat(&path).await.expect("秒传后 stat");
    assert_eq!(st.size, data.len() as u64);
}

#[tokio::test]
async fn writer_on_existing_directory_path_is_invalid() {
    // trait 契约（stager.rs/driver.rs）：目标路径是已存在目录 → Invalid。
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    mock.seed_dir(&format!("{MOCK_ROOT}/adir"));
    let driver = driver_with_sessions(&mock, None).await;

    let err = driver
        .writer(&RelPath::new("adir").expect("rel path"), &hint_for(10))
        .await
        .err()
        .expect("writer 到已存在目录必须报错");
    assert_eq!(
        err,
        StorageError::Invalid,
        "目标路径是已存在目录 → Invalid（trait 契约）"
    );
}
