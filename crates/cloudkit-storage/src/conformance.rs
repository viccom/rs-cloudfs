//! conformance kit（foundation D9 / interfaces §6）。
//!
//! 每个驱动（含 mock 与真机 `#[ignore]` 版本）跑同一套语义断言；
//! **最小断言集 v1 共八条，Phase 2 手册可扩不可减**：
//!
//! 1. 上传往返（N 覆盖 0/1/跨块/多块，含 commit-on-close 不可见性）
//! 2. Range 语义（半开/越界钳制/start≥size 声明一致）
//! 3. stat/list（分页遍历完整稳定有序）
//! 4. mkdir/delete 幂等语义（驱动声明其一并恒定）
//! 5. 错误映射表回放断言
//! 6. rename（文件+目录）
//! 7. 断点续传差集（声明 RESUME 时；驱动层可观测断言）
//! 8. 并发读互不干扰
//!
//! 接入形态（≈一行）：实现 [`ConformanceHarness`] 后
//! `cloudkit_storage::conformance::assert_conforms(&harness).await;`
//! 或用 [`crate::conformance_suite`] 宏直接生成测试函数。
//! 未声明的可选能力对应断言自动跳过（当前仅⑦ RESUME；①–⑥⑧属基础
//! trait 义务，永不跳过）。
//!
//! 套件自身通过 `panic!` 报告失败（消息带 ①–⑧ 编号），供测试框架捕获。

use async_trait::async_trait;

use crate::capability::Capabilities;
use crate::driver::StorageDriver;
use crate::error::StorageError;
use crate::ids::BackendHandle;
use crate::vocab::{ByteStream, Entry, EntryKind, Page, PageCursor, Range, RelPath, WriteHint};
use futures_util::StreamExt;

/// 断言⑤的回放条目：符号化后端错误码 → 期望的 [`StorageError`] 映射。
///
/// `backend_code` 对套件不透明（L2 运行时不认识任何具体后端错误码——R1）；
/// 由 harness 负责把码注入后端并由驱动映射。
#[derive(Debug, Clone)]
pub struct ErrorReplay {
    pub backend_code: String,
    pub expected: StorageError,
}

/// conformance 套件的驱动侧配套接口。
///
/// 驱动测试代码实现本 trait（离线版接 mock 后端，真机版接 `#[ignore]`
/// 流程）；默认值即最宽松声明，驱动按实情覆盖。
#[async_trait]
pub trait ConformanceHarness: Send + Sync {
    /// 被测驱动。
    fn driver(&self) -> &dyn StorageDriver;

    /// 后端存储分块边界（字节）。断言①的 N 覆盖与断言⑦的差集计算
    /// 依赖它；无分块驱动报 1。
    fn chunk_size(&self) -> u64 {
        1
    }

    /// 断言④声明：删除不存在的句柄 → `true` = 恒 `NotFound`，
    /// `false` = 恒幂等 `Ok(())`。驱动必须二选一并恒定。
    fn delete_missing_yields_not_found(&self) -> bool {
        true
    }

    /// 断言②声明：`range.start >= size` 时 → `true` = 空流，
    /// `false` = `NotFound`。
    fn empty_range_yields_empty_stream(&self) -> bool {
        true
    }

    /// 断言⑤声明：后端错误码 → StorageError 映射表（每驱动必配，
    /// interfaces §3）。空表 = 断言⑤空跑（仅 mock 之外无后端错误的
    /// local 类允许）。
    fn error_table(&self) -> Vec<ErrorReplay> {
        Vec::new()
    }

    /// 断言⑤注入：让被测驱动的下一次 `stat` 在**后端层**失败并返回
    /// `backend_code` 对应的原始错误（驱动须自行映射后抛出）。
    async fn inject_backend_error(&self, _backend_code: &str) {}

    /// 断言⑦观测点：后端至今实际收到的字节数（驱动层可观测）。
    /// 声明了 RESUME 却返回 `None` 视为不可验证 → 套件失败（R4：
    /// 声明即必须可验证）。
    async fn backend_bytes_received(&self) -> Option<u64> {
        None
    }
}

/// 运行完整八条断言（离线形态；真机 `#[ignore]` 版共享同一断言集）。
///
/// 失败即 panic，消息带 ①–⑧ 编号与上下文。
pub async fn assert_conforms(harness: &dyn ConformanceHarness) {
    let driver = harness.driver();
    let caps = driver.capabilities();
    let chunk = harness.chunk_size().max(1);
    assert_roundtrip(driver, chunk).await;
    assert_range_semantics(driver, harness, chunk).await;
    assert_stat_list(driver, chunk).await;
    assert_mkdir_delete(driver, harness, chunk).await;
    assert_error_replay(driver, harness, chunk).await;
    assert_rename(driver, chunk).await;
    assert_resume(driver, harness, caps, chunk).await;
    assert_concurrent_read(driver, chunk).await;
}

// --- 断言①：上传往返 ------------------------------------------------------

async fn assert_roundtrip(driver: &dyn StorageDriver, chunk: u64) {
    let parent = rp("conformance/a1");
    driver
        .mkdir(&parent)
        .await
        .unwrap_or_else(|e| panic!("① mkdir({parent}): {e}"));
    let sizes = [
        0u64,
        1,
        chunk.saturating_sub(1),
        chunk,
        chunk + 1,
        3 * chunk + 7,
    ];
    for &n in &sizes {
        let path = parent
            .join(&format!("f-{n}"))
            .unwrap_or_else(|e| panic!("① join: {e}"));
        let data = pattern(n as usize);
        let hint = WriteHint {
            size: Some(n),
            ..Default::default()
        };
        let mut stager = driver
            .writer(&path, &hint)
            .await
            .unwrap_or_else(|e| panic!("① writer({path}, n={n}): {e}"));
        // commit-on-close：close 前对象必须不可见
        match driver.stat(&path).await {
            Err(StorageError::NotFound) => {}
            Err(e) => panic!("① mid-staging stat({path}) 期望 NotFound，得到 {e}"),
            Ok(_) => panic!("① mid-staging stat({path}) 期望 NotFound，close 前对象已可见"),
        }
        stager
            .write(&data)
            .await
            .unwrap_or_else(|e| panic!("① write({path}, n={n}): {e}"));
        let entry = stager
            .close()
            .await
            .unwrap_or_else(|e| panic!("① close({path}, n={n}): {e}"));
        assert_eq!(entry.kind, EntryKind::File, "① kind({path})");
        assert_eq!(entry.size, n, "① size({path})");
        assert_eq!(entry.path, path, "① path({path})");
        assert_eq!(
            entry.id.volume,
            *driver.volume(),
            "① entry.id 必须带驱动自己的卷"
        );
        assert!(entry.mtime > 0.0, "① mtime({path}) 必须为正 epoch 秒");
        let bytes = read_all(
            driver
                .reader(&entry.id, None)
                .await
                .unwrap_or_else(|e| panic!("① reader({path}): {e}")),
        )
        .await;
        assert_eq!(bytes, data, "① roundtrip({path}, n={n}) 逐字节相等");
    }
}

// --- 断言②：Range 语义 ----------------------------------------------------

async fn assert_range_semantics(
    driver: &dyn StorageDriver,
    harness: &dyn ConformanceHarness,
    chunk: u64,
) {
    let size = 4 * chunk + 3;
    let path = rp("conformance/a2/ranges");
    let data = pattern(size as usize);
    let entry = upload(driver, &path, Some(size), &data, "②").await;
    let id = &entry.id;

    // 精确窗口 [s, e)
    let s = size / 4;
    let len = size / 4;
    let e = s + len;
    let got = read_range(driver, id, Range::new(s, Some(e)).unwrap()).await;
    assert_eq!(
        got,
        data[s as usize..e as usize],
        "② 半开区间 [{s},{e}) 恰好返回该窗口"
    );

    // 开放区间 [s, EOF)
    let got = read_range(driver, id, Range::new(s, None).unwrap()).await;
    assert_eq!(got, data[s as usize..], "② 开放区间 [{s},EOF) 返回后缀");

    // end 越界钳制到 EOF
    let got = read_range(driver, id, Range::new(s, Some(size + chunk)).unwrap()).await;
    assert_eq!(got, data[s as usize..], "② end 越界钳制到 EOF");

    // 空窗口 [s, s)
    let got = read_range(driver, id, Range::new(s, Some(s)).unwrap()).await;
    assert!(got.is_empty(), "② 空窗口 [{s},{s}) 返回空流");

    // start >= size：与驱动声明一致
    let empty_stream = harness.empty_range_yields_empty_stream();
    for start in [size, size + 1] {
        let range = Range::new(start, None).unwrap();
        let res = driver.reader(id, Some(range)).await;
        match (res, empty_stream) {
            (Ok(stream), true) => {
                let got = read_all(stream).await;
                assert!(
                    got.is_empty(),
                    "② start>=size 声明空流，start={start} 却得到 {} 字节",
                    got.len()
                );
            }
            (Err(StorageError::NotFound), false) => {}
            (Err(err), _) => panic!(
                "② start>=size (start={start}) 与声明不符（empty_stream={empty_stream}）：{err}"
            ),
            (Ok(_), false) => panic!(
                "② start>=size (start={start}) 声明 NotFound 却返回了流（empty_stream=false）"
            ),
        }
    }
}

// --- 断言③：stat / list 分页 ----------------------------------------------

async fn assert_stat_list(driver: &dyn StorageDriver, chunk: u64) {
    let a3 = rp("conformance/a3");
    let sub1 = a3.join("sub1").unwrap();
    driver
        .mkdir(&sub1)
        .await
        .unwrap_or_else(|e| panic!("③ mkdir({sub1}): {e}"));

    // 7 个根级文件 + 子目录 2 文件
    let mut expected: Vec<RelPath> = Vec::new();
    for i in 1..=7u64 {
        let p = a3.join(&format!("f{i}")).unwrap();
        upload(
            driver,
            &p,
            Some(i * chunk),
            &pattern((i * chunk) as usize),
            "③",
        )
        .await;
        expected.push(p);
    }
    for name in ["g1", "g2"] {
        let p = sub1.join(name).unwrap();
        upload(driver, &p, Some(chunk), &pattern(chunk as usize), "③").await;
    }
    expected.push(sub1.clone());
    expected.sort();

    // stat 字段正确
    for p in &expected {
        let st = driver
            .stat(p)
            .await
            .unwrap_or_else(|e| panic!("③ stat({p}): {e}"));
        assert_eq!(&st.path, p, "③ stat path 回显一致");
        assert!(st.mtime > 0.0, "③ stat({p}) mtime 为正");
        if st.kind == EntryKind::Dir {
            assert_eq!(st.size, 0, "③ 目录 size 为 0");
        }
    }
    let f3 = driver.stat(&a3.join("f3").unwrap()).await.unwrap();
    assert_eq!(f3.size, 3 * chunk, "③ stat size 正确");
    assert_eq!(f3.kind, EntryKind::File, "③ stat kind 正确");

    // 分页遍历：完整 + 稳定 + 有序
    let walk1 = walk(driver, &a3, 3).await;
    assert_eq!(walk1, expected, "③ 分页遍历完整且有序（limit=3）");
    let walk2 = walk(driver, &a3, 3).await;
    assert_eq!(walk1, walk2, "③ 分页遍历稳定（两次一致）");
    let walk3 = walk(driver, &a3, 2).await;
    assert_eq!(walk3, expected, "③ 分页遍历与页大小无关（limit=2）");
    let sub_children = walk(driver, &sub1, 1).await;
    assert_eq!(
        sub_children.len(),
        2,
        "③ 子目录遍历只含直接子条目（depth-1）"
    );
}

// --- 断言④：mkdir / delete 幂等 -------------------------------------------

async fn assert_mkdir_delete(
    driver: &dyn StorageDriver,
    harness: &dyn ConformanceHarness,
    chunk: u64,
) {
    let a4 = rp("conformance/a4");
    let x = a4.join("x").unwrap();
    driver
        .mkdir(&x)
        .await
        .unwrap_or_else(|e| panic!("④ mkdir({x}): {e}"));
    match driver.mkdir(&x).await {
        Err(StorageError::Exists) => {}
        res => panic!("④ mkdir 已存在必须 Exists，得到 {res:?}"),
    }

    // mkdir 隐式父目录
    let yz = a4.join("y").unwrap().join("z").unwrap();
    driver
        .mkdir(&yz)
        .await
        .unwrap_or_else(|e| panic!("④ mkdir 隐式父目录({yz}): {e}"));

    // delete 不存在：声明形态且恒定
    let missing = crate::ids::EntryId::new(
        driver.volume().clone(),
        BackendHandle::new("conformance-missing-handle"),
    );
    let not_found = harness.delete_missing_yields_not_found();
    for round in 1..=2u8 {
        let res = driver.delete(&missing).await;
        match (not_found, res) {
            (true, Err(StorageError::NotFound)) => {}
            (false, Ok(())) => {}
            (_, res) => panic!(
                "④ delete 不存在句柄第 {round} 次与声明（not_found={not_found}）不符：{res:?}"
            ),
        }
    }

    // delete 文件 → stat NotFound
    let f = a4.join("del.bin").unwrap();
    let entry = upload(driver, &f, Some(chunk), &pattern(chunk as usize), "④").await;
    driver
        .delete(&entry.id)
        .await
        .unwrap_or_else(|e| panic!("④ delete({f}): {e}"));
    match driver.stat(&f).await {
        Err(StorageError::NotFound) => {}
        res => panic!("④ 删除后 stat({f}) 必须 NotFound，得到 {res:?}"),
    }

    // delete 目录（递归）
    let dd = a4.join("dd").unwrap();
    let inner = dd.join("in").unwrap();
    upload(driver, &inner, Some(chunk), &pattern(chunk as usize), "④").await;
    let dir_entry = driver
        .stat(&dd)
        .await
        .unwrap_or_else(|e| panic!("④ stat({dd}): {e}"));
    assert_eq!(dir_entry.kind, EntryKind::Dir, "④ 目录 kind");
    driver
        .delete(&dir_entry.id)
        .await
        .unwrap_or_else(|e| panic!("④ delete 目录({dd}): {e}"));
    match driver.stat(&inner).await {
        Err(StorageError::NotFound) => {}
        res => panic!("④ 递归删除后 stat({inner}) 必须 NotFound，得到 {res:?}"),
    }
}

// --- 断言⑤：错误映射表回放 -------------------------------------------------

async fn assert_error_replay(
    driver: &dyn StorageDriver,
    harness: &dyn ConformanceHarness,
    chunk: u64,
) {
    let probe = rp("conformance/a5/probe");
    upload(driver, &probe, Some(chunk), &pattern(chunk as usize), "⑤").await;

    for replay in harness.error_table() {
        harness.inject_backend_error(&replay.backend_code).await;
        let err = match driver.stat(&probe).await {
            Err(e) => e,
            Ok(_) => panic!("⑤ 码 {} 回放后 stat 必须失败", replay.backend_code),
        };
        assert_eq!(
            err, replay.expected,
            "⑤ 码 {} 必须映射到期望的 StorageError",
            replay.backend_code
        );
        // 注入恰好一次：故障消费后恢复正常
        driver.stat(&probe).await.unwrap_or_else(|e| {
            panic!(
                "⑤ 码 {} 注入应恰好一次，后续 stat 又失败：{e}",
                replay.backend_code
            )
        });
    }
}

// --- 断言⑥：rename（文件 + 目录）-------------------------------------------

async fn assert_rename(driver: &dyn StorageDriver, chunk: u64) {
    let a6 = rp("conformance/a6");
    let dest = a6.join("dest").unwrap();
    driver
        .mkdir(&dest)
        .await
        .unwrap_or_else(|e| panic!("⑥ mkdir({dest}): {e}"));

    // 文件
    let src = a6.join("mv-src").unwrap();
    let dst = dest.join("mv-dst").unwrap();
    let content = pattern((2 * chunk + 5) as usize);
    upload(driver, &src, None, &content, "⑥").await;
    driver
        .rename(&src, &dst)
        .await
        .unwrap_or_else(|e| panic!("⑥ rename 文件({src} → {dst}): {e}"));
    match driver.stat(&src).await {
        Err(StorageError::NotFound) => {}
        res => panic!("⑥ rename 后旧路径必须 NotFound，得到 {res:?}"),
    }
    let entry = driver
        .stat(&dst)
        .await
        .unwrap_or_else(|e| panic!("⑥ stat({dst}): {e}"));
    assert_eq!(entry.size, content.len() as u64, "⑥ rename 后 size 不变");
    let got = read_all(
        driver
            .reader(&entry.id, None)
            .await
            .unwrap_or_else(|e| panic!("⑥ reader({dst}): {e}")),
    )
    .await;
    assert_eq!(got, content, "⑥ rename 后内容不变");

    // 目录（递归搬移）
    let dtree = a6.join("dtree").unwrap();
    let inner = dtree.join("inner").unwrap();
    let deep_leaf = dtree.join("deep").unwrap().join("leaf").unwrap();
    let c_inner = pattern((chunk + 1) as usize);
    let c_leaf = pattern((3 * chunk) as usize);
    upload(driver, &inner, None, &c_inner, "⑥").await;
    upload(driver, &deep_leaf, None, &c_leaf, "⑥").await;
    let dtree2 = a6.join("dtree2").unwrap();
    driver
        .rename(&dtree, &dtree2)
        .await
        .unwrap_or_else(|e| panic!("⑥ rename 目录({dtree} → {dtree2}): {e}"));
    match driver.stat(&dtree).await {
        Err(StorageError::NotFound) => {}
        res => panic!("⑥ rename 目录后旧路径必须 NotFound，得到 {res:?}"),
    }
    let e_inner = driver
        .stat(&dtree2.join("inner").unwrap())
        .await
        .unwrap_or_else(|e| panic!("⑥ stat({dtree2}/inner): {e}"));
    let e_leaf = driver
        .stat(&dtree2.join("deep").unwrap().join("leaf").unwrap())
        .await
        .unwrap_or_else(|e| panic!("⑥ stat({dtree2}/deep/leaf): {e}"));
    let got_inner = read_all(
        driver
            .reader(&e_inner.id, None)
            .await
            .unwrap_or_else(|e| panic!("⑥ reader inner: {e}")),
    )
    .await;
    let got_leaf = read_all(
        driver
            .reader(&e_leaf.id, None)
            .await
            .unwrap_or_else(|e| panic!("⑥ reader leaf: {e}")),
    )
    .await;
    assert_eq!(got_inner, c_inner, "⑥ 目录 rename 后子文件内容不变");
    assert_eq!(got_leaf, c_leaf, "⑥ 目录 rename 后深层子文件内容不变");
}

// --- 断言⑦：断点续传差集（声明 RESUME 时）----------------------------------

async fn assert_resume(
    driver: &dyn StorageDriver,
    harness: &dyn ConformanceHarness,
    caps: Capabilities,
    chunk: u64,
) {
    if !caps.resume {
        return; // 未声明 RESUME：自动跳过（能力位门控）
    }
    let Some(mark) = harness.backend_bytes_received().await else {
        panic!("⑦ 声明了 RESUME 却无法观测后端收到的字节数——R4：声明即必须可验证");
    };
    let total = 5 * chunk + 7;
    let staged = 3 * chunk + 4; // 3 个整块 + 半块
    let path = rp("conformance/a7/resume");
    let data = pattern(total as usize);
    let hint = WriteHint {
        size: Some(total),
        ..Default::default()
    };

    // 第一段上传：staged 字节后丢弃 stager（模拟中断）
    let mut st1 = driver
        .writer(&path, &hint)
        .await
        .unwrap_or_else(|e| panic!("⑦ writer 第一段: {e}"));
    st1.write(&data[..staged as usize])
        .await
        .unwrap_or_else(|e| panic!("⑦ 第一段 write: {e}"));
    drop(st1); // 不 close 不 abort：中断

    // 重新上传同路径：应只补未 staging 完成的块
    let mut st2 = driver
        .writer(&path, &hint)
        .await
        .unwrap_or_else(|e| panic!("⑦ writer 续传: {e}"));
    st2.write(&data)
        .await
        .unwrap_or_else(|e| panic!("⑦ 续传 write: {e}"));
    let entry = st2
        .close()
        .await
        .unwrap_or_else(|e| panic!("⑦ 续传 close: {e}"));
    assert_eq!(entry.size, total, "⑦ 续传后 size");
    let got = read_all(
        driver
            .reader(&entry.id, None)
            .await
            .unwrap_or_else(|e| panic!("⑦ reader: {e}")),
    )
    .await;
    assert_eq!(got, data, "⑦ 续传结果逐字节正确");

    let after = harness
        .backend_bytes_received()
        .await
        .expect("⑦ 观测点必须持续可用");
    let retransferred = after - mark;
    // 只有「未 staging 完成的块」允许重传：整块边界对齐后的差集是上界
    let fully_staged = staged / chunk * chunk;
    let allowed = total - fully_staged;
    assert!(
        retransferred <= allowed,
        "⑦ 重传 {retransferred} 字节超过差集上界 {allowed}（必须只补未完成的块）"
    );
    assert!(
        retransferred < total,
        "⑦ 重传 {retransferred} == 总量 {total}：没有任何 staging 被复用"
    );
}

// --- 断言⑧：并发读互不干扰 --------------------------------------------------

async fn assert_concurrent_read(driver: &dyn StorageDriver, chunk: u64) {
    let size = 3 * chunk + 11;
    let path = rp("conformance/a8/conc");
    let data = pattern(size as usize);
    let entry = upload(driver, &path, Some(size), &data, "⑧").await;

    let r1 = driver
        .reader(&entry.id, Some(Range::new(0, Some(chunk + 3)).unwrap()))
        .await
        .unwrap_or_else(|e| panic!("⑧ reader1: {e}"));
    let r2 = driver
        .reader(&entry.id, Some(Range::new(2 * chunk, None).unwrap()))
        .await
        .unwrap_or_else(|e| panic!("⑧ reader2: {e}"));

    // 两个 reader 并发推进（join 交替 poll），互不干扰
    let (a, b) = futures_util::future::join(read_all(r1), read_all(r2)).await;
    assert_eq!(a, data[..(chunk + 3) as usize], "⑧ 并发 reader1 精确窗口");
    assert_eq!(b, data[(2 * chunk) as usize..], "⑧ 并发 reader2 后缀窗口");
}

// --- 内部辅助 ---------------------------------------------------------------

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("套件内置路径必须合法")
}

fn pattern(n: usize) -> Vec<u8> {
    // 确定性伪随机字节（非全零/非递增，避免驱动偷懒匹配）
    let mut x = 0x2Fu8;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(31).wrapping_add(i as u8 + 7);
            x
        })
        .collect()
}

async fn upload(
    driver: &dyn StorageDriver,
    path: &RelPath,
    size_hint: Option<u64>,
    data: &[u8],
    label: &str,
) -> Entry {
    let hint = WriteHint {
        size: size_hint,
        ..Default::default()
    };
    let mut stager = driver
        .writer(path, &hint)
        .await
        .unwrap_or_else(|e| panic!("{label} writer({path}): {e}"));
    stager
        .write(data)
        .await
        .unwrap_or_else(|e| panic!("{label} write({path}): {e}"));
    stager
        .close()
        .await
        .unwrap_or_else(|e| panic!("{label} close({path}): {e}"))
}

async fn read_all(mut stream: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(e) => panic!("流中途错误: {e}"),
        }
    }
    out
}

async fn read_range(driver: &dyn StorageDriver, id: &crate::ids::EntryId, range: Range) -> Vec<u8> {
    let stream = driver
        .reader(id, Some(range))
        .await
        .unwrap_or_else(|e| panic!("② reader(range): {e}"));
    read_all(stream).await
}

async fn walk(driver: &dyn StorageDriver, dir: &RelPath, limit: usize) -> Vec<RelPath> {
    let mut paths = Vec::new();
    let mut cursor = PageCursor::Start;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard <= 10_000, "③ 分页遍历疑似不终止（游标回环？）");
        let listing: crate::vocab::Listing = driver
            .list(dir, Page { limit, cursor })
            .await
            .unwrap_or_else(|e| panic!("③ list({dir}): {e}"));
        for e in &listing.entries {
            paths.push(e.path.clone());
        }
        // 页内有序
        let mut sorted = true;
        for w in listing.entries.windows(2) {
            if w[0].path > w[1].path {
                sorted = false;
            }
        }
        assert!(sorted, "③ 页内必须按路径字典序有序");
        match listing.next {
            Some(next) => cursor = next,
            None => break,
        }
    }
    paths
}
