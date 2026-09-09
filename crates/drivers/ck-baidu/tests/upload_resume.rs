//! 差集续传 / 会话生命周期（Batch B2 红②；K7 会话表 + 三路恢复）。
//!
//! 语义契约（断言即契约，实现者禁改；黄金参照：spike §3 resume 实证 +
//! PCFS api.go:440-479 4-worker 形态）：
//!
//! - **会话表落盘**（K7）：`<sessions_dir>/baidu_state/sessions/<hash>.json`
//!   （path/size/block_md5/uploadid/完成位图，随分片完成即刻落盘）；
//!   `sessions_dir=None` 时纯内存会话仍支撑单进程内恢复（conformance ⑦
//!   形态），跨进程恢复由本套件 `Some(dir)` 钉死；
//! - **「到齐即传」策略**（2026-09-08 真网 31363 实证驱动，B2 返工）：
//!   precreate 一次性锁定全量 block_list 且 create 必须原样重申——
//!   数据未到齐（累计字节 < hint.size）时 write **不做任何网络动作**
//!   （无 precreate、无分片、零后端流量）；到齐后 write 返回前完成
//!   全量 precreate + 串行传全部满块 + 位图落盘（write 同步落定契约
//!   保持——drop 后已传满块确定）；尾块（不满 4MiB 的最后一片）在
//!   close 时传；
//! - **中断形态**（对齐「到齐即传」）：stager write 全量数据（满块已
//!   传、尾块未传）后**直接 drop**（不 close 不 abort）——已上传的满块
//!   是可复用资产；未到齐 drop 则无会话无分片（见专测）；
//! - **差集**：同 sessions_dir 重建 driver（模拟进程重启）后 writer 恢复
//!   旧会话（旧 uploadid），第二次上传 mock **只收到缺失分片**（到齐
//!   drop 场景 = 恰尾块，0 满块重传）——探活分片必须是缺失分片之一
//!   （不得重传已完成分片，spike §3 old_uploadid_probe 形态）；
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

use common::{
    filter_recorded, parse_urlencoded, pattern_bytes, MockBaidu, CHUNK_4M, MOCK_ROOT, XPAN_FILE,
};

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

    // 3 满块 + 1MiB 尾块（尾块归 close——到齐即传只传满块）。
    let data = pattern_bytes(3 * CHUNK_4M + CHUNK_4M / 4);

    // 第一段：write 全量（数据到齐）后 drop（不 close 不 abort）——真网
    // 31363 实证驱动的场景改造：旧形态「写部分数据后 drop（已满块即传）」
    // 依赖部分声明 precreate，已被 31363 否定；新形态到齐后满块已传、
    // 尾块未传，差集断言语义保留（第二次只补缺失分片）。
    let path = RelPath::new("resume.bin").expect("rel path");
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("第一段 writer 打开");
        st1.write(&data)
            .await
            .expect("第一段 write 全量（到齐即传）");
        drop(st1); // 中断
    }

    // 契约钉死：write 返回时**到齐满块已完成上传**（drop 后立即可观测
    // ——write 同步落定契约保持）；尾块归 close，不在到齐时传。
    let uploads = mock.uploadids();
    assert_eq!(uploads.len(), 1, "第一段恰一次 precreate（旧会话锚点）");
    assert_eq!(
        mock.session_partseqs(&uploads[0]),
        vec![0, 1, 2],
        "write 到齐返回时满块 0/1/2 已到达后端（到齐即传、同步落定；尾块未传）"
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

    // 差集断言：续传 0 满块重传 + 恰补尾块（探活分片是缺失分片之一且
    // 计入补传；不重传已完成块、不重 precreate）。
    assert_eq!(
        mock.uploadids().len(),
        1,
        "差集腿：旧会话恢复，不重 precreate（spike §3.2：重发≠恢复）"
    );
    let retransferred = mock.bytes_received_total() - mark;
    assert_eq!(
        retransferred,
        (CHUNK_4M / 4) as u64,
        "续传恰补尾块（差集字节精确断言；3 满块复用、0 满块重传）"
    );
    assert_eq!(
        mock.session_partseqs(&mock.uploadids()[0]),
        vec![0, 1, 2, 3],
        "旧会话最终分片集 = 全集（满块 0/1/2 中断前 + 尾块 3 续传补齐）"
    );

    // 收尾校验：内容正确 + create 成功 + 会话作废。
    let got = read_all(&d2, &entry).await;
    assert_eq!(got, data, "续传结果逐字节正确");
    assert_eq!(session_file_count(&sessions_root), 0, "close 后会话作废");
}

/// 真网 31363 实证驱动（新模式钉死，2026-09-08 返工）：数据未到齐时
/// write **不做任何网络动作**——precreate 需一次性锁定全量 block_list，
/// 部分声明会话在 create 必然 31363。未到齐 drop → 无会话、无分片、零
/// 后端流量；随后正常全量上传不受影响。
#[tokio::test]
async fn partial_write_drop_stages_nothing_until_data_complete() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let sessions_root = tempfile::tempdir().expect("sessions tempdir");

    let data = pattern_bytes(3 * CHUNK_4M);
    let path = RelPath::new("partial.bin").expect("rel path");
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("第一段 writer 打开");
        // 1 整块 + 3 字节（未到齐：累计 < hint.size）。
        st1.write(&data[..CHUNK_4M + 3])
            .await
            .expect("第一段 write（未到齐）");
        drop(st1); // 中断
    }

    assert!(
        mock.uploadids().is_empty(),
        "未到齐不 precreate（31363：precreate 一次性锁定全量 block_list）"
    );
    assert_eq!(
        mock.superfile2_records().len(),
        0,
        "未到齐零分片上传（无会话可挂分片）"
    );
    assert_eq!(
        session_file_count(&sessions_root),
        0,
        "未到齐无会话资产（无 precreate 即无会话记录）"
    );
    assert_eq!(
        mock.bytes_received_total(),
        0,
        "未到齐零后端流量（本地 staging 不产生网络动作）"
    );

    // 后续正常路径不受影响：全量 writer → 到齐 → 全量 precreate → close。
    let d2 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
    let mut st2 = d2
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("全量 writer 打开");
    st2.write(&data).await.expect("全量 write（到齐）");
    let entry = st2.close().await.expect("全量 close");
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(mock.uploadids().len(), 1, "到齐才 precreate（恰一次）");
    assert_eq!(
        mock.session_partseqs(&mock.uploadids()[0]),
        vec![0, 1, 2],
        "到齐即传全量满块"
    );
    let got = read_all(&d2, &entry).await;
    assert_eq!(got, data, "全量上传结果逐字节正确");
}

#[tokio::test]
async fn dead_session_falls_back_to_full_reupload_with_new_uploadid() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let sessions_root = tempfile::tempdir().expect("sessions tempdir");

    // 3 满块 + 尾块：真网 31363 实证驱动的场景调整——到齐即传后 drop，
    // 恢复会话满块全在位图（无缺失满块），探活由 close 的尾块上传承担。
    let data = pattern_bytes(3 * CHUNK_4M + CHUNK_4M / 4);
    let path = RelPath::new("dead.bin").expect("rel path");

    // 第一段：write 全量（到齐，满块 0/1/2 已传）后 drop。
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("第一段 writer");
        st1.write(&data).await.expect("第一段 write 全量（到齐）");
        drop(st1);
    }
    let old_uploadid = mock.uploadids()[0].clone();

    // 注入会话死亡：旧 uploadid 的下一分片（=close 尾块上传，兼探活）顶
    // 非 0 error_code。
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
        vec![0, 1, 2, 3],
        "兜底腿：新会话全量分片重传（3 满块 + 尾块）"
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

    // 3 满块 + 尾块（真网 31363 实证驱动：到齐即传形态，abort 后差集 =
    // 恰尾块）。
    let data = pattern_bytes(3 * CHUNK_4M + CHUNK_4M / 4);
    let path = RelPath::new("abort.bin").expect("rel path");

    // 显式 abort：不清会话表（上传会话是可复用资产——abort 只放弃本次
    // 暂存提交，不销毁服务端会话分片与本地位图）。
    {
        let d1 = driver_with_sessions(&mock, Some(sessions_root.path().to_path_buf())).await;
        let mut st1 = d1
            .writer(&path, &hint_for(data.len()))
            .await
            .expect("writer 打开");
        st1.write(&data)
            .await
            .expect("write 全量（到齐，满块已传）");
        st1.abort().await.expect("显式 abort");
    }
    assert_eq!(
        session_file_count(&sessions_root),
        1,
        "abort 不清会话表（K7 资产保留——与 close 成功的作废语义相对）"
    );

    // abort 后仍可差集续传：重建 driver → 只补尾块（0 满块重传）。
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
        (CHUNK_4M / 4) as u64,
        "abort 后差集续传：恰补尾块（3 满块复用、0 满块重传）"
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

/// 真网 31300/31023 实证驱动（2026-09-08 第四轮返工，原
/// `close_survives_meta_propagation_delay_via_list_fallback` 改造）：此
/// appkey 下 meta 端点全废——close 的 Entry 构造不再有 meta 主路径，
/// **直接经父目录 list + fs_id 匹配**（list 即时可见——真网探针 #4/#5：
/// create 后 list 立即可见，meta 轮询 10s 不可见是持续无权限非延迟）。
/// 新契约：上传全程零 `method=meta` 流量；close 的 Entry 由父目录 list
/// 构造且句柄可回读。（mock 的 `fail_next_meta` 注入面保留——正式 appkey
/// 复测 meta 权限时的恢复面，驱动已不消费。）
#[tokio::test]
async fn close_builds_entry_via_parent_list_without_meta() {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let driver = driver_with_sessions(&mock, None).await;

    let data = pattern_bytes(CHUNK_4M + 11);
    let path = RelPath::new("delay.bin").expect("rel path");
    let mut stager = driver
        .writer(&path, &hint_for(data.len()))
        .await
        .expect("writer 打开");
    stager.write(&data).await.expect("write 到齐");
    let entry = stager.close().await.expect("close 构造 Entry");

    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, data.len() as u64);
    assert!(entry.mtime > 0.0, "mtime 为正 epoch 秒（后端入树时间戳）");
    // 新契约 wire 断言：全程零 meta 调用；Entry 经父目录 list 构造。
    let recorded = mock.recorded();
    let metas = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=meta"]);
    assert!(
        metas.is_empty(),
        "close 的 Entry 构造不经 meta（31300 停用）：{metas:?}"
    );
    let listed_parent = filter_recorded(&recorded, "GET", XPAN_FILE, &["method=list"])
        .into_iter()
        .any(|r| {
            parse_urlencoded(&r.query)
                .iter()
                .any(|(k, v)| k == "dir" && v == MOCK_ROOT)
        });
    assert!(
        listed_parent,
        "close 的 Entry 经父目录 list 构造（dir={MOCK_ROOT}）"
    );
    // list 构造的句柄有效：reader 可回读全量内容。
    let got = read_all(&driver, &entry).await;
    assert_eq!(got, data, "list 构造 Entry 的句柄可回读全量内容");
}
