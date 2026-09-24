//! 本地 spool 暂存件（L2 共享机制件——commit-on-close 的写面）。
//!
//! **契约来源**：ck-pan115/ck-pan123 的 spool stager `write()` 体在去注释后
//! 逐行相同（架构审查 D4 项①）；baidu 不适用（内存 buffer + 满块增量 md5 +
//! 流式 flush，无 spool 文件）。抽取为 L2 件，行为等价第一（D-4）。
//!
//! **只收纯机制**：溢出守卫（`checked_add`）→ append 落 spool → 累计 →
//! 超承诺拒绝 → **到齐判定**（承诺 size 到齐的那一次 write 返回 `true`）。
//! 到齐后做什么（起传输链）是驱动私有——调用方据返回值在自身
//! `transfer.is_none()` 守卫下触发；L2 不含任何协议面。
//!
//! 形态裁决：**自由函数 [`spool_append_write`]**（操作调用方既有的
//! `spool`/`written` 字段）+ 薄封装 [`SpoolStage`]（独立使用/单测用）。
//! 自由函数让 stager 无需重构字段所有权即可接入——`run_transfer` 等后续
//! 步骤照常读 `self.written`/`self.spool`，零迁移面。

use crate::error::StorageError;

/// 追加一段数据到 spool 文件并更新累计计数（共享机制，逐形自两驱动的
/// `write()` 体）。
///
/// 顺序（与驱动端原实现逐形）：
/// 1. u64 溢出守卫（`checked_add`，拒绝而非回绕）；
/// 2. append 落 `spool`（`create(true)` 对驱动侧「构造即建 spool」幂等，
///    独立使用则首写自建）；
/// 3. `written += data.len()`；
/// 4. 超承诺拒绝（`hint_size` 存在且 `written > hinted` → `Invalid`）；
/// 5. 返回 **本次 write 是否到齐**（`hint_size` 存在且 `written == hinted`
///    → `true`；无承诺或未达量 → `false`）。
///
/// - `spool`：调用方持有的 spool 文件路径（生命周期归调用方——Drop/abort
///   清理由调用方负责）；
/// - `written`：调用方持有的累计计数（本函数就地更新；溢出守卫先看后写，
///   失败不留半更新态）。
///
/// 错误即返回，`written` 与 spool 状态与驱动端原实现一致（到齐失败的
/// 后续 `close` 续传形态不受影响）。
pub async fn spool_append_write(
    spool: &std::path::Path,
    written: &mut u64,
    data: &[u8],
    hint_size: Option<u64>,
) -> Result<bool, StorageError> {
    // u64 溢出守卫：checked_add 语义（天文数字级累积才可能触发，
    // 形态上仍拒绝而非回绕）。
    written
        .checked_add(data.len() as u64)
        .ok_or(StorageError::Invalid)?;
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        // create(true) 对驱动侧既有的「构造即建 spool」是幂等无操作
        // （文件已在）；对本件的独立使用/单测则保证首写可自建——行为
        // 等价（D-4），仅放宽「文件必须预存在」这一隐含前置。
        .create(true)
        .open(spool)
        .await
        .map_err(|e| StorageError::Io(format!("spool append: {e}")))?;
    file.write_all(data)
        .await
        .map_err(|e| StorageError::Io(format!("spool write: {e}")))?;
    *written += data.len() as u64;
    // 超承诺：立刻失败（不再拉数据做无用工）。
    if let Some(hinted) = hint_size {
        if *written > hinted {
            return Err(StorageError::Invalid);
        }
    }
    // 到齐判定：承诺 size 到齐的那一次 write 返回 true。
    Ok(matches!(hint_size, Some(hinted) if *written == hinted))
}

/// 本地 spool 暂存进度（薄封装——独立使用/单测用；stager 接入走
/// [`spool_append_write`] 自由函数，无需本结构）。
pub struct SpoolStage {
    /// spool 文件路径（本地临时；生命周期归调用方）。
    spool: std::path::PathBuf,
    /// 已落 spool 的累计字节数。
    written: u64,
}

impl SpoolStage {
    /// 以 spool 路径构造（文件不必预先存在——首次 write 以 append 打开）。
    pub fn new(spool: std::path::PathBuf) -> Self {
        SpoolStage { spool, written: 0 }
    }

    /// 已暂存字节数（传输链/会话键的输入面）。
    pub fn written(&self) -> u64 {
        self.written
    }

    /// 追加一段数据（薄封装，转发 [`spool_append_write`]）。返回值含义：
    /// 本次 write 是否到齐（承诺 size 到齐的那一次 write 返回 `true`）。
    ///
    /// 到齐后做什么（起传输链）是调用方私有——调用方据返回值在自身
    /// `transfer.is_none()` 守卫下触发（`run_transfer` 需要 `&mut self`，
    /// 无法嵌进本件的借用期，故本件只判定、由调用方反应）。
    pub async fn write(
        &mut self,
        data: &[u8],
        hint_size: Option<u64>,
    ) -> Result<bool, StorageError> {
        spool_append_write(&self.spool.clone(), &mut self.written, data, hint_size).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_spool() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = dir.path().join("spool.bin");
        (dir, spool)
    }

    #[tokio::test]
    async fn write_accumulates_and_persists_bytes() {
        let (_guard, spool) = tmp_spool();
        let mut stage = SpoolStage::new(spool.clone());
        assert!(!stage.write(b"abc", None).await.expect("w1"));
        assert!(!stage.write(b"de", None).await.expect("w2"));
        assert_eq!(stage.written(), 5);
        assert_eq!(std::fs::read(&spool).expect("read"), b"abcde");
    }

    #[tokio::test]
    async fn arrival_is_reported_only_on_reaching_the_promised_size() {
        let (_guard, spool) = tmp_spool();
        let mut stage = SpoolStage::new(spool);
        // 承诺 5：第一次到 3（未到齐 → false），第二次到 5（到齐 → true）。
        assert!(!stage.write(b"abc", Some(5)).await.expect("w1"));
        assert!(stage.write(b"de", Some(5)).await.expect("w2"));
    }

    #[tokio::test]
    async fn overpromise_is_rejected_after_the_bytes_land() {
        let (_guard, spool) = tmp_spool();
        let mut stage = SpoolStage::new(spool);
        assert!(!stage.write(b"abc", Some(4)).await.expect("w1"));
        let err = stage.write(b"de", Some(4)).await.expect_err("超承诺须拒绝");
        assert!(matches!(err, StorageError::Invalid), "got {err:?}");
    }

    #[tokio::test]
    async fn no_hint_never_reports_arrival() {
        let (_guard, spool) = tmp_spool();
        let mut stage = SpoolStage::new(spool);
        assert!(!stage.write(b"abcdef", None).await.expect("no-hint write"));
    }
}
