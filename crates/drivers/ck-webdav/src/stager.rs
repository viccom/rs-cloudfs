//! WebDAV 上传暂存器（Phase 7 / WD1a 占位——WD3 接线，计划 §4.6）。
//!
//! commit-on-close 语义链（与 ck-local/ck-sftp 同构）：
//!
//! - **open_writer**：本地 NamedTempFile spool（drop 自动清理）；
//!   写入超 hint 承诺 → `Invalid`（sftp hint 契约同款）；
//! - **close**：spool → `PUT <final>.ckwd-<pid>-<seq>.part`
//!   （**Content-Length 已知**——D5：chunked 直传挂账，样本仅二不
//!   赌）→ `MOVE .part → final` 固化 → **stat 复核 size ==
//!   written**（aeroftp 硬仗②）→ 清理。覆盖写 stash 预案（断言①
//!   实测裁决后上：open 时 final 存在 → `MOVE final → .ckwd-*.old`，
//!   close 成功删 stash、abort 恢复）；
//! - **重放窗防线**（K67 H2 同型）：MOVE ACK 丢失重放 → .part 不
//!   在 + final 就位且 size 吻合 → 按已提交继续；
//! - **abort**：删 spool + DELETE .part（若已 PUT）；final 从未被
//!   直接触碰；
//! - `.ckwd-` 前缀暂存件对 `list` 恒过滤（断言③集合完整性）。
//!
//! mtime 写策略：generic **不写**（D2 降级——WD0 实证两家均不能真
//! 写）；nextcloud PUT 携 `X-OC-Mtime` 搭车（零成本保留）。
//!
//! - 语义：上表；
//! - 错误：传输失败按计划 §4.4 映射；close 时 size 承诺不符 →
//!   `Invalid`；提交后远端 size != written → `Io`（硬仗②）；
//! - 并发：单 stager 串行使用（trait 契约）；
//! - 生命周期：close/abort/Drop（spool 的 NamedTempFile 自动清理）
//!   三选一。

use async_trait::async_trait;

use cloudkit_storage::{Entry, RelPath, StorageError, UploadStager};

/// WebDAV 上传暂存器（WD1a 类型占位：字段就位，动词面 WD3 接线）。
pub struct WebdavStager {
    /// 本地 spool（NamedTempFile——drop 自动清理；WD3 的字节源）。
    #[allow(dead_code)] // WD1a 骨架：commit 链 WD3 接线后进入使用
    spool: tempfile::NamedTempFile,
    /// 最终卷内路径（`.part`/stash 命名的基准）。
    #[allow(dead_code)] // WD1a 骨架：WD3 接线后进入使用
    final_rel: RelPath,
    /// 已写入 spool 的字节数（close 的 size 复核基准）。
    #[allow(dead_code)] // WD1a 骨架：WD3 接线后进入使用
    written: u64,
    /// WriteHint 的 size 承诺（超承诺 → `Invalid`）。
    #[allow(dead_code)] // WD1a 骨架：WD3 接线后进入使用
    hinted_size: Option<u64>,
    /// close/abort 收尾标志（防二次收尾）。
    #[allow(dead_code)] // WD1a 骨架：WD3 接线后进入使用
    finished: bool,
}

#[async_trait]
impl UploadStager for WebdavStager {
    /// 写入 spool（本地 IO——WD3 接线；超 hint 承诺 → `Invalid`）。
    async fn write(&mut self, _data: &[u8]) -> Result<(), StorageError> {
        // TODO(wd3): spool 写入 + 承诺校验接线。
        Err(StorageError::Unsupported)
    }

    /// 提交（PUT .part → MOVE 固化 → size 复核——WD3 接线）。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        // TODO(wd3): commit-on-close 链接线（stash 预案随断言①裁决）。
        self.finished = true;
        Err(StorageError::Unsupported)
    }

    /// 放弃（回到 writer 打开前状态——WD3 接线）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        // TODO(wd3): spool 清理 + .part DELETE 接线。
        self.finished = true;
        Err(StorageError::Unsupported)
    }
}
