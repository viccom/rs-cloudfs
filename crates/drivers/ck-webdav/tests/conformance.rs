//! ck-webdav conformance 套件接入（Phase 7 / WD3；interfaces §6 / D9）。
//!
//! 被测对象 = 全量实现驱动（WD2 读面 + WD3 写面），后端注入 = **dav-server
//! 参照桩**（`davref/mod.rs`——独立第二实现：dav-server 0.11 生产代码 +
//! LocalFs 真文件系统；「桩照实现抄」防线，计划 §1.2 双桩制的真实现腿）。
//! `conformance_suite_offline` 跑断言①–⑥⑧；⑦ RESUME 未声明（`resume =
//! false`——上层缓存承担断点续传，WD2 能力位）由能力位门控自动跳过
//!（local/sftp 先例）。
//!
//! harness 形态声明：
//! - `chunk_size = 1`：WebDAV PUT 整体上传无分块（无分块驱动报 1）；
//! - `delete_missing = NotFound`：stat 预检天然给出（sftp 同款声明——
//!   driver 文档恒定）；
//! - `empty_range = 空流`：start>=size 不开 GET 直接空流（§4.7）；
//! - `error_table` 两码选码理由（注入路径是 **stat**，走参照桩的
//!   `fail_next_propfind` 一次性拦截——映射经真实 HTTP 回放）：
//!   - `HTTP_404 → NotFound`：NotFound 判定只认显式 404（§4.4 表；
//!     403/传输类绝不折叠为 NotFound）；
//!   - `HTTP_401 → Unauthorized{false}`：协商已尽的终局形态（无凭据
//!     配置下 401 无自救——可行动文案在 warn 通道，R3 双通道）；
//!   - 刻意不选 5xx 族（Unavailable）：PROPFIND 在重试白名单内，单次
//!     注入会被重试自愈消费——「stat 必须失败」不成立（sftp conformance
//!     刻意不选 SSH_FX_NO_CONNECTION 的同款互指：重试腿由 connect_auth
//!     的 5xx 自愈/耗尽测试独立钉死）；
//!   - 刻意不选 429：同因（Retry-After 白名单重试）。
//!
//! ## 断言①覆盖写腿的独立裁决（§4.6 预案，带着判决书进场）
//!
//! [`assertion1_overwrite_leg_target_invisible_while_staging`]：覆盖写
//! 场景的 staging 窗口内旧对象必须不可见——由本参照桩（真实现）判定。
//! 判决：**无 stash 形态红**（旧对象在 close 前仍可见于目标路径）→
//! stash 协议上车（writer 打开时 `MOVE final → .ckwd-*.old`，close 成功
//! 删、abort 恢复）→ 绿。证据见 WD3 批次日志（跟踪单）。

mod davref;

use std::collections::HashMap;

use async_trait::async_trait;
use davref::{spawn_davref, DavRefHandle};
use futures_util::StreamExt;

use cloudkit_storage::conformance::{ConformanceHarness, ErrorReplay};
use cloudkit_storage::{ByteStream, RelPath, StorageDriver, StorageError, WriteHint};

use ck_webdav::{parse_from_map, WebdavDriver};

/// 无分块驱动：chunk 边界报 1（ConformanceHarness::chunk_size 契约）。
const CHUNK: u64 = 1;

struct WebdavHarness {
    /// 参照桩活到 harness 生命结束（shutdown 随测试结束回收）。
    _reference: DavRefHandle,
    driver: WebdavDriver,
}

impl WebdavHarness {
    async fn new() -> Self {
        let reference = spawn_davref().await;
        let mut map = HashMap::new();
        map.insert("webdav_url".to_string(), reference.url.clone());
        let params = parse_from_map(&map).expect("conformance params parse");
        let driver = WebdavDriver::new(params).expect("driver constructs");
        WebdavHarness {
            _reference: reference,
            driver,
        }
    }
}

#[async_trait]
impl ConformanceHarness for WebdavHarness {
    fn driver(&self) -> &dyn StorageDriver {
        &self.driver
    }

    fn chunk_size(&self) -> u64 {
        CHUNK
    }

    fn delete_missing_yields_not_found(&self) -> bool {
        // stat 预检形态（write_path.rs `delete_file_dir_recursive_and_
        // missing_idempotency` 钉死）：删除不存在路径恒 NotFound。
        true
    }

    fn empty_range_yields_empty_stream(&self) -> bool {
        // driver.rs reader：start>=size / 空窗口不开 GET 直接空流（§4.7）。
        true
    }

    fn error_table(&self) -> Vec<ErrorReplay> {
        vec![
            ErrorReplay {
                backend_code: "HTTP_404".to_string(),
                expected: StorageError::NotFound,
            },
            ErrorReplay {
                backend_code: "HTTP_401".to_string(),
                expected: StorageError::Unauthorized { recoverable: false },
            },
        ]
    }

    async fn inject_backend_error(&self, backend_code: &str) {
        match backend_code {
            "HTTP_404" => self._reference.fail_next_propfind(404),
            "HTTP_401" => self._reference.fail_next_propfind(401),
            other => panic!("error_table 码必须是可注入的 HTTP 状态，got {other:?}"),
        }
    }
}

cloudkit_storage::conformance_suite!(WebdavHarness::new().await);

// ------------------------------------------------ 断言①覆盖写腿裁决 ---

/// 断言①（覆盖写腿，§4.6 预案的实测裁决）+ abort 恢复 + stash 退役：
/// dav-server 参照桩（独立第二实现）判定——writer 打开后的 staging
/// 窗口内，被覆盖的旧对象必须不可见；abort 逐字节恢复；close 后新内容
/// 就位、无 `.ckwd-` 残留。
#[tokio::test]
async fn assertion1_overwrite_leg_target_invisible_while_staging() {
    let reference = spawn_davref().await;
    let mut map = HashMap::new();
    map.insert("webdav_url".to_string(), reference.url.clone());
    let driver =
        WebdavDriver::new(parse_from_map(&map).expect("params")).expect("driver constructs");

    let rel = RelPath::new("judgment/f.bin").expect("rel path");
    // 先经正式写面提交一版。
    upload_all(&driver, &rel, b"old-version-bytes", "①judgment").await;

    // ---- 裁决点：staging 窗口内旧对象不可见 ----
    let fresh = b"new-version-bytes";
    let hint = WriteHint {
        size: Some(fresh.len() as u64),
        ..Default::default()
    };
    let mut stager = driver.writer(&rel, &hint).await.expect("writer opens");
    match driver.stat(&rel).await {
        Err(StorageError::NotFound) => {}
        Err(error) => panic!("①judgment mid-staging stat 期望 NotFound，得到 {error}"),
        Ok(_) => panic!(
            "①judgment mid-staging stat：close 前目标仍可见——覆盖写场景的旧对象必须被 \
             stash 不可见（断言①，sftp .old 判例）"
        ),
    }
    stager.write(fresh).await.expect("write");
    let entry = stager.close().await.expect("close commits");
    assert_eq!(entry.size, fresh.len() as u64);
    assert_eq!(
        read_all(
            driver.reader(&entry.id, None).await.expect("reader"),
            "①judgment"
        )
        .await,
        fresh,
        "①judgment new content lands"
    );

    // ---- abort 恢复腿：旧对象逐字节回位 ----
    let mut stager = driver
        .writer(&rel, &WriteHint::default())
        .await
        .expect("writer opens (abort leg)");
    stager.write(b"never-lands").await.expect("write");
    stager.abort().await.expect("abort restores");
    let entry = driver.stat(&rel).await.expect("old object restored");
    assert_eq!(entry.size, fresh.len() as u64);
    assert_eq!(
        read_all(
            driver.reader(&entry.id, None).await.expect("reader"),
            "①judgment"
        )
        .await,
        fresh,
        "①judgment abort restores the pre-writer content byte-for-byte"
    );

    // ---- 无残留：.ckwd- 暂存件（.part/.old）不出现在任何层级 ----
    let listing = driver
        .list(
            &RelPath::new("judgment").expect("dir"),
            cloudkit_storage::Page {
                limit: 100,
                cursor: cloudkit_storage::PageCursor::Start,
            },
        )
        .await
        .expect("list");
    assert!(
        listing.entries.iter().all(|entry| !entry
            .path
            .file_name()
            .unwrap_or("")
            .contains(".ckwd-")),
        "①judgment no staging residue in list: {:?}",
        listing.entries
    );

    reference.shutdown().await;
}

// ------------------------------------------------------------ 测试工具 ---

async fn upload_all(
    driver: &WebdavDriver,
    path: &RelPath,
    data: &[u8],
    label: &str,
) -> cloudkit_storage::Entry {
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

async fn read_all(mut stream: ByteStream, label: &str) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(error) => panic!("{label} 流中途错误: {error}"),
        }
    }
    out
}
