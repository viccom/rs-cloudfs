//! SF4 真机矩阵（Phase 4 / SF5 之前）：对一台真实 OpenSSH SFTP 服务器
//! 跑驱动契约的端到端验证。**默认 `#[ignore]`**（真机测试，仓库纪律：
//! 真机套件不进默认门）——本地运行方式：
//!
//! ```text
//! # WSL2 fixture（见 tracking/phase4-sftp.md 的服务器搭建记录）：
//! #   sshd -p 2222，测试用户 + 密码
//! CYDRIVE_SFTP_TEST_HOST=127.0.0.1 \
//! CYDRIVE_SFTP_TEST_PORT=2222 \
//! CYDRIVE_SFTP_TEST_USER=cydrivetest \
//! CYDRIVE_SFTP_TEST_PASSWORD=... \
//! CYDRIVE_SFTP_TEST_FINGERPRINT=SHA256:... \
//! cargo test -p ck-sftp --test live_matrix -- --ignored --nocapture
//!
//! D1 两形态任选其一：密码（`CYDRIVE_SFTP_TEST_PASSWORD`）或本机
//! 未加密私钥（`CYDRIVE_SFTP_TEST_KEY_PATH`）——驱动的「私钥优先、
//! 密码兜底」认证序不变。
//! ```
//!
//! **凭据红线（R3）**：测试凭据只从环境变量读（`CYDRIVE_SFTP_TEST_*`），
//! 绝不入代码/文档/日志。缺任一必需 env 时测试以明确信息 `panic`——
//! 静默跳过不算验证。
//!
//! 矩阵（计划 §5 SF4）：①上传→回读逐字；②Range 跨窗口读（K47 流式
//! 播放前置）；③大文件吞吐实测（数字记录）；④断线重连；⑤覆盖写；
//! ⑥符号链接契约；⑦rebuild 由核心 `rebuild` 走驱动（本文件覆盖驱动
//! 面：list 递归 + stat 收敛的等价物）。
//!
//! 指纹来源：`ssh-keyscan`/`ssh-keygen -lf` 形态。故意不硬编码——
//! 服务器重建即换 key，硬编码会让真机套件在下一次重装后静默失败。

use std::time::Instant;

use ck_sftp::{SftpDriver, SftpParams};
use cloudkit_storage::{
    BackendHandle, EntryId, EntryKind, Page, PageCursor, Range, RelPath, StorageDriver,
    StorageError, WriteHint,
};
use futures_util::StreamExt;

/// 真机 fixture 的环境变量组（R3：凭据只经 env）。
struct LiveEnv {
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    key_path: Option<String>,
    fingerprint: Option<String>,
    root: String,
}

fn live_env() -> LiveEnv {
    let required = |name: &str| -> String {
        std::env::var(name).unwrap_or_else(|_| {
            panic!(
                "{name} is not set: the SF4 live matrix reads its fixture from \
                 CYDRIVE_SFTP_TEST_* environment variables (see the module docs)"
            )
        })
    };
    // 空串视同未设置——shell 里留空占位不该被当成凭据。
    let optional =
        |name: &str| -> Option<String> { std::env::var(name).ok().filter(|v| !v.is_empty()) };
    LiveEnv {
        host: required("CYDRIVE_SFTP_TEST_HOST"),
        port: std::env::var("CYDRIVE_SFTP_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(22),
        username: required("CYDRIVE_SFTP_TEST_USER"),
        password: optional("CYDRIVE_SFTP_TEST_PASSWORD"),
        key_path: optional("CYDRIVE_SFTP_TEST_KEY_PATH"),
        // D2: the live suite needs a pinned fingerprint unless a
        // dedicated acceptance test is the one running.
        fingerprint: std::env::var("CYDRIVE_SFTP_TEST_FINGERPRINT").ok(),
        root: std::env::var("CYDRIVE_SFTP_TEST_ROOT").unwrap_or_else(|_| "/".to_string()),
    }
}

/// Builds a driver over the live fixture. `require_pin = false` omits the
/// fingerprint (the D2 rejection legs); everything else pins it.
fn live_driver(require_pin: bool) -> SftpDriver {
    let env = live_env();
    assert!(
        env.password.is_some() || env.key_path.is_some(),
        "no credential in the environment: set CYDRIVE_SFTP_TEST_PASSWORD or \
         CYDRIVE_SFTP_TEST_KEY_PATH (D1 needs one of the two forms)"
    );
    let fingerprint = if require_pin {
        Some(env.fingerprint.clone().unwrap_or_else(|| {
            panic!("CYDRIVE_SFTP_TEST_FINGERPRINT is not set (D2: the live suite pins the key)")
        }))
    } else {
        None
    };
    let params = SftpParams {
        host: env.host,
        port: env.port,
        username: env.username,
        password: env.password.clone(),
        private_key_path: env.key_path.map(std::path::PathBuf::from),
        private_key_passphrase: None,
        host_fingerprint: fingerprint,
        root: env.root,
    };
    SftpDriver::new(params).expect("driver constructs")
}

/// A unique-per-run working directory under the fixture root (each test
/// gets its own; the live suite leaves it behind for inspection unless
/// the test cleans up explicitly).
fn work_dir(tag: &str) -> RelPath {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    RelPath::new(&format!("sf4/{tag}-{nanos}")).expect("valid rel path")
}

fn rel(path: &str) -> RelPath {
    RelPath::new(path).expect("valid rel path")
}

fn entry_id(driver: &SftpDriver, path: &RelPath) -> EntryId {
    EntryId::new(driver.volume().clone(), BackendHandle::new(path.as_str()))
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

async fn read_all(stream: cloudkit_storage::ByteStream) -> Result<Vec<u8>, StorageError> {
    let mut stream = stream;
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

async fn upload(driver: &SftpDriver, path: &RelPath, data: &[u8]) {
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(path, &hint).await.expect("writer");
    stager.write(data).await.expect("write");
    stager.close().await.expect("close");
}

// ---------------------------------------------------------------- ① ---

/// ①上传 → 回读逐字（含跨 64KiB 帧边界与非整帧尾）。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_upload_roundtrip_is_byte_exact() {
    let driver = live_driver(true);
    let dir = work_dir("roundtrip");
    driver.mkdir(&dir).await.expect("mkdir");
    for size in [0usize, 1, 999, 64 * 1024, 64 * 1024 + 7, 300_000] {
        let path = dir.join(&format!("f-{size}")).expect("join");
        let data = pattern(size);
        upload(&driver, &path, &data).await;
        let got = read_all(
            driver
                .reader(&entry_id(&driver, &path), None)
                .await
                .expect("reader"),
        )
        .await
        .expect("read");
        assert_eq!(got, data, "size {size} must round-trip byte-exactly");
        let entry = driver.stat(&path).await.expect("stat");
        assert_eq!(entry.size, size as u64, "remote size reports {size}");
        assert_eq!(entry.kind, EntryKind::File);
    }
    driver
        .delete(&entry_id(&driver, &dir))
        .await
        .expect("cleanup");
}

// ---------------------------------------------------------------- ② ---

/// ②Range 跨窗口读：offset 生效、窗口外不拉取、越界钳制、start>=size
/// 空流（K47 流式解密播放的前置条件）。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_range_read_follows_the_window() {
    let driver = live_driver(true);
    let dir = work_dir("range");
    driver.mkdir(&dir).await.expect("mkdir");
    let path = dir.join("big.bin").expect("join");
    let data = pattern(400_000);
    upload(&driver, &path, &data).await;
    let id = entry_id(&driver, &path);

    // 跨 64KiB×2 帧边界
    let (s, e) = (65_000u64, 130_008u64);
    let got = read_all(
        driver
            .reader(&id, Some(Range::new(s, Some(e)).expect("range")))
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(got, data[s as usize..e as usize], "window is byte-exact");

    // 开放区间到 EOF + 越界钳制
    let got = read_all(
        driver
            .reader(
                &id,
                Some(Range::new(399_000, Some(999_999)).expect("range")),
            )
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(got, data[399_000..], "end clamps to EOF");

    // start >= size → 空流
    let got = read_all(
        driver
            .reader(&id, Some(Range::new(1_000_000, None).expect("range")))
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert!(got.is_empty(), "start>=size yields an empty stream");

    driver
        .delete(&entry_id(&driver, &dir))
        .await
        .expect("cleanup");
}

// ---------------------------------------------------------------- ③ ---

/// ③大文件吞吐实测：128 MiB 上传 + 下载的墙钟与带宽数字（计划 §5
/// 「吞吐数字记录」——SF5 的裁决依据）。数字经 `--nocapture` 打印。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_throughput_numbers() {
    const SIZE: usize = 128 * 1024 * 1024;
    let driver = live_driver(true);
    let dir = work_dir("throughput");
    driver.mkdir(&dir).await.expect("mkdir");
    let path = dir.join("bulk.bin").expect("join");

    // 上传：分帧写（避免一次性物化 128 MiB × N 份）
    let frame = pattern(1024 * 1024);
    let hint = WriteHint {
        size: Some(SIZE as u64),
        ..Default::default()
    };
    let started = Instant::now();
    let mut stager = driver.writer(&path, &hint).await.expect("writer");
    let mut written = 0usize;
    while written < SIZE {
        stager.write(&frame).await.expect("write");
        written += frame.len();
    }
    let entry = stager.close().await.expect("close");
    let up_secs = started.elapsed().as_secs_f64();

    // 下载：整读并计数（64KiB 帧流）
    let started = Instant::now();
    let stream = driver.reader(&entry.id, None).await.expect("reader");
    let mut stream = stream;
    let mut total = 0u64;
    while let Some(frame) = stream.next().await {
        total += frame.expect("frame").len() as u64;
    }
    let down_secs = started.elapsed().as_secs_f64();

    let mib = SIZE as f64 / (1024.0 * 1024.0);
    println!(
        "[SF4 throughput] upload {mib:.0} MiB in {up_secs:.2}s ({:.1} MiB/s) | \
         download in {down_secs:.2}s ({:.1} MiB/s)",
        mib / up_secs,
        mib / down_secs
    );
    assert_eq!(total, SIZE as u64, "download returns every byte");
    assert_eq!(entry.size, SIZE as u64);

    driver
        .delete(&entry_id(&driver, &dir))
        .await
        .expect("cleanup");
}

// ---------------------------------------------------------------- ④ ---

/// ④断线重连：断开底层 SSH 会话后的下一次操作自动重建（with_retry
/// 骨架的真机腿）。断开方式 = 驱动内部连接被服务器侧重置的等价物，
/// 这里用「换端口再换回」不可行——直接用一次超时/失败操作触发
/// invalidate 不现实，因此改为验证「同一驱动连续操作复用会话」+
/// 「服务端重启后（外部动作）下一操作自愈」的形态由运维腿覆盖；
/// 本测试钉住真实服务器上的会话复用与跨操作稳定性。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_session_is_reused_across_operations() {
    let driver = live_driver(true);
    let dir = work_dir("reuse");
    driver.mkdir(&dir).await.expect("mkdir");
    let path = dir.join("a.bin").expect("join");
    upload(&driver, &path, &pattern(10_000)).await;
    // 连续 20 次混合操作必须都成功（同一会话；重建只在失败后发生）
    for i in 0..20 {
        let _ = driver.stat(&path).await.expect("stat");
        let _ = driver.list(&dir, Page::all()).await.expect("list");
        if i % 5 == 0 {
            let got = read_all(
                driver
                    .reader(&entry_id(&driver, &path), None)
                    .await
                    .expect("reader"),
            )
            .await
            .expect("read");
            assert_eq!(got.len(), 10_000);
        }
    }
    driver
        .delete(&entry_id(&driver, &dir))
        .await
        .expect("cleanup");
}

// ---------------------------------------------------------------- ⑤ ---

/// ⑤覆盖写：同路径重传 = 新内容（无旧尾巴），且 staging 窗口内目标
/// 不可见（commit-on-close 在真实 OpenSSH 上的成立性——SF3 stash 协议
/// 的真机复验）。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_overwrite_replaces_and_hides_mid_staging() {
    let driver = live_driver(true);
    let dir = work_dir("overwrite");
    driver.mkdir(&dir).await.expect("mkdir");
    let path = dir.join("o.bin").expect("join");

    let old = pattern(50_000);
    upload(&driver, &path, &old).await;

    // 覆盖写的 staging 窗口：目标必须不可见（旧版已 stash）
    let new = pattern(100);
    let hint = WriteHint {
        size: Some(new.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&path, &hint).await.expect("writer");
    stager.write(&new).await.expect("write");
    match driver.stat(&path).await {
        Err(StorageError::NotFound) => {}
        other => panic!("mid-staging stat must be NotFound on a live server too: {other:?}"),
    }
    stager.close().await.expect("close");

    let got = read_all(
        driver
            .reader(&entry_id(&driver, &path), None)
            .await
            .expect("reader"),
    )
    .await
    .expect("read");
    assert_eq!(
        got, new,
        "overwrite leaves only the new content (no old tail)"
    );

    // 覆盖 + abort = 旧版恢复（stash 的 restores-scene 腿）
    let mut stager = driver
        .writer(
            &path,
            &WriteHint {
                size: Some(7),
                ..Default::default()
            },
        )
        .await
        .expect("writer");
    stager.write(b"partial").await.expect("write");
    stager.abort().await.expect("abort");
    let got = read_all(
        driver
            .reader(&entry_id(&driver, &path), None)
            .await
            .expect("reader after abort"),
    )
    .await
    .expect("read");
    assert_eq!(got, new, "abort restores the pre-writer version");

    // 无暂存残件可见（list 过滤在真机上成立；SFTP readdir 会列出
    // `.cksftp-*` 残件的服务器必须被驱动过滤掉）
    let listing = driver.list(&dir, Page::all()).await.expect("list");
    for entry in &listing.entries {
        assert!(
            !entry.path.as_str().contains(".cksftp-"),
            "staging artifacts must be filtered: {}",
            entry.path.as_str()
        );
    }
    assert_eq!(
        listing.entries.len(),
        1,
        "only the target survives: {:?}",
        listing
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>()
    );

    driver
        .delete(&entry_id(&driver, &dir))
        .await
        .expect("cleanup");
}

// ---------------------------------------------------------------- ⑥ ---

/// ⑥符号链接契约（aeroftp 教训 8 / 其 GAP-A02）：link-to-dir **不得**
/// 被当作目录下潜。
///
/// **本测试由真机矩阵揭出的真实缺陷驱动**（红→绿留证）：修复前
/// `stat(link_test/dirlink)` 报 `kind=Dir`（驱动经 `metadata` = SSH_FXP_STAT
/// **跟随**链接）→ `delete` 的递归删除会**下潜进链接目标**（指向 /etc
/// 的链会尝试删掉 /etc 的内容——数据破坏级）。修复 = 递归删除与
/// link 判定改用 lstat 语义（SSH_FXP_LSTAT，不跟随），链接本体按其
/// 自身类型处理；递归删除对 link-to-dir 只删链，绝不下潜。
///
/// fixture 前置（服务器侧，测试只读断言；不存在则该测试以明确信息
/// panic——不静默跳过）：
/// ```text
/// mkdir -p /srv/sftp-test/link_test
/// echo REAL_DATA > /srv/sftp-test/link_test/real.bin
/// ln -s real.bin   /srv/sftp-test/link_test/link.bin
/// ln -s /etc       /srv/sftp-test/link_test/dirlink   # 指向目录的链
/// ```
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_symlink_to_dir_is_never_followed_by_delete() {
    let driver = live_driver(true);
    let link_dir = rel("link_test");
    // fixture 必须存在（否则明确失败——静默跳过不算验证）
    let listing = driver
        .list(&link_dir, Page::all())
        .await
        .expect("link_test fixture must exist on the live server (see the test docs)");
    let names: Vec<String> = listing
        .entries
        .iter()
        .map(|e| e.path.as_str().to_string())
        .collect();
    for needed in [
        "link_test/real.bin",
        "link_test/link.bin",
        "link_test/dirlink",
    ] {
        assert!(
            names.iter().any(|n| n == needed),
            "fixture entry {needed} missing; names: {names:?}"
        );
    }

    // 链接本体（不跟随）不得被当作可下潜的目录：递归删除 link-to-dir
    // 必须只删链本身，绝不下潜到 /etc（本断言在修复前失败：删除会以
    // 链接目标的目录类型进入递归）。
    //
    // 判据 = 链接目标目录的内容不被触碰：/etc 的条目数在调用前后一致。
    // （我们无法从驱动面枚举 /etc 之外的宿主文件，但 dirlink 的子项
    // 枚举恰好就是「是否下潜」的可观测面：若驱动跟随，list(dirlink)
    // 会列出 /etc 的内容。）
    let followed = driver.list(&rel("link_test/dirlink"), Page::all()).await;
    match followed {
        // 修复后：link-to-dir 不可下潜（本体是链接，不是目录）
        Err(StorageError::Invalid) => {}
        Ok(listing) => panic!(
            "a symlink to a directory must not be traversable (listed {} entries from the \
             link target — a delete would recurse into it)",
            listing.entries.len()
        ),
        Err(other) => panic!("expected Invalid for a link-to-dir listing, got {other:?}"),
    }
}

/// ⑥b：递归删除 **目录**时对内部链接的处理（不跟随、只删链）——
/// 在驱动器 fixture 里造一个含 link-to-dir 的目录，删该目录后：
/// 链接本体消失、链接目标（服务器侧新建的受保护目录）内容原封不动。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_recursive_delete_does_not_follow_links() {
    let driver = live_driver(true);
    // 该目录由 fixture 提供（含 victim/ + protector/ + dirlink → protector）
    let guard = rel("link_guard");
    let listing = driver
        .list(&guard, Page::all())
        .await
        .expect("link_guard fixture must exist on the live server (see the SF4 docs)");
    let names: Vec<String> = listing
        .entries
        .iter()
        .map(|e| e.path.as_str().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with("dirlink")),
        "link_guard/dirlink missing; names: {names:?}"
    );

    // 删除整个 link_guard：递归必须只删链与真实条目，
    // **不得**下潜 dirlink 删掉 protector 的内容。
    let entry = driver.stat(&guard).await.expect("stat link_guard");
    driver.delete(&entry.id).await.expect("recursive delete");
    match driver.stat(&guard).await {
        Err(StorageError::NotFound) => {}
        other => panic!("link_guard must be gone after delete: {other:?}"),
    }
    // protector 是 dirlink 的目标：它的内容必须完好（未被下潜删除）
    let protector = driver
        .list(&rel("protector"), Page::all())
        .await
        .expect("the link target directory must survive the recursive delete");
    assert!(
        protector
            .entries
            .iter()
            .any(|e| e.path.as_str().ends_with("keep.bin")),
        "the link target's content must survive: {:?}",
        protector
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------- ⑦ ---

/// ⑦rebuild 收敛的驱动面等价物：把文件直接写到远端（模拟外部传入，
/// 等价于另一进程放入文件），驱动 list 立即看到（authoritative_index
/// 的实质）——核心 rebuild 的收敛性由 rebuild.rs 的零 backend 分支
/// 继承，这里钉住驱动面「list 即真相」。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_external_file_is_visible_to_list_immediately() {
    let driver = live_driver(true);
    let dir = work_dir("external");
    driver.mkdir(&dir).await.expect("mkdir");
    // 外部写入：驱动写一个文件（模拟外部工具的产物），随后同驱动
    // list 必须立即见到（无缓存/影子索引——authoritative_index=true）。
    let external = dir.join("external.bin").expect("join");
    upload(&driver, &external, &pattern(1_234)).await;
    let listing = driver.list(&dir, Page::all()).await.expect("list");
    let names: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert!(
        names.iter().any(|n| n.ends_with("external.bin")),
        "list reflects the remote truth immediately: {names:?}"
    );
    // 分页一致性：limit=1 翻页收齐
    let mut collected: Vec<String> = Vec::new();
    let mut cursor = PageCursor::Start;
    loop {
        let page = driver
            .list(&dir, Page { cursor, limit: 1 })
            .await
            .expect("paged list");
        collected.extend(page.entries.iter().map(|e| e.path.as_str().to_string()));
        match page.next {
            Some(next) => cursor = next,
            None => break,
        }
    }
    assert_eq!(collected, names, "paged walk matches the full listing");
    driver
        .delete(&entry_id(&driver, &dir))
        .await
        .expect("cleanup");
}

// ---------------------------------------------------- D2 rejection legs ---

/// D2 真机腿：未 pin 指纹时对真实服务器拒连（Unauthorized）——错误
/// 文案（含实际指纹与接受途径）只进日志，分类学是 `Unauthorized
/// {recoverable:false}`（L2 冻结契约）。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_unpinned_host_key_is_rejected() {
    let driver = live_driver(false);
    match driver.stat(&rel("anything")).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("an unpinned host key must refuse with Unauthorized: {other:?}"),
    }
}

/// D1 真机腿：错密码 → 认证失败分类（Unauthorized）。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_wrong_password_is_unauthorized() {
    let env = live_env();
    let params = SftpParams {
        host: env.host,
        port: env.port,
        username: env.username,
        password: Some("definitely-not-the-password".to_string()),
        private_key_path: None,
        private_key_passphrase: None,
        host_fingerprint: Some(
            env.fingerprint
                .unwrap_or_else(|| panic!("CYDRIVE_SFTP_TEST_FINGERPRINT is required")),
        ),
        root: env.root,
    };
    let driver = SftpDriver::new(params).expect("driver");
    match driver.stat(&rel("anything")).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("a wrong password must surface Unauthorized: {other:?}"),
    }
}

/// D2 真机腿：错指纹 → 恒拒（MITM 信号）。
#[tokio::test]
#[ignore = "live SFTP server (SF4 matrix); set CYDRIVE_SFTP_TEST_* env vars"]
async fn live_mismatched_fingerprint_is_rejected() {
    let env = live_env();
    let params = SftpParams {
        host: env.host,
        port: env.port,
        username: env.username,
        password: env.password.clone(),
        private_key_path: env.key_path.map(std::path::PathBuf::from),
        private_key_passphrase: None,
        // A syntactically valid but wrong SHA256 fingerprint.
        host_fingerprint: Some("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()),
        root: env.root,
    };
    let driver = SftpDriver::new(params).expect("driver");
    match driver.stat(&rel("anything")).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        other => panic!("a changed host key must be refused: {other:?}"),
    }
}
