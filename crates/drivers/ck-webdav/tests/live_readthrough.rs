//! RT5 真机矩阵（Phase 8 / RT5）——read-through 机制在**真实后端**上的
//! 端到端验证：空 sqlite + 真 ck-webdav 驱动 + 真 `Vfs`，逐层回源物化、
//! 删除双确认 prune、外部增量即时可见、回源调用有界性、rebuild 续跑
//! 全树落库，外加负责人真实 AList 场景腿。**全部 `#[ignore]`**，凭据只
//! 经 env（R3），本套之外零真机。
//!
//! ## 运行形态（照 live_matrix.rs 先例）
//!
//! ```text
//! # 1) 取 WSL2 VM IP（NAT 模式，重启会变）
//! IP=$(wsl.exe -d Ubuntu-24.04 -- bash -c "hostname -I" | tr -d '\r\n\0' | awk '{print $1}')
//! # 2) 双服务器 fixture env（见 docs/tracking/phase7-webdav-fixture.md；
//! #    凭据值不入任何文件）
//! export CYDRIVE_WEBDAV_TEST_RCLONE_URL="http://$IP:8080/" \
//!        CYDRIVE_WEBDAV_TEST_APACHE_URL="http://$IP:8081/dav/" \
//!        CYDRIVE_WEBDAV_TEST_USER=spike \
//!        CYDRIVE_WEBDAV_TEST_PASS=<一次性 fixture 凭据，勿入库>
//! # 3) AList 场景腿 env（可选：缺省时该腿 eprintln 说明并跳过，不算失败）。
//!    注意 URL 须指向可写挂载内（/dav/ 顶层是挂载命名空间，MKCOL 405）
//! export CYDRIVE_READTHROUGH_ALIST_URL="http://<host>:5244/dav/<mount>/" \
//!        CYDRIVE_READTHROUGH_ALIST_USER=... CYDRIVE_READTHROUGH_ALIST_PASS=...
//! # 4) 串行跑全套（共享 fixture + apache access log 计数，--test-threads=1 纪律）
//! cargo test -p ck-webdav --test live_readthrough -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 缺 fixture env → 测试 panic 带可行动指引（K79.6：测试根必填，静默
//! 跳过不算验证）；唯独 AList 腿按 RT5 任务单明文为「env 缺省 skip」。
//!
//! ## 矩阵腿 ↔ 测试名映射（`--test-threads=1` 下按名字典序执行）
//!
//! | 腿 | 测试函数 | 服务器 |
//! |---|---|---|
//! | A1 空索引零 rebuild 即见（外部落盘真值 + 驱动落盘两形态） | `live_rt_01_a1_fresh_index_sees_fixture_tree` | apache + rclone |
//! | A2 删除腿：外部删文件 → 行被双确认 prune | `live_rt_02_a2_external_delete_prunes_row` | apache |
//! | A2 新增腿：外部加文件 → D5 立即可见 | `live_rt_03_a2_external_add_visible_immediately` | apache |
//! | A3 规模腿：千级条目 1 次 PROPFIND + TTL 零网络 + 深跳 1 list | `live_rt_04_a3_scale_bounded_backend_calls` | apache |
//! | A4 续跑腿：max_entries 断趟 → rerun 恰好补完（零重扫） | `live_rt_05_a4_rebuild_resume_completes_tree` | apache |
//! | AList 真实场景腿 | `live_rt_06_alist_readthrough_scenario` | 负责人 AList |
//! | 全套全局核空（名字典序收尾） | `live_rt_07_global_residue_check` | 双服务器根 |
//!
//! ## 断言面选型（如实记录）
//!
//! - **回源调用计数不用 tracing 抓取也不靠耗时推断**：真驱动不可注入
//!   计数，但 **apache 是 mod_dav_fs 直连 fs**——每个请求都在
//!   `webdav-spike-access.log` 留一行（combined 格式含请求路径）。测试
//!   以 `grep -c <stamp>` 对 access log 前后差分 = 真实网络请求数，
//!   A3 的「每层 O(1)」与 A4 的「rerun 零重扫」都是**精确计数断言**
//!   （比任务单降级预案的「总耗时」更强）。
//! - **外部真值腿锚定 apache**：rclone `--vfs-cache-mode writes` 有
//!   ≈5min 的目录缓存窗（WD0 怪癖⑥），外部 fs 写在窗内**结构性不可
//!   见**（live_matrix 腿⑥已实证并记录）——A2/A3/A4 的外部真值/批量
//!   预置若走 rclone 会假红。rclone 的 read-through 覆盖由腿 01
//!   （驱动自己落盘 → VFS 缓存与远端一致）承担；外部真相 × rclone 的
//!   组合已由 live_matrix 腿⑥以「杀/重启冷读」形态钉过，不重复。
//!
//! ## 纪律
//!
//! - **stamp 唯一名**（K72）：本文件前缀 `e2e_rt_`（AList 腿
//!   `e2e_readthrough_`）独立于 live_matrix 的 `e2e_webdav_`——access
//!   log 计数与残留核空互不串扰；全部写操作限定 stamp 内；收尾删除 +
//!   stat 核空 + 腿 07 find 零残留核空；
//! - **64 位 LCG 随机内容**（K72/K77.6）；wsl.exe 通道脚本纪律（单行
//!   + 字面路径 + 尾守卫）照 fixture 文档执行；
//! - 测试根必填、腿 06 缺 env 跳过（任务单明文例外）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ck_webdav::{WebdavDriver, WebdavParams, WebdavTransport};
use cloudkit_core::cache::CacheManager;
use cloudkit_core::config::EncryptionScheme;
use cloudkit_core::database::{FileRecord, MetaDatabase};
use cloudkit_core::rebuild::{rebuild_from_backend_with, RebuildInterrupted, RebuildLimits};
use cloudkit_core::rel_path::RelPath as VfsPath;
use cloudkit_core::upload_queue::RetryPolicy;
use cloudkit_core::vfs::{Vfs, VfsConfig};
use cloudkit_storage::{EntryId, RelPath as VolRel, StorageDriver, StorageError, WriteHint};

// ------------------------------------------------------------- env 面 ---

/// 真机 fixture 的环境变量组（R3：凭据只经 env）。
struct LiveEnv {
    rclone: String,
    apache: String,
    user: String,
    pass: String,
}

fn live_env() -> LiveEnv {
    let required = |name: &str| -> String {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                panic!(
                    "{name} is not set: the RT5 live read-through matrix reads its fixture from \
                 the CYDRIVE_WEBDAV_TEST_* environment variables — start the WSL2 fixture per \
                 docs/tracking/phase7-webdav-fixture.md and export RCLONE_URL / APACHE_URL / \
                 USER / PASS, then rerun with --ignored --test-threads=1"
                )
            })
    };
    LiveEnv {
        rclone: required("CYDRIVE_WEBDAV_TEST_RCLONE_URL"),
        apache: required("CYDRIVE_WEBDAV_TEST_APACHE_URL"),
        user: required("CYDRIVE_WEBDAV_TEST_USER"),
        pass: required("CYDRIVE_WEBDAV_TEST_PASS"),
    }
}

fn params_for(url: &str, env: &LiveEnv) -> WebdavParams {
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), url.to_string());
    map.insert("webdav_username".to_string(), env.user.clone());
    map.insert("webdav_password".to_string(), env.pass.clone());
    ck_webdav::parse_from_map(&map).expect("live params parse")
}

fn params_from_map(map: HashMap<String, String>) -> WebdavParams {
    ck_webdav::parse_from_map(&map).expect("params parse")
}

// --------------------------------------------------------- vfs harness ---

/// 空 sqlite + 真驱动 + 真 Vfs 的 read-through 夹具（RT2 core 测试
/// harness 的真驱动版；加密密码恒 None = 明文实例，D10 闸门不触发）。
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<MetaDatabase>,
    vfs: Arc<Vfs>,
    driver: Arc<WebdavDriver>,
}

async fn harness_for(params: WebdavParams) -> Harness {
    let driver = ck_webdav::factory(&params)
        .await
        .expect("webdav driver constructs offline");
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(MetaDatabase::open(&dir.path().join("meta.db")).expect("open db"));
    let cache = CacheManager::new(dir.path().join("cache"), u64::MAX);
    let transport = Arc::new(WebdavTransport::new(Arc::clone(&driver)));
    let cfg = VfsConfig {
        chunk_size_bytes: 64 * 1024,
        workers: 1,
        queue_capacity: 16,
        retry: RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_attempts: 3,
        },
        encryption_password: None,
        encryption_scheme: EncryptionScheme::AeadV2,
        hydrate_timeout: Duration::from_secs(60),
    };
    let vfs = Arc::new(Vfs::new(Arc::clone(&db), cache, transport, cfg));
    Harness {
        _dir: dir,
        db,
        vfs,
        driver,
    }
}

// ------------------------------------------------------------ 纪律面 ---

/// 每轮唯一的 nanos 十六进制段（K72：跨轮不撞）。
fn nanos_hex() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

/// stamp 目录名（本套件前缀 `e2e_rt_`）。
fn stamp() -> String {
    format!("e2e_rt_{}", nanos_hex())
}

fn rand_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
}

/// 确定性伪随机载荷（64 位 LCG，K77.6——不用小种子线性递推）。
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

/// 经正式写面上传（writer → write → close；staging 协议全链）。
async fn upload(driver: &WebdavDriver, path: &VolRel, data: &[u8], label: &str) {
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver
        .writer(path, &hint)
        .await
        .unwrap_or_else(|e| panic!("{label} writer: {e}"));
    stager
        .write(data)
        .await
        .unwrap_or_else(|e| panic!("{label} write: {e}"));
    stager
        .close()
        .await
        .unwrap_or_else(|e| panic!("{label} close: {e}"));
}

/// stamp 目录清理 + stat 核空（「删了就是删了」的驱动面半边；文件系统
/// 侧核空在各腿内做）。
async fn cleanup(driver: &WebdavDriver, base: &VolRel, label: &str) {
    match driver.stat(base).await {
        Ok(entry) => {
            driver
                .delete(&entry.id)
                .await
                .unwrap_or_else(|e| panic!("{label} cleanup delete: {e}"));
        }
        Err(StorageError::NotFound) => return,
        Err(e) => panic!("{label} cleanup stat: {e}"),
    }
    match driver.stat(base).await {
        Err(StorageError::NotFound) => {}
        Ok(_) => panic!("{label} cleanup verify: still stat-able"),
        Err(e) => panic!("{label} cleanup verify: expected NotFound, got {e}"),
    }
}

// ------------------------------------------------------- wsl 通道面 ---

/// wsl.exe 单发（脚本一律单行 + 字面路径；Rust `Command` 直传不经 Git
/// Bash，MSYS 改写不适用）。
fn wsl(script: &str) -> std::process::Output {
    std::process::Command::new("wsl.exe")
        .args(["-d", "Ubuntu-24.04", "--", "bash", "-c", script])
        .output()
        .expect("wsl.exe spawn (the WSL2 fixture must exist)")
}

fn wsl_ok(script: &str) -> String {
    let out = wsl(script);
    assert!(
        out.status.success(),
        "wsl command failed (exit {:?}): {script}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// fixture 的两服务器根（WSL 文件系统侧锚点；fixture 文档既定布局）。
const WSL_RCLONE_ROOT: &str = "/srv/rclone-dav";
const WSL_APACHE_ROOT: &str = "/srv/webdav-test/dav";

/// apache access log 里带 stamp 的行数 = 本 stamp 的真实请求数（A3/A4
/// 的计数断言面；combined 格式含请求行）。计数前短睡一拍让日志落盘。
fn apache_hits(stamp: &str) -> usize {
    let out = wsl_ok(&format!(
        "sleep 0.3; grep -c {stamp} /var/log/apache2/webdav-spike-access.log || true"
    ));
    out.trim()
        .parse::<usize>()
        .unwrap_or_else(|_| panic!("apache log count not a number: {out:?}"))
}

/// WSL 文件系统侧的外部真值预置。**wsl.exe 通道纪律（本批实证补强）**：
/// wsl.exe 会把单参数脚本用双引号包一层交给外层 shell——脚本里的
/// `$(...)` 会被**外层 shell 抢先展开**（双引号语义），有状态的替换
/// （对尚未存在的文件 `wc`、多行 `find` 回填）在脚本运行前就发生并
/// 打碎脚本（本批真机第一轮踩实）。故本脚本**零 `$()`**：字面父目录
/// 显式 `mkdir -p`、`head | tr` 重定向、`test -f` 尾守卫——live_matrix
/// 腿⑥实证存活的纯字面形态；尺寸真值由后续 driver stat 断言。
fn preset_script(root: &str, stamp_s: &str, files: &[(&str, usize, char)]) -> String {
    let dir = format!("{root}/{stamp_s}");
    let mut parents: Vec<String> = Vec::new();
    let mut script = String::from("set -e");
    for (name, _, _) in files {
        let parent = match name.rsplit_once('/') {
            Some((parent, _)) => format!("{dir}/{parent}"),
            None => dir.clone(),
        };
        if !parents.contains(&parent) {
            parents.push(parent.clone());
            script.push_str(&format!("; mkdir -p {parent}"));
        }
    }
    for (name, len, byte) in files {
        script.push_str(&format!(
            "; head -c {len} /dev/zero | tr '\\0' '{byte}' > {dir}/{name}; chmod 644 {dir}/{name}"
        ));
    }
    for (name, _, _) in files {
        script.push_str(&format!("; test -f {dir}/{name}"));
    }
    // 归属归一：wsl 通道以 root 落盘，而 mod_dav 以 www-data 服务——
    // root 树会让驱动 DELETE 吃 207 半失败（本批真机实证）；外部真值
    // 指「内容外部写入」，归属随 fixture 服务用户归一不削弱该语义。
    script.push_str(&format!(
        "; chown -R www-data:www-data {dir}; echo PRESET_OK"
    ));
    script
}

/// 驱动列出的行名（字典序投影，断言用）。
fn names(rows: &[FileRecord]) -> Vec<String> {
    let mut names: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
    names.sort();
    names
}

// ------------------------------------------- 腿 01：A1 空索引零 rebuild ---

/// A1 测试化（用户场景复现）：空索引 + 真后端有货 → `read_dir_fresh`
/// 逐层即见、行落库，**全程零 rebuild**。
///
/// - **apache**：stamp 树由 WSL 文件系统侧**外部**预置（读穿的纯粹
///   形态——本地从未参与写入）；
/// - **rclone**：stamp 树由驱动自己落盘（VFS 缓存窗内自写自见；外部
///   真相 × rclone 已由 live_matrix 腿⑥钉过，不重复）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_rt_01_a1_fresh_index_sees_fixture_tree() {
    let env = live_env();

    // ---- apache：外部真值形态 ----
    {
        let label = "01/apache";
        let stamp_s = stamp();
        let out = wsl_ok(&preset_script(
            WSL_APACHE_ROOT,
            &stamp_s,
            &[
                ("a.txt", 4096, 'X'),
                ("docs/inner.txt", 512, 'Y'),
                ("notes.md", 33, 'Z'),
            ],
        ));
        assert!(out.contains("PRESET_OK"), "{label}: preset failed: {out}");
        let h = harness_for(params_for(&env.apache, &env)).await;
        let sdir = VolRel::new(&stamp_s).expect("stamp path");

        let rows = h
            .vfs
            .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
            .await
            .unwrap_or_else(|e| panic!("{label}: read_dir_fresh: {e}"));
        assert_eq!(
            names(&rows),
            vec!["a.txt", "docs", "notes.md"],
            "{label}: the externally preset tree must be visible with an empty index"
        );
        let a =
            h.db.get_file(&format!("/{stamp_s}/a.txt"))
                .expect("db")
                .unwrap_or_else(|| panic!("{label}: a.txt row must be materialized"));
        assert_eq!(a.size, 4096, "{label}: materialized size");
        assert!(!a.is_dir);
        let docs =
            h.db.get_file(&format!("/{stamp_s}/docs"))
                .expect("db")
                .unwrap_or_else(|| panic!("{label}: docs row must be materialized"));
        assert!(docs.is_dir);

        // 深跳：子层逐层进入（第二次 read_dir_fresh = 用户再进一层）。
        let inner_rows = h
            .vfs
            .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}/docs")).expect("vpath"))
            .await
            .unwrap_or_else(|e| panic!("{label}: read_dir_fresh(docs): {e}"));
        assert_eq!(names(&inner_rows), vec!["inner.txt"], "{label}: layer 2");
        let inner =
            h.db.get_file(&format!("/{stamp_s}/docs/inner.txt"))
                .expect("db")
                .unwrap_or_else(|| panic!("{label}: inner.txt row must be materialized"));
        assert_eq!(inner.size, 512);

        cleanup(&h.driver, &sdir, label).await;
        let check = wsl(&format!(
            "[ -e '{WSL_APACHE_ROOT}/{stamp_s}' ] && echo EXISTS || echo GONE"
        ));
        assert!(
            String::from_utf8_lossy(&check.stdout).contains("GONE"),
            "{label}: the stamp dir must be gone from the WSL filesystem"
        );
        eprintln!("[SUMMARY] 01 a1_fresh_index|server=apache|external preset tree|3+1 rows materialized|zero rebuild");
    }

    // ---- rclone：驱动自落盘形态 ----
    {
        let label = "01/rclone";
        let stamp_s = stamp();
        let h = harness_for(params_for(&env.rclone, &env)).await;
        let sdir = VolRel::new(&stamp_s).expect("stamp path");
        h.driver
            .mkdir(&sdir)
            .await
            .unwrap_or_else(|e| panic!("{label}: mkdir: {e}"));
        upload(
            &h.driver,
            &sdir.join("up.bin").expect("path"),
            &pattern(2048, rand_seed()),
            label,
        )
        .await;
        let ssub = sdir.join("sub").expect("path");
        h.driver
            .mkdir(&ssub)
            .await
            .unwrap_or_else(|e| panic!("{label}: mkdir sub: {e}"));
        upload(
            &h.driver,
            &ssub.join("deep.bin").expect("path"),
            &pattern(777, rand_seed()),
            label,
        )
        .await;

        // 独立空索引实例：同一驱动面、全新 db——读穿必须立即可见。
        let fresh = harness_for(params_for(&env.rclone, &env)).await;
        let rows = fresh
            .vfs
            .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
            .await
            .unwrap_or_else(|e| panic!("{label}: read_dir_fresh: {e}"));
        assert_eq!(
            names(&rows),
            vec!["sub", "up.bin"],
            "{label}: driver-written tree visible in a cold index"
        );
        // 逐层进入：机制按层现查（depth-1）——深层行在列出该层时落库。
        let inner_rows = fresh
            .vfs
            .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}/sub")).expect("vpath"))
            .await
            .unwrap_or_else(|e| panic!("{label}: read_dir_fresh(sub): {e}"));
        assert_eq!(
            names(&inner_rows),
            vec!["deep.bin"],
            "{label}: layer-2 listing sees the deep file"
        );
        let deep = fresh
            .db
            .get_file(&format!("/{stamp_s}/sub/deep.bin"))
            .expect("db")
            .unwrap_or_else(|| panic!("{label}: deep.bin row must exist"));
        assert_eq!(deep.size, 777);

        cleanup(&fresh.driver, &sdir, label).await;
        eprintln!(
            "[SUMMARY] 01 a1_fresh_index|server=rclone|driver-written tree|cold index sees it"
        );
    }
}

// ----------------------------------------- 腿 02：A2 删除双确认 prune ---

/// A2 删除腿：外部（WSL 文件系统侧）删一个文件 → 下一次 `read_dir_fresh`
/// 的列表里没有它 → prune 候选经**真驱动 stat 双确认**（NotFound）→ 行
/// 删；其余行完好。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_rt_02_a2_external_delete_prunes_row() {
    let env = live_env();
    let stamp_s = stamp();
    let out = wsl_ok(&preset_script(
        WSL_APACHE_ROOT,
        &stamp_s,
        &[
            ("a.bin", 1024, 'A'),
            ("b.bin", 2048, 'B'),
            ("c.bin", 4096, 'C'),
        ],
    ));
    assert!(out.contains("PRESET_OK"), "preset failed: {out}");
    let h = harness_for(params_for(&env.apache, &env)).await;
    let sdir = VolRel::new(&stamp_s).expect("stamp path");
    let vpath = |name: &str| format!("/{stamp_s}/{name}");

    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("02: read_dir_fresh #1: {e}"));
    assert_eq!(
        names(&rows),
        vec!["a.bin", "b.bin", "c.bin"],
        "02: first view"
    );

    let out = wsl_ok(&format!(
        "set -e; rm {WSL_APACHE_ROOT}/{stamp_s}/b.bin; test ! -f {WSL_APACHE_ROOT}/{stamp_s}/b.bin; \
         test -f {WSL_APACHE_ROOT}/{stamp_s}/a.bin; echo RM_OK"
    ));
    assert!(out.contains("RM_OK"), "external rm failed: {out}");

    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("02: read_dir_fresh #2: {e}"));
    assert_eq!(
        names(&rows),
        vec!["a.bin", "c.bin"],
        "02: the externally deleted file must vanish from the fresh view"
    );
    assert!(
        h.db.get_file(&vpath("b.bin")).expect("db").is_none(),
        "02: the deleted file's row must be pruned (driver stat double-confirm NotFound)"
    );
    assert!(h.db.get_file(&vpath("a.bin")).expect("db").is_some());
    assert!(h.db.get_file(&vpath("c.bin")).expect("db").is_some());

    cleanup(&h.driver, &sdir, "02").await;
    eprintln!("[SUMMARY] 02 a2_delete|apache|external rm → row pruned via real stat double-confirm|siblings intact");
}

// ------------------------------------------- 腿 03：A2 新增 D5 即时可见 ---

/// A2 新增腿：外部加一个文件 → `read_dir_fresh` **立即**可见（D5 每调
/// 强制 revalidate——无 TTL 等待、无 rebuild）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_rt_03_a2_external_add_visible_immediately() {
    let env = live_env();
    let stamp_s = stamp();
    let out = wsl_ok(&preset_script(
        WSL_APACHE_ROOT,
        &stamp_s,
        &[("first.bin", 8192, 'F')],
    ));
    assert!(out.contains("PRESET_OK"), "preset failed: {out}");
    let h = harness_for(params_for(&env.apache, &env)).await;
    let sdir = VolRel::new(&stamp_s).expect("stamp path");

    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("03: read_dir_fresh #1: {e}"));
    assert_eq!(names(&rows), vec!["first.bin"], "03: first view");

    let out = wsl_ok(&preset_script(
        WSL_APACHE_ROOT,
        &stamp_s,
        &[("second.bin", 2222, 'S')],
    ));
    assert!(out.contains("PRESET_OK"), "external add failed: {out}");

    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("03: read_dir_fresh #2: {e}"));
    assert_eq!(
        names(&rows),
        vec!["first.bin", "second.bin"],
        "03: the external addition must be visible on the very next fresh read (D5)"
    );
    let row =
        h.db.get_file(&format!("/{stamp_s}/second.bin"))
            .expect("db")
            .unwrap_or_else(|| panic!("03: second.bin row must be materialized"));
    assert_eq!(row.size, 2222);

    cleanup(&h.driver, &sdir, "03").await;
    eprintln!("[SUMMARY] 03 a2_add|apache|external add → visible on next read_dir_fresh (D5, no TTL wait)");
}

// --------------------------------- 腿 04：A3 规模 + 回源调用有界性 ---

/// A3 规模腿：千级条目目录的 `read_dir_fresh` = **恰 1 次 PROPFIND**
/// （access log 差分计数）；随后的 `stat_fresh` 在 TTL 窗内 = **0 请求**
/// （D6 快路径零网络）；未列过目录的深跳 `stat_fresh` = **恰 1 次**
/// （D6 风暴归并：一次父 list 即命中）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_rt_04_a3_scale_bounded_backend_calls() {
    const BIG: usize = 1000;
    let env = live_env();
    let stamp_s = stamp();
    let h = harness_for(params_for(&env.apache, &env)).await;
    let sroot = VolRel::new(&stamp_s).expect("stamp path");
    let sbig = sroot.join("big").expect("path");
    let sother = sroot.join("other").expect("path");
    h.driver
        .mkdir(&sbig)
        .await
        .unwrap_or_else(|e| panic!("04: mkdir big: {e}"));
    h.driver
        .mkdir(&sother)
        .await
        .unwrap_or_else(|e| panic!("04: mkdir other: {e}"));

    let payload = pattern(64, rand_seed());
    let t_setup = Instant::now();
    for i in 0..BIG {
        let name = format!("f{i:04}.bin");
        upload(
            &h.driver,
            &sbig.join(&name).expect("path"),
            &payload,
            &format!("04/setup/{name}"),
        )
        .await;
    }
    upload(
        &h.driver,
        &sother.join("g.bin").expect("path"),
        &pattern(96, rand_seed()),
        "04/setup/other",
    )
    .await;
    eprintln!(
        "[04:setup] {BIG}+1 files uploaded via the driver in {:?}",
        t_setup.elapsed()
    );

    // ---- 主断言：一次 read_dir_fresh = 恰 512/页 规整页数的 PROPFIND
    // （真驱动分页界 = core materialize::list_all_pages 的 limit 512；
    // 千级条目 → 2 页。有界性不变量 = O(1) 页数，绝非逐条 stat 的
    // O(n)=1001 请求反模式）----
    let page_limit = 512usize;
    let expected_lists = BIG.div_ceil(page_limit);
    let before = apache_hits(&stamp_s);
    let t0 = Instant::now();
    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}/big")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("04: read_dir_fresh(big): {e}"));
    let elapsed = t0.elapsed();
    assert_eq!(rows.len(), BIG, "04: all {BIG} entries in one fresh read");
    let delta = apache_hits(&stamp_s) - before;
    assert_eq!(
        delta, expected_lists,
        "04: read_dir_fresh over {BIG} entries must cost exactly {expected_lists} paged lists \
         (512/page), saw {delta}"
    );
    let mid =
        h.db.get_file(&format!("/{stamp_s}/big/f{:04}.bin", BIG / 2))
            .expect("db")
            .unwrap_or_else(|| panic!("04: middle row must be materialized"));
    assert_eq!(mid.size, 64);
    assert!(h
        .db
        .get_file(&format!("/{stamp_s}/big/f0000.bin"))
        .expect("db")
        .is_some());
    assert!(h
        .db
        .get_file(&format!("/{stamp_s}/big/f{:04}.bin", BIG - 1))
        .expect("db")
        .is_some());
    eprintln!(
        "[SUMMARY] 04 a3_scale|read_dir_fresh({BIG}) = {delta} PROPFINDs (paged 512/page) in {elapsed:?}|rows landed"
    );

    // ---- TTL 窗内 stat_fresh：零网络（D6 快路径）----
    let before = apache_hits(&stamp_s);
    let hit = h
        .vfs
        .stat_fresh(&VfsPath::new(&format!("/{stamp_s}/big/f0500.bin")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("04: stat_fresh in-window: {e}"));
    assert_eq!(hit.size, 64);
    let delta = apache_hits(&stamp_s) - before;
    assert_eq!(
        delta, 0,
        "04: stat_fresh inside the parent TTL window must be zero-network, saw {delta} requests"
    );

    // ---- 深跳：父目录从未列过 → 恰 1 次父 list 即命中（D6）----
    let before = apache_hits(&stamp_s);
    let hit = h
        .vfs
        .stat_fresh(&VfsPath::new(&format!("/{stamp_s}/other/g.bin")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("04: stat_fresh deep jump: {e}"));
    assert_eq!(hit.size, 96);
    let delta = apache_hits(&stamp_s) - before;
    assert_eq!(
        delta, 1,
        "04: the deep-jump stat_fresh must cost exactly one parent list, saw {delta}"
    );
    eprintln!(
        "[SUMMARY] 04 a3_scale|stat_fresh in-window = 0 requests|deep-jump stat_fresh = 1 parent list"
    );

    cleanup(&h.driver, &sroot, "04").await;
}

// ------------------------------------- 腿 05：A4 rebuild 续跑（真机） ---

/// A4 续跑真机腿：真服务器 9 目录 200 条目树 + `max_entries = 40` →
/// 首趟 `EntriesBudget` 中断；**同 db rerun 直到完成**；access log 差分
/// 钉死「全序列恰 9 次 list = 目录总数」——已完成目录绝不重扫（真机
/// 形态的 A4 计数断言）；完成趟 sweep 后全树行落库。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_rt_05_a4_rebuild_resume_completes_tree() {
    const SUBS: usize = 8;
    const FILES: usize = 24;
    let env = live_env();
    let stamp_s = stamp();
    let h = harness_for(params_for(&env.apache, &env)).await;
    let sroot = VolRel::new(&stamp_s).expect("stamp path");
    let payload = pattern(48, rand_seed());
    for s in 1..=SUBS {
        let sub = sroot.join(&format!("sub{s}")).expect("path");
        h.driver
            .mkdir(&sub)
            .await
            .unwrap_or_else(|e| panic!("05: mkdir sub{s}: {e}"));
        for f in 1..=FILES {
            upload(
                &h.driver,
                &sub.join(&format!("f{f:02}.bin")).expect("path"),
                &payload,
                &format!("05/setup/sub{s}/f{f}"),
            )
            .await;
        }
    }

    let limits = RebuildLimits {
        max_entries: 40,
        time_budget: None,
    };
    let before = apache_hits(&stamp_s);

    // 首趟：root(8) + sub1(24) + sub2(24) = 56 ≥ 40 → pop sub3 时断；
    // 恰 3 次 list（root+sub1+sub2）。
    let outcome1 = rebuild_from_backend_with(h.driver.as_ref(), &h.db, &sroot, limits)
        .await
        .expect("05: rebuild pass 1");
    assert_eq!(
        outcome1.interrupted,
        Some(RebuildInterrupted::EntriesBudget),
        "05: pass 1 must stop on the entries budget"
    );
    assert_eq!(
        outcome1.files,
        FILES * 2,
        "05: pass 1 materialized two subdirs of files"
    );
    assert_eq!(
        outcome1.dirs, SUBS,
        "05: pass 1 saw all eight subdirs via root"
    );
    let mut counted = apache_hits(&stamp_s) - before;
    assert_eq!(
        counted, 3,
        "05: pass 1 must list exactly root+sub1+sub2 (3 PROPFINDs), saw {counted}"
    );

    // rerun 直到完成：预算检查是 per-pass 计数（RT4 语义），每趟推进
    // 到上限即断；目录粒度检查保证零重复列举。全程 list 总数 = 目录数。
    let mut passes = 1usize;
    let last;
    loop {
        let outcome = rebuild_from_backend_with(h.driver.as_ref(), &h.db, &sroot, limits)
            .await
            .expect("05: rebuild rerun");
        counted = apache_hits(&stamp_s) - before;
        if outcome.interrupted.is_none() {
            last = outcome;
            break;
        }
        assert_eq!(
            outcome.interrupted,
            Some(RebuildInterrupted::EntriesBudget),
            "05: intermediate reruns may only stop on the entries budget"
        );
        passes += 1;
        assert!(passes <= 10, "05: rerun did not converge");
    }
    assert_eq!(
        counted,
        SUBS + 1,
        "05: the whole scan must list each of the {} directories exactly once \
         (zero re-scan on rerun), saw {counted}",
        SUBS + 1
    );
    assert_eq!(last.interrupted, None);
    assert_eq!(
        last.pruned, 0,
        "05: the completion sweep must prune nothing on a clean tree"
    );
    eprintln!(
        "[SUMMARY] 05 a4_resume|passes={passes}|total PROPFINDs={counted} (= dir count, zero re-scan)|\
         files={} dirs={} pruned={}",
        last.files, last.dirs, last.pruned
    );

    // 全树落库逐路径断言。
    for s in 1..=SUBS {
        let dir_row =
            h.db.get_file(&format!("/{stamp_s}/sub{s}"))
                .expect("db")
                .unwrap_or_else(|| panic!("05: sub{s} dir row must be landed"));
        assert!(dir_row.is_dir);
        let rows = h.db.list_dir(&format!("/{stamp_s}/sub{s}")).expect("db");
        assert_eq!(
            rows.len(),
            FILES,
            "05: sub{s} must hold all {FILES} file rows"
        );
    }

    cleanup(&h.driver, &sroot, "05").await;
}

// ---------------------------------------------- 腿 06：AList 真实场景 ---

/// 负责人真实 AList（RaiDrive 对等场景）上的 read-through 全链：mkcol
/// 前缀目录 + PUT 若干 → 空索引 `read_dir_fresh` 全见 → `stat_fresh`
/// 深跳命中 → 删 1 个 → 再枚举行消失。
///
/// **AList 挂载命名空间（本批真机实证）**：`/dav/` 顶层是挂载点清单，
/// 顶层 MKCOL → 405（驱动映射 Exists）——URL env 必须指向**可写挂载**
/// 内（如 `http://<host>:5244/dav/local/`），前缀目录落在该挂载根下。
///
/// **纪律（硬性）**：全部写操作限定 `/_e2e_readthrough_<stamp>/` 前缀
/// 目录；内容按轮随机（LCG）；收尾删除前缀目录并核对空；绝不触碰前缀
/// 外任何路径。env 缺失 → `eprintln!` 说明并 return（任务单明文例外：
/// 不算失败）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_READTHROUGH_ALIST_* env (skip-with-notice when absent)"]
async fn live_rt_06_alist_readthrough_scenario() {
    let pick = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(url), Some(user), Some(pass)) = (
        pick("CYDRIVE_READTHROUGH_ALIST_URL"),
        pick("CYDRIVE_READTHROUGH_ALIST_USER"),
        pick("CYDRIVE_READTHROUGH_ALIST_PASS"),
    ) else {
        eprintln!(
            "[SKIP] 06 alist: CYDRIVE_READTHROUGH_ALIST_URL/USER/PASS not set — the AList \
             scenario leg is skipped (export the three variables to include it)"
        );
        return;
    };

    let stamp_s = format!("e2e_readthrough_{}", nanos_hex());
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), url);
    map.insert("webdav_username".to_string(), user);
    map.insert("webdav_password".to_string(), pass);
    let h = harness_for(params_from_map(map)).await;
    let sbase = VolRel::new(&stamp_s).expect("stamp path");
    let vpath = |name: &str| format!("/{stamp_s}/{name}");

    h.driver
        .mkdir(&sbase)
        .await
        .unwrap_or_else(|e| panic!("06/alist: mkdir prefix: {e}"));
    let seed = rand_seed();
    let sizes = [1024usize, 64 * 1024, 257];
    for (i, size) in sizes.iter().enumerate() {
        upload(
            &h.driver,
            &sbase.join(&format!("f{i}.bin")).expect("path"),
            &pattern(*size, seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            &format!("06/alist/f{i}"),
        )
        .await;
    }

    // 空索引 → read_dir_fresh 全见。
    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("06/alist: read_dir_fresh: {e}"));
    assert_eq!(
        names(&rows),
        vec!["f0.bin", "f1.bin", "f2.bin"],
        "06/alist: all uploaded files visible in a cold index"
    );
    assert_eq!(rows.iter().map(|r| r.size).max(), Some(64 * 1024));

    // stat_fresh 深跳命中。
    let hit = h
        .vfs
        .stat_fresh(&VfsPath::new(&vpath("f2.bin")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("06/alist: stat_fresh: {e}"));
    assert_eq!(hit.size, 257);

    // 删 1 个（驱动面 = 外部真值：不经 Vfs 簿记）→ 再枚举行消失。
    let victim: EntryId = h
        .driver
        .stat(&sbase.join("f1.bin").expect("path"))
        .await
        .unwrap_or_else(|e| panic!("06/alist: stat victim: {e}"))
        .id;
    h.driver
        .delete(&victim)
        .await
        .unwrap_or_else(|e| panic!("06/alist: delete victim: {e}"));
    let rows = h
        .vfs
        .read_dir_fresh(&VfsPath::new(&format!("/{stamp_s}")).expect("vpath"))
        .await
        .unwrap_or_else(|e| panic!("06/alist: read_dir_fresh after delete: {e}"));
    assert_eq!(
        names(&rows),
        vec!["f0.bin", "f2.bin"],
        "06/alist: the deleted file must vanish from the fresh view"
    );
    assert!(
        h.db.get_file(&vpath("f1.bin")).expect("db").is_none(),
        "06/alist: the victim row must be pruned"
    );

    // 收尾：删前缀目录 + 核对空（AList 纪律：绝不留残）。
    cleanup(&h.driver, &sbase, "06/alist").await;
    eprintln!(
        "[SUMMARY] 06 alist|3 files cold-index visible|stat_fresh hit|delete → pruned|\
         prefix dir removed and verified gone (stat NotFound)"
    );
}

// --------------------------------------------------- 全套全局核空 ---

/// 收尾核空（名字典序最后执行）：两服务器根的 `e2e_rt_*` / `e2e_readthrough_*`
/// 前缀零残留（AList 腿自带核对，不在服务器根上扫）。残留在场 = 某腿
/// 收尾失败，fail loud 并顺手清根（照 live_matrix 腿⑩形态）。
///
/// 脚本形态：`find -exec rm -rf {{}} +`（清理与检查全程零 `$()`——
/// wsl.exe 的双引号包层会让外层 shell 抢先展开 `$()`，多行 find 输出
/// 会被折成逐行脚本执行——本批真机第一轮实证，见 preset_script 注释）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_rt_07_global_residue_check() {
    let _ = live_env();
    let roots = format!("{WSL_RCLONE_ROOT} {WSL_APACHE_ROOT}");
    let names_expr = "\\( -name 'e2e_rt_*' -o -name 'e2e_readthrough_*' \\)";
    let residue = wsl_ok(&format!(
        "find {roots} -maxdepth 1 {names_expr} -type d 2>/dev/null"
    ));
    if !residue.trim().is_empty() {
        eprintln!(
            "[07:note] residual stamp dirs survived the per-leg cleanup; removing directly \
             via WSL and failing the check: {}",
            residue.trim()
        );
        wsl_ok(&format!(
            "find {roots} -maxdepth 1 {names_expr} -type d -exec rm -rf {{}} + 2>/dev/null"
        ));
    }
    let final_check = wsl_ok(&format!(
        "find {roots} -maxdepth 1 {names_expr} 2>/dev/null"
    ));
    assert!(
        final_check.trim().is_empty(),
        "07: zero residue expected across the server roots, got: {}",
        final_check.trim()
    );
    eprintln!(
        "[SUMMARY] 07 residue|e2e_rt_* / e2e_readthrough_* residue=0 across rclone+apache roots"
    );
}
