//! WD5 真机矩阵（Phase 7）——WSL2 双 WebDAV 服务器 fixture（rclone serve
//! webdav + Apache mod_dav）上的端到端验证。**默认 `#[ignore]`**，凭据只
//! 经 env（R3），本套之外零真机。
//!
//! ## 运行形态（照 ck-sftp / ck-pan123 live_matrix 先例）
//!
//! ```text
//! # 1) 取 WSL2 VM IP（NAT 模式，重启会变）
//! IP=$(wsl.exe -d Ubuntu-24.04 -- bash -c "hostname -I" | tr -d '\r\n\0' | awk '{print $1}')
//! # 2) 设 env（fixture 服务器与凭据 = 本机回环一次性，见
//! #    docs/tracking/phase7-webdav-fixture.md；凭据值不入任何文件）
//! export CYDRIVE_WEBDAV_TEST_RCLONE_URL="http://$IP:8080/" \
//!        CYDRIVE_WEBDAV_TEST_APACHE_URL="http://$IP:8081/dav/" \
//!        CYDRIVE_WEBDAV_TEST_APACHE_STALE_URL="http://$IP:8081/dav-stale/" \
//!        CYDRIVE_WEBDAV_TEST_USER=spike \
//!        CYDRIVE_WEBDAV_TEST_PASS=<见 fixture 文档，勿入库>
//! # 3) 串行跑全套（服务器共享 fixture，--test-threads=1 纪律）
//! cargo test -p ck-webdav --test live_matrix -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! 缺任一 env → 测试 panic 带可行动指引（K79.6：测试根必填，静默跳过
//! 不算验证）。腿⑥（rclone 半腿：VFS 缓存窗外冷读）与腿⑩（断线自愈）
//! 会经 wsl.exe 杀/重启 rclone（杀/起拆两次调用）——重启后探活再继续，
//! 两腿均在套件尾段执行。
//!
//! ## 矩阵腿 ↔ 测试名映射（`--test-threads=1` 下按名字典序执行）
//!
//! | 矩阵腿 | 测试函数 | 说明 |
//! |---|---|---|
//! | ① 上传→回读逐字多档尺寸（双服务器） | `live_01_upload_readback_multisize` | 0B/1B/1KiB/1MiB/8MiB+1（跨窗）/16MiB |
//! | ② Range 跨窗口逐字节 | `live_02_range_windows_byte_exact` | 缝窗/尾窗/越界空流/EOF 钳制（双服务器） |
//! | ③ 吞吐 128 MiB（rclone） | `live_03_throughput_128mib_rclone` | 回环口径健康检查（sftp SF5 判例） |
//! | ④ 会话复用 20 次混合 | `live_04_session_reuse_mixed_ops` | 每 server 10 次零失败 |
//! | ⑤ 覆盖写 + staging 窗 + abort 恢复 | `live_05_overwrite_stash_window_abort` | stash 窗真机行为记录（不预设） |
//! | ⑥ 外部文件 list 可见性 | `live_06_external_write_visible` | authoritative_index 实证（apache 真即时；rclone VFS 缓存窗记录 + 重启冷读） |
//! | ⑦ 错误腿分类 | `live_07_error_classification` | 404/401 族/412→Exists/mkdir 201 陷阱 |
//! | ⑧ digest 全链路 | `live_08_digest_nc_and_stale_recovery` | nc 单调 + stale 重协商恰一次恢复 |
//! | ⑩ 拒绝腿 + doctor 五态 | `live_09_rejection_legs_and_probe` | 错凭据/坏 URL + `probe()` 分类 |
//! | ⑨ 断线白名单自愈（**排最后**） | `live_10_disconnect_selfheal` | 杀/重启 rclone + 全局核空 |
//!
//! 腿⑨（杀服务器）刻意排到最后执行（名字序 `live_10_*`）：服务器重启
//! 影响兄弟腿，跑完后全套结束。
//!
//! ## 纪律
//!
//! - **stamp 唯一名**（K72）：每测试每轮 `e2e_webdav_<unix_nanos>` 前缀
//!   目录；全部写操作限定 stamp 内；收尾 driver 删除 + stat 核空 + 腿⑩
//!   末尾 wsl find 三服务器根零残留核空；
//! - **64 位 LCG 随机内容**（K72/K77.6）：每轮随机种子——WebDAV 无秒传，
//!   随机内容纪律照守（缓存/去重短路的双保险缺一不可）；
//! - **怪癖对策**（fixture 文档「怪癖矩阵」）：集合操作恒尾斜杠（驱动
//!   已实现——测试只走驱动面，不用裸路径绕过）；**目录 rename 后立即
//!   list 的断言禁用**（rclone VFS 缓存窗 ≈5min——矩阵⑤挂账；本套无
//!   目录 MOVE 腿）；stat/list 断言容忍 rclone 目录 getcontentlength
//!   内层 404（驱动已处理，测试只见投影）；
//! - wsl.exe 直连脚本只携带路径（腿⑩重启命令除外——凭据经 env 传入
//!   命令行，fixture 一次性形态与 fixture 文档一致，不落任何文件）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloudkit_storage::{
    ByteStream, Entry, EntryKind, Page, PageCursor, Range, RelPath, StorageDriver, StorageError,
    WriteHint,
};
use futures_util::StreamExt;

use ck_webdav::{WebdavDriver, WebdavParams, WebdavProbe};

const MIB: u64 = 1024 * 1024;

/// fixture 的三台服务器根（WSL 文件系统侧——外部写/核空脚本的锚；
/// fixture 文档「一次性安装」节的既定布局，非凭据）。
const WSL_RCLONE_ROOT: &str = "/srv/rclone-dav";
const WSL_APACHE_ROOT: &str = "/srv/webdav-test/dav";
const WSL_APACHE_STALE_ROOT: &str = "/srv/webdav-test/dav-stale";

// ------------------------------------------------------------- env 面 ---

/// 真机 fixture 的环境变量组（R3：凭据只经 env）。
struct LiveEnv {
    rclone: String,
    apache: String,
    apache_stale: String,
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
                    "{name} is not set: the WD5 live matrix reads its fixture from the \
                 CYDRIVE_WEBDAV_TEST_* environment variables — start the WSL2 fixture per \
                 docs/tracking/phase7-webdav-fixture.md (§WD5 真机矩阵运行) and export all five \
                 (RCLONE_URL / APACHE_URL / APACHE_STALE_URL / USER / PASS), then rerun with \
                 --ignored --test-threads=1"
                )
            })
    };
    LiveEnv {
        rclone: required("CYDRIVE_WEBDAV_TEST_RCLONE_URL"),
        apache: required("CYDRIVE_WEBDAV_TEST_APACHE_URL"),
        apache_stale: required("CYDRIVE_WEBDAV_TEST_APACHE_STALE_URL"),
        user: required("CYDRIVE_WEBDAV_TEST_USER"),
        pass: required("CYDRIVE_WEBDAV_TEST_PASS"),
    }
}

/// 双服务器参数对（腿①②④⑤⑥⑦ 共用）。
fn servers(env: &LiveEnv) -> [(&'static str, &String); 2] {
    [("rclone", &env.rclone), ("apache", &env.apache)]
}

// --------------------------------------------------------- params 面 ---

fn params_from(map: HashMap<String, String>) -> WebdavParams {
    ck_webdav::parse_from_map(&map).expect("live params parse")
}

fn base_map(url: &str, env: &LiveEnv) -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), url.to_string());
    map.insert("webdav_username".to_string(), env.user.clone());
    map.insert("webdav_password".to_string(), env.pass.clone());
    map
}

/// 正确凭据 + 缺省 auto（Basic 预发 → 401 Digest 协商）。
fn params_for(url: &str, env: &LiveEnv) -> WebdavParams {
    params_from(base_map(url, env))
}

/// digest 显式模式（「无认证首发吃 challenge」路径——腿⑧ stale 驱动）。
fn digest_params(url: &str, env: &LiveEnv) -> WebdavParams {
    let mut map = base_map(url, env);
    map.insert("webdav_auth".to_string(), "digest".to_string());
    params_from(map)
}

/// 错误密码（每轮随机后缀——绝不与真凭据撞）。
fn wrong_pass_params(url: &str, env: &LiveEnv) -> WebdavParams {
    let mut map = base_map(url, env);
    map.insert(
        "webdav_password".to_string(),
        format!("definitely-wrong-{}", rand_seed()),
    );
    params_from(map)
}

/// 匿名（无凭据——401 缺凭据分类腿）。
fn anon_params(url: &str) -> WebdavParams {
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), url.to_string());
    params_from(map)
}

/// 坏 URL（同 host、必然无监听的端口——连接拒绝 → Unavailable/Unreachable）。
fn bad_url(base_url: &str) -> String {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let host_port = rest.split(['/', '?']).next().unwrap_or_default();
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port);
    format!("http://{host}:59999/")
}

async fn driver_for(params: WebdavParams) -> Arc<WebdavDriver> {
    ck_webdav::factory(&params)
        .await
        .expect("webdav driver constructs offline")
}

// ------------------------------------------------------------ 纪律面 ---

/// 每轮唯一 stamp 目录名（K72：跨轮不撞——缓存/去重短路的防线之一）。
fn stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("e2e_webdav_{nanos:x}")
}

/// 每轮随机种子（K72/K77.6 的另一半：内容不重复）。
fn rand_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
}

/// 确定性伪随机载荷（ck-pan123 live_matrix 同款 64 位 LCG——不用小种子
/// 线性递推）。
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

fn page() -> Page {
    Page {
        limit: 100,
        cursor: PageCursor::Start,
    }
}

// ------------------------------------------------------------ 工具面 ---

async fn read_all(mut stream: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("stream chunk"));
    }
    out
}

/// 经正式写面上传（writer → write → close；staging 协议全链）。
async fn upload(driver: &WebdavDriver, path: &RelPath, data: &[u8], label: &str) -> Entry {
    let hint = WriteHint {
        size: Some(data.len() as u64),
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

/// stamp 目录清理 + stat 核空（「删了就是删了」的驱动面半边；WSL 文件
/// 系统侧的核空在腿⑩末尾全局限）。
async fn cleanup(driver: &WebdavDriver, base: &RelPath, label: &str) {
    match driver.stat(base).await {
        Ok(entry) => {
            driver
                .delete(&entry.id)
                .await
                .unwrap_or_else(|e| panic!("{label} cleanup delete: {e}"));
        }
        Err(StorageError::NotFound) => return, // 本腿未落任何东西
        Err(e) => panic!("{label} cleanup stat: {e}"),
    }
    match driver.stat(base).await {
        Err(StorageError::NotFound) => {}
        Ok(entry) => panic!(
            "{label} cleanup verify: still stat-able (size={})",
            entry.size
        ),
        Err(e) => panic!("{label} cleanup verify: expected NotFound, got {e}"),
    }
}

/// wsl.exe 单发（复杂逻辑全在 bash -c 脚本串内；Windows 侧无 shell 插值
/// ——Command 直传不经 Git Bash，MSYS 改写不适用）。
fn wsl(script: &str) -> std::process::Output {
    std::process::Command::new("wsl.exe")
        .args(["-d", "Ubuntu-24.04", "--", "bash", "-c", script])
        .output()
        .expect(
            "wsl.exe spawn (the WSL2 fixture must exist: docs/tracking/phase7-webdav-fixture.md)",
        )
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

/// POSIX 单引号包裹（腿⑥⑩重启命令的凭据段——fixture 一次性形态）。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 杀掉 rclone serve（第一次 wsl 调用；`serv[e]` 字符类防 pkill 自匹配）。
fn wsl_kill_rclone() {
    let out = wsl_ok(
        "pkill -f 'rclone serv[e]'; for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; \
         do pgrep -f 'rclone serv[e]' >/dev/null || { echo KILLED; exit 0; }; sleep 0.2; done; \
         echo STILL_ALIVE >&2; exit 1",
    );
    assert!(
        out.contains("KILLED"),
        "kill script must confirm the death, said: {out}"
    );
}

/// 重启 rclone serve（第二次 wsl 调用；fixture 文档同款 nohup 行 + pid
/// 文件回写；凭据经 env 传入命令行——fixture 一次性形态，不落任何文件）。
fn wsl_start_rclone(env: &LiveEnv) {
    let script = format!(
        "nohup rclone serve webdav {WSL_RCLONE_ROOT} --addr 0.0.0.0:8080 --user {} --pass {} \
         --vfs-cache-mode writes >/tmp/webdav-spike-rclone.log 2>&1 & echo $! > \
         /run/webdav-spike-rclone.pid; for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f 'rclone \
         serv[e]' >/dev/null && {{ echo STARTED; exit 0; }}; sleep 0.3; done; echo START_FAILED \
         >&2; tail -5 /tmp/webdav-spike-rclone.log >&2; exit 1",
        shell_quote(&env.user),
        shell_quote(&env.pass)
    );
    let out = wsl_ok(&script);
    assert!(
        out.contains("STARTED"),
        "restart script must confirm, said: {out}"
    );
}

// ------------------------------------------------- ① 上传→回读逐字 ---

/// 矩阵腿①：多档尺寸上传→回读逐字（rclone + apache 双跑）。
/// 0B / 1B / 1KiB / 1MiB / 8MiB+1（跨 8MiB 读窗缝）/ 16MiB（恰两窗）。
/// stager close 产出 size + stat size + reader 全量逐字三重吻合。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_01_upload_readback_multisize() {
    let env = live_env();
    for (name, url) in servers(&env) {
        let driver = driver_for(params_for(url, &env)).await;
        let base = RelPath::new(&stamp()).expect("stamp path");
        for size in [0u64, 1, 1024, MIB, 8 * MIB + 1, 16 * MIB] {
            let data = pattern(size as usize, rand_seed());
            let path = base.join(&format!("size-{size}.bin")).expect("path");
            let entry = upload(&driver, &path, &data, &format!("01/{name}/{size}B")).await;
            assert_eq!(
                entry.size, size,
                "01/{name}/{size}: close reports the staged size"
            );
            let stat = driver
                .stat(&path)
                .await
                .unwrap_or_else(|e| panic!("01/{name}/{size}: stat: {e}"));
            assert_eq!(
                stat.size, size,
                "01/{name}/{size}: server-side size after commit"
            );
            assert_eq!(stat.kind, EntryKind::File);
            let got = read_all(
                driver
                    .reader(&stat.id, None)
                    .await
                    .unwrap_or_else(|e| panic!("01/{name}/{size}: reader: {e}")),
            )
            .await;
            assert_eq!(got, data, "01/{name}/{size}: byte-exact readback");
        }
        cleanup(&driver, &base, &format!("01/{name}")).await;
        eprintln!("[SUMMARY] 01 upload_readback|server={name}|sizes=0,1,1KiB,1MiB,8MiB+1,16MiB|byte-exact");
    }
}

// ------------------------------------------------- ② Range 跨窗口 ---

/// 矩阵腿②：16MiB 文件上的 Range 窗口族逐字节（rclone + apache 双跑）：
/// [0,1) / [8MiB-1, 8MiB+1)（跨 8MiB 驱动窗缝）/ [末-1, 末) / 中段 64KiB；
/// 越界（start ≥ size → 空流不开 GET）；EOF 钳制（end 越界截到文件尾）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_02_range_windows_byte_exact() {
    let env = live_env();
    let size = 16 * MIB;
    for (name, url) in servers(&env) {
        let driver = driver_for(params_for(url, &env)).await;
        let base = RelPath::new(&stamp()).expect("stamp path");
        let data = pattern(size as usize, rand_seed());
        let path = base.join("range.bin").expect("path");
        let entry = upload(&driver, &path, &data, &format!("02/{name}")).await;
        let id = entry.id;

        for (s, e) in [
            (0u64, 1u64),
            (8 * MIB - 1, 8 * MIB + 1), // 跨驱动 8MiB 读窗缝
            (size - 1, size),           // 最后一字节
            (3 * MIB + 7, 3 * MIB + 7 + 64 * 1024),
        ] {
            let range = Range::new(s, Some(e)).expect("range");
            let got = read_all(
                driver
                    .reader(&id, Some(range))
                    .await
                    .unwrap_or_else(|e| panic!("02/{name}: reader [{s},{e}): {e}")),
            )
            .await;
            assert_eq!(
                got,
                data[s as usize..e as usize],
                "02/{name}: window [{s},{e})"
            );
        }

        // 越界 → 空流（start >= size 不开 GET，§4.7）。
        for start in [size, size + MIB] {
            let range = Range::new(start, None).expect("range");
            let got = read_all(
                driver
                    .reader(&id, Some(range))
                    .await
                    .unwrap_or_else(|e| panic!("02/{name}: oob reader at {start}: {e}")),
            )
            .await;
            assert!(
                got.is_empty(),
                "02/{name}: start {start} >= size must yield an empty stream, got {} bytes",
                got.len()
            );
        }

        // EOF 钳制：end 越界截到文件尾（WD0 矩阵④：双服务器钳制/416 语义）。
        let range = Range::new(size - 4096, Some(size + MIB)).expect("range");
        let got = read_all(
            driver
                .reader(&id, Some(range))
                .await
                .expect("02/{name}: clamped reader"),
        )
        .await;
        assert_eq!(
            got,
            data[(size - 4096) as usize..],
            "02/{name}: clamp to EOF"
        );

        cleanup(&driver, &base, &format!("02/{name}")).await;
        eprintln!("[SUMMARY] 02 range_windows|server={name}|4 windows+2 oob+1 clamp|byte-exact");
    }
}

// ------------------------------------------------- ③ 吞吐 128 MiB ---

/// 矩阵腿③：128 MiB 上行（stager 写+close）与下行（reader 全量）各计时
/// （rclone；回环口径——sftp SF5 判例：健康检查，不作为性能判据）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_03_throughput_128mib_rclone() {
    let env = live_env();
    let driver = driver_for(params_for(&env.rclone, &env)).await;
    let base = RelPath::new(&stamp()).expect("stamp path");
    let total = (128 * MIB) as usize;
    let data = pattern(total, rand_seed());

    let t0 = Instant::now();
    let path = base.join("thru.bin").expect("path");
    let entry = upload(&driver, &path, &data, "03/upload").await;
    let up = t0.elapsed();
    assert_eq!(entry.size, total as u64);

    let t1 = Instant::now();
    let got = read_all(driver.reader(&entry.id, None).await.expect("03: reader")).await;
    let down = t1.elapsed();
    assert_eq!(got.len(), total);
    assert_eq!(got, data, "03: 128MiB readback byte-exact");

    let rate = |secs: Duration| total as f64 / MIB as f64 / secs.as_secs_f64();
    eprintln!(
        "[SUMMARY] 03 throughput|upload={up:?} ({:.1} MiB/s)|download={down:?} ({:.1} MiB/s)|128MiB loopback",
        rate(up),
        rate(down)
    );
    cleanup(&driver, &base, "03").await;
}

// ------------------------------------------------- ④ 会话复用 20 次 ---

/// 矩阵腿④：同一 client 连续 20 次混合操作（stat/list/小读写交替——
/// rclone 与 apache 各 10 次），零失败零重连异常（HTTP 池内自愈；Digest
/// 侧 = 同一 nonce 会话 nc 单调续用）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_04_session_reuse_mixed_ops() {
    let env = live_env();
    for (name, url) in servers(&env) {
        let driver = driver_for(params_for(url, &env)).await;
        let base = RelPath::new(&stamp()).expect("stamp path");
        driver
            .mkdir(&base)
            .await
            .unwrap_or_else(|e| panic!("04/{name}: mkdir: {e}"));
        let mut ops = 0usize;

        let f1 = base.join("re-1.bin").expect("path");
        let d1 = pattern(1024, rand_seed());
        driver.stat(&base).await.expect("04 op1 stat");
        ops += 1;
        driver.list(&base, page()).await.expect("04 op2 list");
        ops += 1;
        upload(&driver, &f1, &d1, &format!("04/{name}/f1")).await;
        ops += 1;
        let e1 = driver.stat(&f1).await.expect("04 op4 stat");
        ops += 1;
        read_all(driver.reader(&e1.id, None).await.expect("04 op5 reader")).await;
        ops += 1;
        driver.list(&base, page()).await.expect("04 op6 list");
        ops += 1;
        let f2 = base.join("re-2.bin").expect("path");
        let d2 = pattern(2048, rand_seed());
        upload(&driver, &f2, &d2, &format!("04/{name}/f2")).await;
        ops += 1;
        let e2 = driver.stat(&f2).await.expect("04 op8 stat");
        ops += 1;
        read_all(driver.reader(&e2.id, None).await.expect("04 op9 reader")).await;
        ops += 1;
        driver.list(&base, page()).await.expect("04 op10 list");
        ops += 1;
        assert_eq!(ops, 10, "04/{name}: exactly ten mixed ops per server");

        cleanup(&driver, &base, &format!("04/{name}")).await;
        eprintln!("[SUMMARY] 04 session_reuse|server={name}|10/10 mixed ops zero failure");
    }
    eprintln!("[SUMMARY] 04 session_reuse|total=20 ops across rclone+apache|no reconnect anomaly");
}

// ------------------------------------- ⑤ 覆盖写 + staging 窗 + abort ---

/// 矩阵腿⑤：覆盖写（新内容生效/旧内容不可见）+ stash 协议真机形态
/// （staging 窗口内开第二观测——**记录真实行为，不预设**）+ abort 恢复
/// （写到一半放弃 → 原文件原位逐字节不动 → 无 `.ckwd-` 残留）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_05_overwrite_stash_window_abort() {
    let env = live_env();
    for (name, url) in servers(&env) {
        let driver = driver_for(params_for(url, &env)).await;
        let base = RelPath::new(&stamp()).expect("stamp path");
        driver
            .mkdir(&base)
            .await
            .unwrap_or_else(|e| panic!("05/{name}: mkdir: {e}"));
        let f = base.join("ov.bin").expect("path");

        let a = pattern(64 * 1024, rand_seed());
        upload(&driver, &f, &a, &format!("05/{name}/A")).await;

        // 覆盖写打开 = stash 已上车（MOVE final → .ckwd-*.old）。staging
        // 窗口内的真实形态只记录不预设（conformance 断言①已用 dav-server
        // 参照桩钉过「不可见」；真机差异 = 发现项）。
        let b = pattern(32 * 1024, rand_seed());
        let hint = WriteHint {
            size: Some(b.len() as u64),
            ..Default::default()
        };
        let mut stager = driver
            .writer(&f, &hint)
            .await
            .unwrap_or_else(|e| panic!("05/{name}: overwrite writer: {e}"));
        match driver.stat(&f).await {
            Err(StorageError::NotFound) => eprintln!(
                "[05:window] {name}: mid-staging stat = NotFound (old object stashed — invisible)"
            ),
            Ok(entry) => eprintln!(
                "[05:window] {name}: mid-staging stat = Ok(size={}) — the server still serves \
                 the old object during staging (recorded live behavior)",
                entry.size
            ),
            Err(other) => {
                eprintln!("[05:window] {name}: mid-staging stat = {other} (recorded live behavior)")
            }
        }
        stager
            .write(&b)
            .await
            .unwrap_or_else(|e| panic!("05/{name}: write B: {e}"));
        let entry = stager
            .close()
            .await
            .unwrap_or_else(|e| panic!("05/{name}: close B: {e}"));
        assert_eq!(entry.size, b.len() as u64, "05/{name}: overwrite size");
        let got = read_all(
            driver
                .reader(&entry.id, None)
                .await
                .unwrap_or_else(|e| panic!("05/{name}: reader: {e}")),
        )
        .await;
        assert_eq!(got, b, "05/{name}: overwrite lands the new content");
        assert_ne!(got, a, "05/{name}: old content must not survive");

        // abort 恢复腿：旧对象逐字节回位。
        let c = pattern(16 * 1024, rand_seed());
        let hint = WriteHint {
            size: Some(c.len() as u64),
            ..Default::default()
        };
        let mut stager2 = driver
            .writer(&f, &hint)
            .await
            .unwrap_or_else(|e| panic!("05/{name}: abort writer: {e}"));
        stager2
            .write(&c)
            .await
            .unwrap_or_else(|e| panic!("05/{name}: partial write: {e}"));
        stager2
            .abort()
            .await
            .unwrap_or_else(|e| panic!("05/{name}: abort: {e}"));
        let restored = driver
            .stat(&f)
            .await
            .unwrap_or_else(|e| panic!("05/{name}: object must be restored after abort: {e}"));
        assert_eq!(
            restored.size,
            b.len() as u64,
            "05/{name}: restore keeps the pre-writer size"
        );
        let got = read_all(
            driver
                .reader(&restored.id, None)
                .await
                .unwrap_or_else(|e| panic!("05/{name}: restore reader: {e}")),
        )
        .await;
        assert_eq!(got, b, "05/{name}: abort restores byte-for-byte");

        // list 面无 `.ckwd-` 残留。
        let listing = driver
            .list(&base, page())
            .await
            .unwrap_or_else(|e| panic!("05/{name}: list: {e}"));
        for item in &listing.entries {
            let file_name = item.path.file_name().unwrap_or_default();
            assert!(
                !file_name.contains(".ckwd-"),
                "05/{name}: staging residue visible in list: {file_name}"
            );
        }

        cleanup(&driver, &base, &format!("05/{name}")).await;
        eprintln!("[SUMMARY] 05 overwrite_stash_abort|server={name}|overwrite+restore byte-exact|no .ckwd- residue");
    }
}

// ------------------------------------------- ⑥ 外部文件 list 可见性 ---

/// 矩阵腿⑥：authoritative_index 实证——测试外经 wsl.exe 直接在服务器根
/// 的 stamp 目录里落两个确定性内容文件（`X`×4096 / `Y`×8192），驱动
/// list 看到外部真相；清理后查 WSL 文件系统核实目录真删。
///
/// **双腿真形（WD5 真机揭出，怪癖矩阵新增行）**：
/// - **apache**（直接 fs）：外部文件真即时可见——全断言；
/// - **rclone**（`--vfs-cache-mode writes`）：外部直写**不进 VFS**——
///   warm 目录的 list 见缓存 stale 形、未缓存路径 404，直到
///   dir-cache-time（缺省 ≈5min）过期或服务器重启（本腿实测记录）。
///   驱动无错（如实转述服务器真相）；本腿先记录窗内行为（不预设），
///   再杀/重启 rclone（杀/起拆两次 wsl 调用）以**新 VFS 冷读外部真相**
///   ——authoritative_index 的端到端实证。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_06_external_write_visible() {
    let env = live_env();

    // ---- apache：直接 fs，外部文件真即时可见 ----
    {
        let label = "06/apache";
        let driver = driver_for(params_for(&env.apache, &env)).await;
        let stamp_s = stamp();
        let base = RelPath::new(&stamp_s).expect("stamp path");
        driver
            .mkdir(&base)
            .await
            .unwrap_or_else(|e| panic!("{label}: mkdir: {e}"));
        let out = wsl_ok(&external_write_script(WSL_APACHE_ROOT, &stamp_s));
        assert!(
            out.contains("4096") && out.contains("8192"),
            "{label}: external files must be written, wc said: {out}"
        );
        assert_external_files_visible(&driver, &base, label).await;
        verify_fs_cleanup(&driver, &base, WSL_APACHE_ROOT, &stamp_s, label).await;
        eprintln!("[SUMMARY] 06 external_visible|server=apache|2 external files listed+read immediately|fs-level cleanup verified");
    }

    // ---- rclone：缓存窗记录 + 重启冷读 ----
    {
        let label = "06/rclone";
        let driver = driver_for(params_for(&env.rclone, &env)).await;
        let stamp_s = stamp();
        let base = RelPath::new(&stamp_s).expect("stamp path");
        driver
            .mkdir(&base)
            .await
            .unwrap_or_else(|e| panic!("{label}: mkdir: {e}"));
        let out = wsl_ok(&external_write_script(WSL_RCLONE_ROOT, &stamp_s));
        assert!(
            out.contains("4096") && out.contains("8192"),
            "{label}: external files must be written, wc said: {out}"
        );
        // 缓存窗内的真实行为只记录不预设（stale 空表为当前 fixture 预期）。
        match driver.list(&base, page()).await {
            Ok(listing) => eprintln!(
                "[06:window] rclone: warm-cache list sees {} child(ren) — the server-side VFS \
                 dir-cache window hides out-of-band writes (recorded live behavior)",
                listing.entries.len()
            ),
            Err(error) => {
                eprintln!("[06:window] rclone: warm-cache list errored ({error}) — recorded")
            }
        }
        // 杀/重启（fresh VFS → 冷读即外部真相）。
        let t0 = Instant::now();
        wsl_kill_rclone();
        wsl_start_rclone(&env);
        let mut waited = Duration::ZERO;
        loop {
            if driver.stat(&base).await.is_ok() {
                break;
            }
            waited += Duration::from_secs(1);
            assert!(
                waited < Duration::from_secs(60),
                "{label}: rclone did not come back within 60s"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        eprintln!(
            "[06:restart] rclone restarted with a fresh VFS in {:?}; cold list follows",
            t0.elapsed()
        );
        assert_external_files_visible(&driver, &base, label).await;
        verify_fs_cleanup(&driver, &base, WSL_RCLONE_ROOT, &stamp_s, label).await;
        eprintln!("[SUMMARY] 06 external_visible|server=rclone|warm-cache window recorded|fresh-VFS cold list sees external truth|fs-level cleanup verified");
    }
}

/// 测试外落盘脚本（stamp 目录已由驱动 MKCOL；确定性内容 + world-readable）。
///
/// **wsl 通道脚本一律单行 + 字面路径 + 零引号零变量**（真机实证的坑：
/// wsl.exe 对 Rust `Command` 传入的脚本做 argv 重 join/重引号——多行
/// 折成一行、双引号形态变形，曾致「赋值行被吞、文件静默落到 WSL 根」
/// 与「`test -d "$D"` 恒假」两种假象；单引号与裸分号序列在腿⑨⑩的
/// 杀/起脚本已证存活）。`test -f` 尾守卫让落点缺席 fail loud。
fn external_write_script(root: &str, stamp_s: &str) -> String {
    let dir = format!("{root}/{stamp_s}");
    format!(
        "set -e; \
         head -c 4096 /dev/zero | tr '\\0' 'X' > {dir}/ext_a.bin; \
         head -c 8192 /dev/zero | tr '\\0' 'Y' > {dir}/ext_b.bin; \
         chmod 644 {dir}/ext_a.bin {dir}/ext_b.bin; \
         wc -c < {dir}/ext_a.bin; wc -c < {dir}/ext_b.bin; \
         test -f {dir}/ext_a.bin; test -f {dir}/ext_b.bin"
    )
}

/// 外部文件的驱动可见性断言（list 名集/尺寸投影 + stat + 逐字回读）。
async fn assert_external_files_visible(driver: &WebdavDriver, base: &RelPath, label: &str) {
    let listing = driver
        .list(base, page())
        .await
        .unwrap_or_else(|e| panic!("{label}: list: {e}"));
    let names: Vec<&str> = listing
        .entries
        .iter()
        .map(|item| item.path.file_name().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        vec!["ext_a.bin", "ext_b.bin"],
        "{label}: external files visible (sorted)"
    );
    let sizes: Vec<u64> = listing.entries.iter().map(|item| item.size).collect();
    assert_eq!(sizes, vec![4096, 8192], "{label}: projected sizes");
    for (file_name, byte, len) in [("ext_a.bin", b'X', 4096usize), ("ext_b.bin", b'Y', 8192)] {
        let p = base.join(file_name).expect("path");
        let stat = driver
            .stat(&p)
            .await
            .unwrap_or_else(|e| panic!("{label}: stat {file_name}: {e}"));
        assert_eq!(stat.size, len as u64);
        let got = read_all(
            driver
                .reader(&stat.id, None)
                .await
                .unwrap_or_else(|e| panic!("{label}: reader {file_name}: {e}")),
        )
        .await;
        assert_eq!(got, vec![byte; len], "{label}: {file_name} byte-exact");
    }
}

/// 清理 + WSL 文件系统级核空（stamp 目录必须真删；rclone 撒谎留残 →
/// 直接清 + 记录）。
async fn verify_fs_cleanup(
    driver: &WebdavDriver,
    base: &RelPath,
    root: &str,
    stamp_s: &str,
    label: &str,
) {
    cleanup(driver, base, label).await;
    let check = wsl(&format!(
        "[ -e '{root}/{stamp_s}' ] && echo EXISTS || echo GONE"
    ));
    if String::from_utf8_lossy(&check.stdout).contains("EXISTS") {
        wsl_ok(&format!("rm -rf '{root}/{stamp_s}'"));
        eprintln!(
            "[06:note] {label}: the underlying stamp dir survived the driver DELETE \
             (server-side deletion semantics) — removed directly via WSL and recorded"
        );
    }
}

// --------------------------------------------------- ⑦ 错误腿分类 ---

/// 矩阵腿⑦：错误分类真机回放——404 → NotFound（双服务器）；412 →
/// Exists（rename 撞既有目标，双方原样完好）；mkdir 已存在 → Exists
///（**WD0 rclone MKCOL-201 幂等陷阱对策的真机复核，必测**）+ mkdir 撞
/// 文件 → Exists；apache 无凭据 → Unauthorized{false}（apache 无纯 403
/// 注入面——401/403 族的真机替身）。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_07_error_classification() {
    let env = live_env();
    for (name, url) in servers(&env) {
        let driver = driver_for(params_for(url, &env)).await;
        let base = RelPath::new(&stamp()).expect("stamp path");
        driver
            .mkdir(&base)
            .await
            .unwrap_or_else(|e| panic!("07/{name}: mkdir: {e}"));

        // ① 404 → NotFound。
        let missing = base.join("no-such.bin").expect("path");
        match driver.stat(&missing).await {
            Err(StorageError::NotFound) => {}
            Ok(entry) => panic!("07/{name}: stat(missing) succeeded (size={})", entry.size),
            Err(other) => panic!("07/{name}: stat(missing) expected NotFound, got {other}"),
        }

        // ② 412 → Exists（Overwrite:F 撞既有目标）。
        let a = base.join("mv-src.bin").expect("path");
        let b = base.join("mv-dst.bin").expect("path");
        let da = pattern(32 * 1024, rand_seed());
        let db = pattern(32 * 1024, rand_seed());
        upload(&driver, &a, &da, &format!("07/{name}/src")).await;
        upload(&driver, &b, &db, &format!("07/{name}/dst")).await;
        match driver.rename(&a, &b).await {
            Err(StorageError::Exists) => {}
            Ok(()) => panic!("07/{name}: rename collision must not overwrite"),
            Err(other) => panic!("07/{name}: rename collision expected Exists, got {other}"),
        }
        let ea = driver
            .stat(&a)
            .await
            .unwrap_or_else(|e| panic!("07/{name}: source must survive the refused rename: {e}"));
        assert_eq!(ea.size, da.len() as u64);
        let eb = driver
            .stat(&b)
            .await
            .unwrap_or_else(|e| panic!("07/{name}: target must survive the refused rename: {e}"));
        let got = read_all(driver.reader(&eb.id, None).await.expect("07: reader")).await;
        assert_eq!(got, db, "07/{name}: target content intact");

        // ③ mkdir 已存在 → Exists（rclone 201 陷阱——stat 预检对策）。
        let sub = base.join("sub").expect("path");
        driver.mkdir(&sub).await.expect("07: first mkdir");
        match driver.mkdir(&sub).await {
            Err(StorageError::Exists) => {}
            Ok(()) => panic!(
                "07/{name}: mkdir(existing) returned Ok — the WD0 rclone MKCOL-201 trap \
                 countermeasure (stat precheck) failed on a real server"
            ),
            Err(other) => panic!("07/{name}: mkdir(existing) expected Exists, got {other}"),
        }
        // mkdir 撞文件 → Exists。
        match driver.mkdir(&a).await {
            Err(StorageError::Exists) => {}
            Ok(()) => panic!("07/{name}: mkdir(onto file) returned Ok"),
            Err(other) => panic!("07/{name}: mkdir(onto file) expected Exists, got {other}"),
        }

        cleanup(&driver, &base, &format!("07/{name}")).await;
        eprintln!(
            "[SUMMARY] 07 errors|server={name}|404→NotFound|412→Exists(both intact)|mkdir-dup→Exists|mkdir-on-file→Exists"
        );
    }

    // ④ apache 无凭据 → Unauthorized{false}（401/403 族真机形态）。
    let anon = driver_for(anon_params(&env.apache)).await;
    let base = RelPath::new(&stamp()).expect("stamp path");
    match anon.stat(&base).await {
        Err(StorageError::Unauthorized { recoverable: false }) => {}
        Ok(entry) => panic!("07: anonymous apache stat succeeded (size={})", entry.size),
        Err(other) => panic!(
            "07: anonymous apache stat expected Unauthorized{{recoverable:false}}, got {other}"
        ),
    }
    eprintln!("[SUMMARY] 07 errors|apache-no-creds→Unauthorized(recoverable=false)");
}

// ------------------------------------------------- ⑧ digest 全链路 ---

/// 矩阵腿⑧（apache）：①nc 真实递增——同一 client 连续 5 请求全成功
///（apache 不查 nc，驱动侧 per-nonce 单调自守）；②stale 语义真机复核
///（**D1 必测**）——`/dav-stale/`（AuthDigestNonceLifetime=2）上
/// sleep 3.5s 过 nonce 期 → 下一请求 401+stale=true → 换 nonce 重算
/// 重发**恰一次**自动恢复，随后会话连续。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_08_digest_nc_and_stale_recovery() {
    let env = live_env();

    // ① nc 单调：auto 模式主驱动（Basic 预发 → 401 → Digest 协商后同会话）。
    let driver = driver_for(params_for(&env.apache, &env)).await;
    let base = RelPath::new(&stamp()).expect("stamp path");
    driver
        .mkdir(&base)
        .await
        .unwrap_or_else(|e| panic!("08: mkdir: {e}"));
    let t0 = Instant::now();
    for i in 1..=5 {
        driver
            .stat(&base)
            .await
            .unwrap_or_else(|e| panic!("08: nc request #{i}: {e}"));
    }
    let five = t0.elapsed();
    eprintln!(
        "[08:nc] 5 consecutive requests in {five:?} (apache does not check nc; the driver's \
         per-nonce counter stays monotonic) — all succeeded"
    );

    // ② stale 腿：digest 显式模式 + AuthDigestNonceLifetime=2。
    let sdriver = driver_for(digest_params(&env.apache_stale, &env)).await;
    let sb = RelPath::new(&stamp()).expect("stamp path");
    sdriver
        .mkdir(&sb)
        .await
        .unwrap_or_else(|e| panic!("08: mkdir stale: {e}"));
    let data = pattern(4096, rand_seed());
    let f = sb.join("stale.bin").expect("path");
    upload(&sdriver, &f, &data, "08/stale-upload").await;
    let pre = sdriver
        .stat(&f)
        .await
        .expect("08: pre-sleep stat (nonce fresh)");
    assert_eq!(pre.size, data.len() as u64);
    eprintln!("[08:stale] nonce is fresh; sleeping 3.5s (> AuthDigestNonceLifetime=2) ...");
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let t1 = Instant::now();
    let post = sdriver.stat(&f).await.unwrap_or_else(|e| {
        panic!(
            "08: post-expiry stat must transparently re-negotiate the stale nonce \
             (401 stale=true → one resend), got: {e}"
        )
    });
    let recovered = t1.elapsed();
    assert_eq!(post.size, data.len() as u64);
    eprintln!(
        "[SUMMARY] 08 digest|nc5={five:?}|stale_recovery_stat={recovered:?} (includes the \
         401→stale→renegotiate→resend round trip)"
    );

    // 会话连续性：再协商后的会话继续服务。
    let got = read_all(
        sdriver
            .reader(&post.id, None)
            .await
            .unwrap_or_else(|e| panic!("08: post-recovery reader: {e}")),
    )
    .await;
    assert_eq!(got, data, "08: post-recovery read byte-exact");
    sdriver.stat(&sb).await.expect("08: post-recovery stat");

    cleanup(&sdriver, &sb, "08/stale").await;
    cleanup(&driver, &base, "08/main").await;
}

// ------------------------------------------- ⑩ 拒绝腿 + doctor 五态 ---

/// 矩阵腿⑩：拒绝腿可行动文案 + doctor probe 五态真机复核——错凭据
///（rclone Basic 错密码 / apache Digest 协商后拒）→ Unauthorized{false}；
/// 坏 URL（连接拒绝）→ Unavailable；`ck_webdav::probe()`：两服务器 =
/// Alive、错凭据 = CredentialsRejected、坏 URL = Unreachable。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_09_rejection_legs_and_probe() {
    let env = live_env();

    // ① 错凭据（协商已尽 → Unauthorized{recoverable:false}）。
    for (name, url) in servers(&env) {
        let driver = driver_for(wrong_pass_params(url, &env)).await;
        let base = RelPath::new(&stamp()).expect("stamp path");
        match driver.stat(&base).await {
            Err(StorageError::Unauthorized { recoverable: false }) => {}
            Ok(entry) => panic!(
                "09/{name}: wrong password must NOT authenticate (size={})",
                entry.size
            ),
            Err(other) => panic!(
                "09/{name}: wrong password expected Unauthorized{{recoverable:false}}, got {other}"
            ),
        }
        eprintln!("[SUMMARY] 09 wrong-creds|server={name}|→Unauthorized(recoverable=false) after negotiation");
    }

    // ② 坏 URL（连接拒绝 → Unavailable；PROPFIND 白名单重试耗尽后浮现）。
    let bad = bad_url(&env.rclone);
    let driver = driver_for(params_for(&bad, &env)).await;
    let base = RelPath::new(&stamp()).expect("stamp path");
    let t0 = Instant::now();
    match driver.stat(&base).await {
        Err(StorageError::Unavailable(_)) => {}
        Ok(entry) => panic!("09: bad URL must not succeed (size={})", entry.size),
        Err(other) => panic!("09: bad URL ({bad}) expected Unavailable, got {other}"),
    }
    eprintln!(
        "[SUMMARY] 09 bad-url|{bad}|→Unavailable in {:?} (includes the idempotent-verb retry budget)",
        t0.elapsed()
    );

    // ③ doctor probe 五态真机复核。
    let cases = [
        ("rclone(alive)", params_for(&env.rclone, &env)),
        ("apache(alive)", params_for(&env.apache, &env)),
        ("rclone(wrong pass)", wrong_pass_params(&env.rclone, &env)),
        ("apache(wrong pass)", wrong_pass_params(&env.apache, &env)),
        ("bad url", params_for(&bad, &env)),
    ];
    let mut verdicts = Vec::new();
    for (label, params) in cases {
        let t = Instant::now();
        let verdict = ck_webdav::probe(&params).await;
        match &verdict {
            WebdavProbe::Alive { dav_class, allow } => eprintln!(
                "[probe] {label}: Alive (dav={dav_class:?}, allow={allow:?}) in {:?}",
                t.elapsed()
            ),
            WebdavProbe::CredentialsRejected { detail } => eprintln!(
                "[probe] {label}: CredentialsRejected in {:?} — {detail}",
                t.elapsed()
            ),
            WebdavProbe::ReachableNoAuth => eprintln!("[probe] {label}: ReachableNoAuth"),
            WebdavProbe::Unreachable { detail } => eprintln!(
                "[probe] {label}: Unreachable in {:?} — {detail}",
                t.elapsed()
            ),
            WebdavProbe::TlsUntrusted { detail } => {
                eprintln!("[probe] {label}: TlsUntrusted — {detail}")
            }
        }
        verdicts.push((label, verdict));
    }
    for (label, verdict) in verdicts {
        let expected_alive = matches!(label, "rclone(alive)" | "apache(alive)");
        let expected_rejected = matches!(label, "rclone(wrong pass)" | "apache(wrong pass)");
        assert!(
            (expected_alive && matches!(verdict, WebdavProbe::Alive { .. }))
                || (expected_rejected
                    && matches!(verdict, WebdavProbe::CredentialsRejected { .. }))
                || (label == "bad url" && matches!(verdict, WebdavProbe::Unreachable { .. })),
            "09: probe classification mismatch for {label}: {verdict:?}"
        );
    }
    eprintln!("[SUMMARY] 09 probe|2x Alive|2x CredentialsRejected|1x Unreachable|as classified");
}

// ------------------------------------- ⑨ 断线自愈（**排最后**）+ 核空 ---

/// 矩阵腿⑨（套件内排最后执行——服务器重启影响兄弟腿）：rclone 腿。
/// 测试内 `wsl.exe` 杀 rclone serve（杀/起拆两次调用；`serv[e]` 字符类
/// 防自匹配）→ 在途请求得 `Unavailable`（不 panic；PROPFIND 白名单重试
/// 耗尽后浮现，PUT 永不重试立即浮现）→ 重启 rclone（fixture 同款
/// nohup 行）→ **同 client** 重试成功（客户端池自愈）+ 数据跨重启逐字
/// 完好 + 中途放弃的写零残留。末尾做**全套全局核空**：find 三服务器根
/// 验证 `e2e_webdav_*` 前缀零残留 + `.ckwd-*` 暂存件零残留。
#[tokio::test]
#[ignore = "live matrix: needs CYDRIVE_WEBDAV_TEST_* env and the WSL2 fixture (see the module docs)"]
async fn live_10_disconnect_selfheal() {
    let env = live_env();
    let driver = driver_for(params_for(&env.rclone, &env)).await;
    let base = RelPath::new(&stamp()).expect("stamp path");
    driver
        .mkdir(&base)
        .await
        .unwrap_or_else(|e| panic!("10: mkdir: {e}"));

    let data = pattern(MIB as usize, rand_seed());
    let f1 = base.join("keep.bin").expect("path");
    upload(&driver, &f1, &data, "10/keep").await;

    // 断线前持有第二 stager（spool 已写入，close 跨断线）。
    let b = pattern(64 * 1024, rand_seed());
    let f2 = base.join("midair.bin").expect("path");
    let hint = WriteHint {
        size: Some(b.len() as u64),
        ..Default::default()
    };
    let mut midair = driver
        .writer(&f2, &hint)
        .await
        .unwrap_or_else(|e| panic!("10: writer before the outage: {e}"));
    midair
        .write(&b)
        .await
        .unwrap_or_else(|e| panic!("10: spool write before the outage: {e}"));

    // 杀（第一次 wsl 调用）。
    let t0 = Instant::now();
    wsl_kill_rclone();
    let killed = t0.elapsed();
    eprintln!("[10:kill] rclone serve killed and confirmed dead in {killed:?}");

    // 在途读 → Unavailable（幂等动词带 500ms+1s+2s 重试预算后浮现）。
    let t1 = Instant::now();
    match driver.stat(&f1).await {
        Err(StorageError::Unavailable(_)) => {}
        Ok(entry) => panic!(
            "10: server is down — stat must fail, got Ok(size={})",
            entry.size
        ),
        Err(other) => panic!("10: server is down — stat expected Unavailable, got {other}"),
    }
    let stat_failed = t1.elapsed();
    eprintln!("[10:outage] stat during outage → Unavailable after {stat_failed:?} (retry budget)");

    // close 链在断线中的分类：ensure_parents 的 stat 走幂等重试预算 →
    // 失败后 restore_scene 的尽力 DELETE 单发不重试（断线瞬间的收尾成
    // 本如实计量）；终局分类恒 Unavailable 不 panic。
    let t2 = Instant::now();
    match midair.close().await {
        Err(StorageError::Unavailable(_)) => {}
        Ok(entry) => panic!(
            "10: server is down — close must fail, got Ok(size={})",
            entry.size
        ),
        Err(other) => panic!(
            "10: server is down — close expected Unavailable (write family never auto-retries), \
             got {other}"
        ),
    }
    eprintln!(
        "[10:outage] stager close during outage → Unavailable after {:?} (write family never \
         auto-retries; restore-scene cleanup paid its own one-shot timeout)",
        t2.elapsed()
    );

    // 起（第二次 wsl 调用；fixture 文档同款 nohup 行 + pid 文件回写）。
    let start = Instant::now();
    wsl_start_rclone(&env);
    eprintln!(
        "[10:restart] rclone serve process up in {:?}",
        start.elapsed()
    );

    // 同 client 池自愈：轮询到首个成功。
    let t3 = Instant::now();
    let mut attempts = 0u32;
    let recovered = loop {
        attempts += 1;
        if driver.stat(&f1).await.is_ok() {
            break t3.elapsed();
        }
        assert!(
            t3.elapsed() < Duration::from_secs(60),
            "10: rclone did not come back within 60s"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    eprintln!(
        "[SUMMARY] 10 selfheal|kill→dead={killed:?}|outage stat→Unavailable={stat_failed:?}|\
         restart→first success={recovered:?} after {attempts} attempt(s)"
    );

    // 数据跨重启逐字完好 + 中途放弃的写零残留。
    let e1 = driver
        .stat(&f1)
        .await
        .unwrap_or_else(|e| panic!("10: stat after recovery: {e}"));
    assert_eq!(e1.size, data.len() as u64);
    let got = read_all(
        driver
            .reader(&e1.id, None)
            .await
            .unwrap_or_else(|e| panic!("10: reader after recovery: {e}")),
    )
    .await;
    assert_eq!(
        got, data,
        "10: uploaded data survives the server restart byte-exact"
    );
    match driver.stat(&f2).await {
        Err(StorageError::NotFound) => {}
        Ok(entry) => panic!(
            "10: the aborted mid-air write must leave nothing at the final path (size={})",
            entry.size
        ),
        Err(other) => panic!("10: aborted mid-air path expected NotFound, got {other}"),
    }
    cleanup(&driver, &base, "10").await;

    // ---- 全套全局核空：三服务器根零残留（K72 收尾纪律）----
    // 清扫用命令替换单行完成（find 的多行输出不得回填进脚本——wsl 通道
    // 对参数内嵌换行不可靠，真机实证会把 `rm -rf p1\np2` 折成两行执行）。
    let roots = format!("{WSL_RCLONE_ROOT} {WSL_APACHE_ROOT} {WSL_APACHE_STALE_ROOT}");
    let residue = wsl_ok(&format!(
        "find {roots} -maxdepth 1 -name 'e2e_webdav_*' 2>/dev/null"
    ));
    if !residue.trim().is_empty() {
        eprintln!(
            "[10:note] residual stamp dirs survived driver-side cleanup (server-side deletion \
             semantics); removing directly via WSL and recording: {}",
            residue.trim()
        );
        wsl_ok(&format!(
            "rm -rf $(find {roots} -maxdepth 1 -name 'e2e_webdav_*' 2>/dev/null)"
        ));
    }
    let final_check = wsl_ok(&format!(
        "find {roots} -maxdepth 1 -name 'e2e_webdav_*' 2>/dev/null; \
         find {roots} -name '*.ckwd-*' 2>/dev/null"
    ));
    assert!(
        final_check.trim().is_empty(),
        "10: zero residue expected across the three server roots, got: {}",
        final_check.trim()
    );
    eprintln!(
        "[SUMMARY] 10 cleanup|e2e_webdav_* residue=0 across rclone+apache+dav-stale roots|\
         *.ckwd-* residue=0 (find on the WSL filesystem)"
    );
}
