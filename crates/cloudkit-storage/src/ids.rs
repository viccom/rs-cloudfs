//! 卷与条目身份（foundation D6）。
//!
//! **v1 只有一个卷，但 ID 格式从第一天就带卷**——多卷时代（WebDAV 挂
//! `Y:=卷1, Z:=卷2` 或 union 根）L3+ 代码零返工的关键预留。
//!
//! `VolumeId` 形态：`"<scheme>:<key>"`，如 `telegram:<ns>` / `baidu:<uid>` /
//! `local:<规范化根路径>`。local 的身份即其根目录（同根多实例共享卷，
//! 换根 = 换卷）；根路径规范化归 ck-local 驱动，L2 只存储 opaque key
//! （因此 key 允许包含 `:`，如 Windows 盘符路径）。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::StorageError;

/// 卷身份 newtype：`scheme:key`（首个 `:` 前为 scheme，其余整体为 key）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VolumeId(String);

impl VolumeId {
    /// 由 scheme 与 key 组合构造（`format!("{scheme}:{key}")` 后走 [`parse`]
    /// 的同一校验）。
    pub fn new(scheme: &str, key: &str) -> Result<Self, StorageError> {
        VolumeId::parse(&format!("{scheme}:{key}"))
    }

    /// 解析 `scheme:key` 文本形态：首个 `:` 前为 scheme（限 ASCII 字母
    /// 数字与 `-`/`_`），其余整体为 key（非空，可再含 `:`）。
    pub fn parse(s: &str) -> Result<Self, StorageError> {
        // red-commit skeleton: 校验在绿提交落地
        Ok(VolumeId(s.to_string()))
    }

    /// 原始文本形态（`scheme:key`）。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// scheme 部分（首个 `:` 之前）。
    pub fn scheme(&self) -> &str {
        self.0.split(':').next().unwrap_or("")
    }

    /// key 部分（首个 `:` 之后，可再含 `:`，如 local 的 Windows 盘符路径）。
    pub fn key(&self) -> &str {
        match self.0.find(':') {
            Some(i) => &self.0[i + 1..],
            None => "",
        }
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for VolumeId {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        VolumeId::parse(s)
    }
}

/// 后端句柄：opaque 字符串（telegram msg_id / baidu fs_id / local inode 等）。
///
/// **serde 恒为字符串形态**——ID 类数值禁止经 JSON float（interfaces §5，
/// PCFS `%.0f` 科学计数法教训）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackendHandle(String);

impl BackendHandle {
    pub fn new(opaque: impl Into<String>) -> Self {
        BackendHandle(opaque.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BackendHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 全局条目身份 = (卷, 后端句柄)。
///
/// 句柄仅在所属卷内唯一；跨卷比较必须连卷一起比。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntryId {
    pub volume: VolumeId,
    pub handle: BackendHandle,
}

impl EntryId {
    pub fn new(volume: VolumeId, handle: BackendHandle) -> Self {
        EntryId { volume, handle }
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.volume, self.handle)
    }
}
