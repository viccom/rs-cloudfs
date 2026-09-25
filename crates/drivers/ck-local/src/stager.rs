//! LocalStager——本地文件系统上传暂存器（Phase 2 Batch L）。
//!
//! commit-on-close（interfaces §2）的落地形态：暂存文件住在卷根下
//! `.cklocal-staging/`（驱动保留名，`list(根)` 恒过滤——见
//! driver.rs 模块文档保留名规则），[`UploadStager::close`] 用同卷
//! `rename` 把暂存文件原子固化为最终路径对象：
//!
//! - **close 前不可见**天然成立——最终路径上不存在半成品文件
//!   （conformance 断言① commit-on-close 不可见性）；
//! - **overwrite 也不可见**：writer 打开时若目标已有旧版本，旧对象被
//!   搬进暂存区 stash（同 `.old` 命名）——staging 期间 stat 目标恒
//!   `NotFound`；close 删 stash（新版本取代），abort/Drop 恢复 stash
//!   （放弃上传 = 回到 writer 打开前状态）；
//! - **close 后立即可见**——rename 返回即完成，无中间态；
//! - **drop 未 close/abort → 删暂存 + 恢复 stash**（不留垃圾，trait 契约）。
//!
//! 生命周期：stager 独占暂存文件句柄与 stash 直到 close/abort/Drop；
//! close/abort 成功路径先置 `finished`，Drop 据此跳过清理。close 中途
//! 失败（如 size 提示不符）：`Box` 在返回时析构 → Drop 收尾（删 tmp +
//! 恢复旧版），无孤儿、旧版本不失守。

use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use cloudkit_storage::{Entry, RelPath, StorageError, UploadStager, VolumeId};

use crate::driver::{entry_from_meta, map_io};

/// 本地上传暂存器：追加写暂存文件，close 原子 rename 为最终对象。
///
/// - 语义：write 顺序追加（单 stager 串行使用——trait 契约）；
/// - 错误：io 失败按 [`map_io`] 映射；close 时 size 提示与实际不符 →
///   `Invalid`（WriteHint 契约）；
/// - 并发：不同 stager（不同暂存文件）可并行；同一 stager 串行；
/// - 生命周期：暂存文件与 stash 由 stager 独占，close（rename + 删
///   stash）/abort（删暂存 + 恢复 stash）/Drop（同 abort，best-effort
///   同步形态）三选一收尾。
pub struct LocalStager {
    volume: VolumeId,
    /// 目标卷内路径（close 后 Entry.path 的来源）。
    final_rel: RelPath,
    /// 目标绝对路径（close 时 rename 的目的地）。
    final_path: PathBuf,
    /// 暂存绝对路径（`.cklocal-staging/` 下，`.part`）。
    tmp_path: PathBuf,
    /// 被 stash 的旧版本（overwrite 场景；`.old`）；None = 目标原本不存在。
    old_path: Option<PathBuf>,
    /// stash 的 sidecar（`.old.meta`，审查 M3——恢复目标的卷内相对路径
    /// 落盘）。与 old_path 同生共死：任何收尾路径都要把两者一起清。
    old_meta: Option<PathBuf>,
    file: tokio::fs::File,
    /// 构造时的 size 承诺（WriteHint）；close 校验。
    hinted_size: Option<u64>,
    /// close/abort 已成功收尾（Drop 据此避免重复/误删清理）。
    finished: bool,
}

impl LocalStager {
    /// 装配暂存器（仅 driver::writer 调用——tmp/stash 创建逻辑在驱动侧）。
    pub(crate) fn new(
        volume: VolumeId,
        final_rel: RelPath,
        final_path: PathBuf,
        tmp_path: PathBuf,
        stashed: Option<(PathBuf, PathBuf)>,
        file: tokio::fs::File,
        hinted_size: Option<u64>,
    ) -> Self {
        LocalStager {
            volume,
            final_rel,
            final_path,
            tmp_path,
            old_path: stashed.as_ref().map(|(old, _)| old.clone()),
            old_meta: stashed.map(|(_, meta)| meta),
            file,
            hinted_size,
            finished: false,
        }
    }

    /// stash 收尾（async 形态，abort 用）：最终路径空 → 恢复旧版本；
    /// 已被他人占用 → 删 stash（不留孤儿）。
    async fn resolve_old_async(&self) {
        if let Some(old) = &self.old_path {
            if tokio::fs::metadata(&self.final_path).await.is_err() {
                let _ = tokio::fs::rename(old, &self.final_path).await;
            } else {
                let _ = tokio::fs::remove_file(old).await;
            }
        }
        if let Some(meta) = &self.old_meta {
            let _ = tokio::fs::remove_file(meta).await;
        }
    }
}

#[async_trait]
impl UploadStager for LocalStager {
    /// 追加一段数据到暂存文件（commit-on-close：此刻远端不可见——
    /// overwrite 场景旧版本已被 stash，同样不可见）。
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.file.write_all(data).await.map_err(map_io)
    }

    /// 提交：冲刷在途写 → 校验 size 承诺 → 建最终父目录 → 原子 rename →
    /// 删 stash → 返回 Entry。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        // tokio::fs::File 的 write_all 返回 Ok 只代表数据已入队后台 blocking
        // 写任务——flush 等待在途写真正落到句柄后，metadata 才可信
        self.file.flush().await.map_err(map_io)?;
        let actual = self.file.metadata().await.map_err(map_io)?.len();
        if let Some(hinted) = self.hinted_size {
            if actual != hinted {
                // WriteHint 契约：承诺与实际不符；tmp/stash 由随后的 Drop 收尾
                return Err(StorageError::Invalid);
            }
        }
        if let Some(parent) = self.final_path.parent() {
            // 隐式父目录（trait 契约：写入路径缺失父目录由驱动创建）
            tokio::fs::create_dir_all(parent).await.map_err(map_io)?;
        }
        // 同卷原子固化：Windows=MoveFileEx(REPLACE_EXISTING)、POSIX=rename(2)
        //——commit 的原子性即 rename 的原子性
        tokio::fs::rename(&self.tmp_path, &self.final_path)
            .await
            .map_err(map_io)?;
        self.finished = true;
        // 新版本已就位，stash 作废（best-effort——失败只留暂存区内不可见
        // 文件，不致 close 失败；sidecar 与 .old 同生共死，审查 M3）
        if let Some(old) = &self.old_path {
            let _ = tokio::fs::remove_file(old).await;
        }
        if let Some(meta) = &self.old_meta {
            let _ = tokio::fs::remove_file(meta).await;
        }
        let meta = tokio::fs::metadata(&self.final_path)
            .await
            .map_err(map_io)?;
        Ok(entry_from_meta(&self.volume, &self.final_rel, &meta))
    }

    /// 放弃：删暂存文件 + 恢复被 stash 的旧版本（幂等容错——暂存已消失
    /// 视为成功，契约「不留垃圾」仍成立）。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        match tokio::fs::remove_file(&self.tmp_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(map_io(e)),
        }
        self.resolve_old_async().await;
        self.finished = true;
        Ok(())
    }
}

impl Drop for LocalStager {
    /// 未 close/abort 即丢弃 → best-effort 收尾：删暂存 + 恢复 stash
    ///（abort 的同步镜像；Drop 无法 async，两次短调用是必然形态）。
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.tmp_path);
            if let Some(old) = &self.old_path {
                if !self.final_path.exists() {
                    let _ = std::fs::rename(old, &self.final_path);
                } else {
                    let _ = std::fs::remove_file(old);
                }
            }
            if let Some(meta) = &self.old_meta {
                let _ = std::fs::remove_file(meta);
            }
        }
    }
}
