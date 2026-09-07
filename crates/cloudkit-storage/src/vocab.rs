//! 公共词汇类型（interfaces.md §5）：`Range`/`Page`/`Entry` 等放 L2 统一定义，
//! 上层与驱动一律复用，禁止各层重复发明。
//!
//! 所有类型均为纯数据（无 IO），可跨层/跨线程自由传递；serde 形态遵守
//! interfaces §4（可选字段 `default` + `skip_serializing_if`）与 §5（ID 字符串
//! 化、时间戳 f64 epoch 秒）。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::StorageError;
use crate::ids::EntryId;

/// 卷内相对路径（POSIX 风格 `/` 分隔，无根前缀）。
///
/// 语义契约：
/// - 根目录 = 空路径（`RelPath::root()`，`as_str()` 为 `""`，`Display` 为 `/`）；
/// - 组件非空且不为 `.`/`..`，不允许 `\` 与 `\0`（跨平台混淆防线）；
/// - 排序即字典序（`Ord`），驱动 `list` 的「稳定有序」以此为准。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    /// 根目录路径（每个卷的唯一根）。
    pub fn root() -> Self {
        RelPath(String::new())
    }

    /// 解析并校验一个相对路径；非法形态（绝对前缀/`..`/空组件/反斜杠等）
    /// 返回 [`StorageError::Invalid`]。
    pub fn new(s: &str) -> Result<Self, StorageError> {
        if s.is_empty() {
            return Ok(RelPath::root());
        }
        if s.starts_with('/') || s.ends_with('/') || s.contains('\\') || s.contains('\0') {
            return Err(StorageError::Invalid);
        }
        for comp in s.split('/') {
            if comp.is_empty() || comp == "." || comp == ".." {
                return Err(StorageError::Invalid);
            }
        }
        Ok(RelPath(s.to_string()))
    }

    /// 是否为根目录。
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// 原始字符串形态（根目录为 `""`；展示用 [`fmt::Display`] 的 `/`）。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 逐组件迭代（根目录产出空迭代器）。
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// 父目录路径；根目录无父。
    pub fn parent(&self) -> Option<RelPath> {
        if self.is_root() {
            return None;
        }
        match self.0.rfind('/') {
            Some(i) => Some(RelPath(self.0[..i].to_string())),
            None => Some(RelPath::root()),
        }
    }

    /// 末段组件名；根目录无文件名。
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            return None;
        }
        self.0.rsplit('/').next()
    }

    /// 追加一个组件（组件本身必须合法：非空、不含 `/`、非 `.`/`..`）。
    pub fn join(&self, name: &str) -> Result<RelPath, StorageError> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(StorageError::Invalid);
        }
        let mut s = self.0.clone();
        if !s.is_empty() {
            s.push('/');
        }
        s.push_str(name);
        Ok(RelPath(s))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str("/")
        } else {
            f.write_str(&self.0)
        }
    }
}

impl FromStr for RelPath {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RelPath::new(s)
    }
}

/// 字节区间，**半开区间** `[start, end)` 语义（interfaces §2）。
///
/// - `end = None` 表示开放区间（读到 EOF）；
/// - `end` 越界时由驱动**钳制到 EOF**（conformance 断言②钉死）；
/// - `start >= end`（end 为 Some 时）为非法构造 → `Invalid`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}

impl Range {
    /// 构造并校验：`end` 为 `Some` 且 `<= start` 时返回 `Invalid`。
    pub fn new(start: u64, end: Option<u64>) -> Result<Self, StorageError> {
        if let Some(e) = end {
            if e < start {
                return Err(StorageError::Invalid);
            }
        }
        Ok(Range { start, end })
    }

    /// 开放区间 `[start, EOF)`。
    pub fn from_start(start: u64) -> Self {
        Range { start, end: None }
    }

    /// 在已知内容长度 `available` 下的实际读取长度（越界钳制后的）；
    /// `start >= available` 时为 0。
    pub fn clamped_len(&self, available: u64) -> u64 {
        if available == 0 || self.start >= available {
            return 0;
        }
        let end = self.end.unwrap_or(available).min(available);
        end - self.start
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.end {
            Some(e) => write!(f, "[{},{})", self.start, e),
            None => write!(f, "[{},EOF)", self.start),
        }
    }
}

/// 分页请求（interfaces §2：分页语义统一在 Page，驱动不得自行发明分页参数）。
///
/// `limit` 是上限（驱动可以返回更少）；`cursor` 由驱动在 [`Listing::next`]
/// 回吐、调用方原样回放——游标/offset 两种后端都被归一到这一形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub limit: usize,
    pub cursor: PageCursor,
}

impl Page {
    /// 不分页（一次取完）。
    pub fn all() -> Self {
        Page {
            limit: usize::MAX,
            cursor: PageCursor::Start,
        }
    }
}

impl Default for Page {
    fn default() -> Self {
        Page::all()
    }
}

/// 分页游标：`Start` 起步，之后回放驱动回吐的 `Next` 不透明令牌。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageCursor {
    Start,
    Next(String),
}

/// `list` 的响应形态：本页条目 + 续读游标（`None` = 已穷尽）。
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub next: Option<PageCursor>,
}

/// 目录条目元数据（驱动 `stat`/`list` 的统一产出）。
///
/// 时间戳为 f64 epoch 秒（rs-CyDrive 数据模型延续，interfaces §5）；
/// `id.handle` 为后端字符串句柄（fs_id/msg_id 等），**永不经 JSON float**
/// （PCFS `%.0f` 教训，interfaces §5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub path: RelPath,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
}

/// 后端配额视图；`total = None` 表示未知/无上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub used: u64,
}

impl Quota {
    /// 剩余可用（饱和减，未知 total 时为 0——调用方应先看 `total`）。
    pub fn available(&self) -> u64 {
        self.total.map(|t| t.saturating_sub(self.used)).unwrap_or(0)
    }
}

/// 上传提示（writer 的可选先验信息）。
///
/// - `size`：调用方承诺的最终字节数（驱动可用于预分配/分块规划/会话复用
///   匹配）；承诺与实际不符时驱动可拒绝（`Invalid`）；
/// - `content_hash`：明文内容哈希（算法前缀形态，如 `md5:<hex>`），
///   供 [`crate::optional::RapidUpload`] 秒传探测；
/// - `rapid_upload`：调用方提示该内容大概率可秒传（允许驱动先行探测）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteHint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rapid_upload: bool,
}

/// 读取字节流：`futures` Stream，项为字节块；**错误可在流中途浮现**
/// （连接中断/token 失效等），消费方必须处理 `Err` 项而非只看首个结果。
///
/// 形态裁决：`futures_core::Stream<Item = Result<Bytes, StorageError>>`
/// 而非 `tokio::io::AsyncRead`——后者无法承载分类学错误（R2）；
/// `Bytes` 块零拷贝传递。AsyncRead 适配属上层（L4 WebDAV）职责。
pub type ByteStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<bytes::Bytes, StorageError>> + Send>>;
