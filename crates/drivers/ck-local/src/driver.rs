//! LocalDriver——本地文件系统 StorageDriver（Batch L 红阶段骨架）。
//!
//! 当前状态：九方法全部 `Err(StorageError::Unsupported)` 占位，能力位恒
//! `Capabilities::none()`（R4：能力位绿阶段随 conformance 全绿逐位点亮，
//! 宁缺勿滥）。各方法契约的 conformance 断言编号见行内注释。

use std::path::PathBuf;

use async_trait::async_trait;

use cloudkit_storage::{
    ByteStream, Capabilities, Entry, EntryId, Listing, Page, Quota, Range, RelPath, StorageDriver,
    StorageError, UploadStager, VolumeId, WriteHint,
};

/// 本地文件系统驱动：卷根目录即后端。
pub struct LocalDriver {
    volume: VolumeId,
    #[allow(dead_code)] // Batch L 占位：九方法未读根路径；绿阶段 list/reader/writer 起读取
    root: PathBuf,
}

impl LocalDriver {
    /// 同步构造（测试与同步装配入口；async 装配走 [`crate::factory`]）：
    /// 创建根目录（幂等）→ 规范化为绝对路径 → 卷身份 `local:<root>`。
    ///
    /// Windows 上规范化产生 `\\?\` 扩展路径前缀——VolumeId 的 key 对
    /// L2 是 opaque 字符串（ids.rs D6），该形态合法。
    pub fn new(root: PathBuf) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&root)?;
        let canonical = std::fs::canonicalize(&root)?;
        let volume = VolumeId::new("local", &canonical.to_string_lossy())?;
        Ok(LocalDriver {
            volume,
            root: canonical,
        })
    }
}

#[async_trait]
impl StorageDriver for LocalDriver {
    fn volume(&self) -> &VolumeId {
        &self.volume
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::none() // R4：绿阶段逐位点亮（断言②⑦验证前不声明）
    }

    async fn list(&self, _dir: &RelPath, _page: Page) -> Result<Listing, StorageError> {
        Err(StorageError::Unsupported) // 断言③
    }

    async fn stat(&self, _path: &RelPath) -> Result<Entry, StorageError> {
        Err(StorageError::Unsupported) // 断言①③④⑥
    }

    async fn mkdir(&self, _path: &RelPath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported) // 断言①③④⑥
    }

    async fn delete(&self, _id: &EntryId) -> Result<(), StorageError> {
        Err(StorageError::Unsupported) // 断言④
    }

    async fn rename(&self, _from: &RelPath, _to: &RelPath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported) // 断言⑥
    }

    async fn reader(
        &self,
        _id: &EntryId,
        _range: Option<Range>,
    ) -> Result<ByteStream, StorageError> {
        Err(StorageError::Unsupported) // 断言①②⑥⑧
    }

    async fn writer(
        &self,
        _path: &RelPath,
        _hint: &WriteHint,
    ) -> Result<Box<dyn UploadStager>, StorageError> {
        Err(StorageError::Unsupported) // 断言①
    }

    async fn quota(&self) -> Result<Quota, StorageError> {
        Err(StorageError::Unsupported)
    }
}
