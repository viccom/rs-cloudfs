//! WebDAV 上传暂存器（Phase 7 / WD3，计划 §4.6——与 ck-local/ck-sftp
//! 同构的 commit-on-close）。
//!
//! ## 语义链
//!
//! - **open_writer**（`WebdavDriver::writer`）：本地 NamedTempFile
//!   spool（drop 自动清理）；覆盖场景旧对象在打开时已 stash 成
//!   `.ckwd-*.old`（staging 窗口内目标不可见——断言①覆盖写腿）；
//! - **write**：spool 追加写入；超 hint 承诺 → `Invalid`（sftp hint
//!   契约同款）；
//! - **close 严格序**：整读 spool → 隐式建父 → `PUT
//!   <final>.ckwd-<pid>-<seq>.part`（**Content-Length 已知**——整件
//!   `Bytes` 形态，D5：chunked 直传挂账样本仅二不赌）→ `MOVE .part →
//!   final`（Overwrite:T）→ **stat 复核 size == written**（不符 →
//!   `Unavailable` 不静默）→ 删 stash（尽力）→ Entry；
//! - **重放窗防线**（K67 H2 同型，两半边）：PUT/MOVE 的 ACK 丢失（效果
//!   已落）→ stat 对账（.part 就位且 size 吻合 / .part 不在 + final
//!   就位且 size 吻合）→ **按已提交继续**；对账不成立 → 清理 + 现场
//!   恢复 + 如实上抛；
//! - **abort**：删 spool（NamedTempFile drop）+ stash→final 恢复
//!   （覆盖腿回到 writer 打开前状态）；`.part` 只在 close 链内产生且
//!   close 消费 self——abort 面上无远端 part 可清（sftp 判例的恢复
//!   语义由 close 失败路径的 [`WebdavStager::restore_scene`] 与 abort
//!   共用承担）；
//! - 失败路径：`.part` 尽力 DELETE + stash→final 复位（「删自己的、
//!   还别人的」序，sftp `restore_scene` 同款）；恢复失败只留不可见
//!   残件（list 过滤），不吞原错误。
//!
//! ## 已知上界（v1 如实声明）
//!
//! - close 整读 spool 进内存（PUT body = `Bytes`）——上传件尺寸 =
//!   内存驻留上界。D5 的 Content-Length 直传形态在 reqwest 面以此为
//!   最简实现；流式 file body（reqwest `stream` 特性 + 手设
//!   Content-Length 的协议耦合）挂账真机吞吐批复核后议。
//!
//! ## mtime 写策略（D2/D4）
//!
//! generic **不写**（WD0 实证两家服务器均不能真写）；nextcloud 的 PUT
//! 携 `X-OC-Mtime` 搭车——值为 spool 文件 mtime（数据落盘时刻，
//! epoch 秒；零成本保留协议面，fixture 无 Nextcloud 未实证）。
//!
//! - 语义：上表；
//! - 错误：传输/状态失败按计划 §4.4 映射；hint 承诺不符 → `Invalid`；
//!   提交后远端 size != written → `Unavailable`（不静默；sftp 同位
//!   校验用 `Io`——其 metadata 是真值，WebDAV 的 stat 报告本身可能
//!   不可信，归服务端异常面）；
//! - 并发：单 stager 串行使用（trait 契约）；
//! - 生命周期：close / abort / Drop（spool 本地自动清理；远端无残留
//!   ——close 前不触碰远端）三选一。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use cloudkit_storage::{Entry, RelPath, StorageError, UploadStager, VolumeId};

use crate::client::{MoveOutcome, WebdavClient};
use crate::config::Vendor;
use crate::driver::{ensure_parents, stat_pe};

/// WebDAV 上传暂存器（commit-on-close——模块文档语义链）。
pub struct WebdavStager {
    /// 写面共用客户端句柄（连接/协商世界与驱动同源）。
    client: Arc<WebdavClient>,
    /// 卷身份（close 产出的 Entry 归属）。
    volume: VolumeId,
    /// 服务端风味（nextcloud 的 X-OC-Mtime 搭车决策）。
    vendor: Vendor,
    /// 最终卷内路径（`.part`/stash 命名的基准）。
    final_rel: RelPath,
    /// 被 stash 的旧对象路径（`.ckwd-<pid>-<seq>.old`）；None = 新建腿。
    stash_rel: Option<RelPath>,
    /// 本次暂存的序号（`.part`/`.old` 同 seq——一个 stager 一个名族）。
    seq: u64,
    /// 本地 spool（NamedTempFile——drop 自动清理）。
    spool: tempfile::NamedTempFile,
    /// 已写入 spool 的字节数（close 的 size 复核基准）。
    written: u64,
    /// WriteHint 的 size 承诺（不符 → `Invalid`）。
    hinted_size: Option<u64>,
    /// close/abort 收尾标志（收尾后再写 = 非法状态）。
    finished: bool,
}

impl WebdavStager {
    /// 构造（`WebdavDriver::writer` 的装配面——stash 裁决与 spool 创建
    /// 归驱动，暂存器只收结果）。
    pub(crate) fn new(
        client: Arc<WebdavClient>,
        volume: VolumeId,
        vendor: Vendor,
        final_rel: RelPath,
        stash_rel: Option<RelPath>,
        seq: u64,
        hinted_size: Option<u64>,
    ) -> Result<Self, StorageError> {
        let spool = tempfile::NamedTempFile::new().map_err(|error| {
            StorageError::Io(format!("webdav upload spool create failed: {error}"))
        })?;
        Ok(WebdavStager {
            client,
            volume,
            vendor,
            final_rel,
            stash_rel,
            seq,
            spool,
            written: 0,
            hinted_size,
            finished: false,
        })
    }

    /// `.part` 暂存件路径（命名协议见 [`crate::driver`] 的
    /// `staging_rel`；与 stash 同 seq——一个 stager 一个名族）。
    fn part_rel(&self) -> Result<RelPath, StorageError> {
        crate::driver::staging_rel(&self.final_rel, self.seq, ".part")
    }

    /// spool mtime 的 epoch 秒（X-OC-Mtime 的值源——数据落盘时刻）。
    fn spool_mtime_secs(&self) -> Option<u64> {
        std::fs::metadata(self.spool.path())
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs()
            .into()
    }

    /// 现场恢复（close 失败 / abort 的共用收尾，sftp `restore_scene`
    /// 同款）：删自己的（.part，best-effort——多半已不在）、还别人的
    ///（stash→final 复位；复位失败只留不可见残件，sftp 判例同型——
    /// list 过滤兜底，数据仍在 stash 里）。
    async fn restore_scene(&self) {
        let part_rel = match self.part_rel() {
            Ok(rel) => rel,
            Err(_) => return, // 不可达防御（命名只依赖合法 final）
        };
        if let Ok(part_url) = crate::driver::url_for(&self.client, &part_rel) {
            match self.client.delete(&part_url).await {
                // 未落地（404）或已清：干净。
                Ok(()) | Err(StorageError::NotFound) => {}
                // 残件不可见（list 过滤）——只留告警。
                Err(error) => tracing::warn!(
                    target: "ck_webdav::stager",
                    part = %part_rel,
                    error = %error,
                    "webdav part cleanup left a filtered artifact behind"
                ),
            }
        }
        if let Some(stash) = &self.stash_rel {
            let urls = (
                crate::driver::url_for(&self.client, stash),
                crate::driver::url_for(&self.client, &self.final_rel),
            );
            if let (Ok(stash_url), Ok(final_url)) = urls {
                match self.client.move_(&stash_url, &final_url, true).await {
                    Ok(MoveOutcome::Done) => {}
                    _ => tracing::warn!(
                        target: "ck_webdav::stager",
                        stash = %stash,
                        "webdav stash restore failed; the old object stays parked at the \
                         filtered .old path"
                    ),
                }
            }
        }
    }
}

#[async_trait]
impl UploadStager for WebdavStager {
    /// 写入 spool（本地追加；超 hint 承诺 → `Invalid`——写后检查，
    /// pan115 同款：拉超量的字节本身已是无用工，失败要早）。
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        if self.finished {
            return Err(StorageError::Invalid); // 收尾后再写 = 非法状态
        }
        self.written = self
            .written
            .checked_add(data.len() as u64)
            .ok_or(StorageError::Invalid)?;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(self.spool.path())
            .await
            .map_err(|error| StorageError::Io(format!("webdav spool append: {error}")))?;
        file.write_all(data)
            .await
            .map_err(|error| StorageError::Io(format!("webdav spool write: {error}")))?;
        if let Some(hinted) = self.hinted_size {
            if self.written > hinted {
                return Err(StorageError::Invalid);
            }
        }
        Ok(())
    }

    /// 提交（严格序见模块文档；两半边 lost-ACK 对账 + 失败路径现场
    /// 恢复——sftp `restore_scene` 同款纪律：目标不得停在被 stash 丢空
    /// 的状态，恢复失败只留不可见残件，不吞原错误）。
    async fn close(mut self: Box<Self>) -> Result<Entry, StorageError> {
        self.finished = true;
        // WriteHint 契约（sftp 判例）：承诺与实际不符 → 恢复现场后
        // Invalid（超量在 write 已挡；此处收 underrun/漂移）。
        if let Some(hinted) = self.hinted_size {
            if hinted != self.written {
                self.restore_scene().await;
                return Err(StorageError::Invalid);
            }
        }
        // 字节源：整读 spool（v1 已知上界——模块文档）。
        let bytes = tokio::fs::read(self.spool.path())
            .await
            .map_err(|error| StorageError::Io(format!("webdav spool read: {error}")))?;
        let part_rel = self.part_rel()?;
        // ① 隐式建父（trait 契约；PUT 到缺父路径的 409 从严形态在桩上
        //    即暴露，真机两 fixture 亦不自动建父）。
        if let Err(error) = ensure_parents(&self.client, &self.final_rel).await {
            self.restore_scene().await;
            return Err(error);
        }
        // ② PUT .part（Content-Length 自带；nextcloud 搭车 X-OC-Mtime）。
        let mtime = match self.vendor {
            Vendor::Nextcloud => self.spool_mtime_secs(),
            Vendor::Generic => None,
        };
        if let Err(error) = self
            .client
            .put(
                &crate::driver::url_for(&self.client, &part_rel)?,
                Bytes::from(bytes),
                mtime,
            )
            .await
        {
            // lost-ACK 防线（PUT 半边，K67 H2 同型）：效果可能已落——
            // stat .part 对账，size 吻合则续链；未落则恢复现场后如实上抛。
            let landed = matches!(
                stat_pe(&self.client, &part_rel).await,
                Ok(pe) if pe.content_length.unwrap_or(0) == self.written
            );
            if !landed {
                self.restore_scene().await;
                return Err(error);
            }
            tracing::warn!(
                target: "ck_webdav::stager",
                path = %self.final_rel,
                "webdav PUT lost its ack but the part had landed; continuing the commit chain"
            );
        }
        // ③ MOVE .part → final（Overwrite:T——final 理论上不在（stash/
        //    新建），T 是竞态带的显式清扫）。
        let move_outcome = self
            .client
            .move_(
                &crate::driver::url_for(&self.client, &part_rel)?,
                &crate::driver::url_for(&self.client, &self.final_rel)?,
                true,
            )
            .await;
        let move_error: Option<StorageError> = match move_outcome {
            Ok(MoveOutcome::Done) => None,
            outcome => {
                // lost-ACK 防线（MOVE 半边——K67 H2 原型）：重放探测
                // .part 不在 + final 就位 + size 吻合 → 按已提交继续；
                // 否 = 真失败，恢复现场后报原错误（不探测直接恢复会把
                // 已提交的新版本覆盖回旧版——数据丢失，sftp 同型注释）。
                let committed = matches!(
                    stat_pe(&self.client, &part_rel).await,
                    Err(StorageError::NotFound)
                ) && matches!(
                    stat_pe(&self.client, &self.final_rel).await,
                    Ok(pe) if pe.content_length.unwrap_or(0) == self.written
                );
                if committed {
                    tracing::warn!(
                        target: "ck_webdav::stager",
                        path = %self.final_rel,
                        "webdav MOVE lost its ack but the commit had landed; resuming as committed"
                    );
                    None
                } else {
                    Some(match outcome {
                        Ok(other) => other.into_storage_error(),
                        Err(error) => error,
                    })
                }
            }
        };
        if let Some(error) = move_error {
            self.restore_scene().await;
            return Err(error);
        }
        // ④ stat 复核（size == written；不符 → Unavailable 不静默——
        //    stat_size_delta 注入面/服务端谎报的同型真实故障）。此时
        //    提交链已走完（final 就位）——不 restore（会删掉已提交的
        //    新版），只报异常。
        let pe = match stat_pe(&self.client, &self.final_rel).await {
            Ok(pe) => pe,
            Err(error) => {
                self.restore_scene().await;
                return Err(error);
            }
        };
        let remote_size = pe.content_length.unwrap_or(0);
        if remote_size != self.written {
            return Err(StorageError::Unavailable(format!(
                "webdav commit size disagreement: staged {} bytes but the remote reports \
                 {remote_size} — the commit chain is not verifiably intact",
                self.written
            )));
        }
        // ⑤ 清 stash（覆盖写：旧对象在提交成功后退役——尽力：失败仅留
        //    list 过滤的不可见残件，不影响提交事实）。
        if let Some(stash) = &self.stash_rel {
            if let Err(error) = self
                .client
                .delete(&crate::driver::url_for(&self.client, stash)?)
                .await
            {
                tracing::warn!(
                    target: "ck_webdav::stager",
                    stash = %stash,
                    error = %error,
                    "webdav stash retirement left a filtered .old artifact behind"
                );
            }
        }
        Ok(crate::driver::entry_from_propfind(
            &self.volume,
            &self.final_rel,
            &pe,
        ))
    }

    /// 放弃（回到 writer 打开前状态）：spool 由 NamedTempFile drop 清理；
    /// `.part` 只在 close 链内产生（close 消费 self）——abort 面上无远端
    /// part 可清；覆盖腿 stash→final 复位。
    async fn abort(mut self: Box<Self>) -> Result<(), StorageError> {
        self.finished = true;
        self.restore_scene().await;
        Ok(())
    }
}
