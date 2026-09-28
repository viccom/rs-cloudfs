//! 手搓 WebDAV 注入桩（Phase 7 / WD2a）——axum + 内存 VFS 的进程内服务端。
//!
//! **桩照协议真形建模，绝不照驱动实现抄**（K74/K77.2 三次真机教训——
//! 本仓最高测试纪律）。真值依据（按序）：
//!
//! 1. RFC 4918（multistatus/response/propstat/href/resourcetype 语义、
//!    MKCOL/MOVE/DELETE/PROPFIND 状态码语义）；
//! 2. RFC 7616（Digest challenge/response/stale/nc——服务端**真实验证**：
//!    HA1/HA2/response 重算比对，md-5）；
//! 3. RFC 9110（Range/206/416/Content-Range）；
//! 4. **WD0 真机怪癖矩阵**（`docs/tracking/phase7-webdav-fixture.md`——
//!    rclone serve webdav v1.60.1 / Apache mod_dav 2.4.58 双服务器实证；
//!    矩阵未覆盖的形态在注释里逐处声明桩的取舍）。
//!
//! 与 WD3 的 dav-server 参照桩（真实现）+ WD5 真机双服务器构成四实现
//! 交叉（计划 §1.2「双桩制」的手搓腿）。
//!
//! ## 与既有桩的形态差异（为什么不是 ck-pan115 的 JSON 面）
//!
//! WebDAV 动词（PROPFIND/MKCOL/MOVE/PROPPATCH）是非标准 HTTP method，
//! 经 `Router::fallback` 单点分发（method 路由表不认识它们）；请求路径
//! 是百分号编码的 wire 形态（href 编码往返的往返半边），所有资源定位
//! 先 `percent_decode` 再归一到无尾斜杠的规范 String 键。
//!
//! ## 观测面（WD2b 断言用）
//!
//! - [`StubHandle::requests`]：逐请求记录（method/path/打码后的
//!   authorization 形态/body_len + Digest nc/nonce 的结构化提取——
//!   nc 纪律与协商轮数的断言面）；
//! - [`StubHandle::snapshot`]/[`StubHandle::take`]：VFS 断言面；
//! - [`StubHandle::knobs`]：`kill_connections` 可运行期调整（原子计数）。
//!
//! ## 简化声明（矩阵未记真值处，逐点保守取舍）
//!
//! - GET 集合：rclone 形态回 200+HTML 目录页（真机自带浏览 UI 的回放），
//!   apache slashed→403（无 autoindex）、no-slash→301——矩阵「附加观察」；
//! - PUT 到父缺失路径→409（apache 从严形态；rclone 的 vfs-cache writes
//!   会自动建父——矩阵未记，从严让漏建父的驱动缺陷在桩上现形）；
//! - MKCOL 新路径（尚不存在）无尾斜杠：SlashStrict 下**执行**（301 仅
//!   对既有集合——mod_dir 重定向要求映射目标已是目录；矩阵只记了
//!   「已存在 no-slash→301」腿）；
//! - 多区间 Range（`a-b,c-d`）与不可解析 Range：按无 Range 处理回 200
//!   （宽容；倒序 `a>b` 才是矩阵记过的 416 腿）；
//! - PROPFIND 的 `Depth: infinity` 与缺头：按 Depth 1 处理（驱动恒发
//!   0/1；矩阵无真值）；
//! - MOVE 缺 Destination 头→400（RFC 4918 §9.9.2 要求；矩阵未记）。
//!
//! 共享测试支撑模块：不同集成测试各自只消费本桩 API 的子集——整体
//! 豁免 dead_code（ck-sftp 桩同款先例）。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::Router;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_core::Stream;
use md5::{Digest as Md5Digest, Md5};

// ------------------------------------------------------------ 常量锚 ---

/// seed 的缺省 mtime（= WD0 记录样本时刻——断言可直接钉
/// "Mon, 21 Sep 2026 10:51:05 GMT" 字面量）。
pub const MTIME_SEED: i64 = 1_789_987_865;
/// 写效果（PUT/MOVE）后的 mtime（+60s——与 seed 可辨，覆盖断言面）。
pub const MTIME_WRITE: i64 = MTIME_SEED + 60;
/// 目录条目的 getlastmodified（桩不追踪目录 mtime 变更——真源在文件侧）。
pub const MTIME_DIR: i64 = MTIME_SEED;
/// 请求体上限（PUT 大件测试面；与 ck-pan115 桩同值）。
const BODY_LIMIT: usize = 64 * 1024 * 1024;
/// 慢滴流的块大小（旋钮只控延迟，块大小定值）。
const DRIP_CHUNK: usize = 4096;
/// 计数流的块大小（M3 served_bytes 观测面——块越小停读计数越贴近
/// 客户端真实读取量）。
const COUNT_CHUNK: usize = 64 * 1024;
/// 目录条目内层 404 propstat 里 getcontentlength 的占位值：状态是 404 →
/// 值语义无效，999 与 WD1 记录的 apache 真形样本同值（lib.rs 测试常量
/// `APACHE_MULTISTATUS` 的 `<g0:getcontentlength>999</g0:getcontentlength>`）。
const DIR_LENGTH_PLACEHOLDER: usize = 999;
/// 桩请求体（自检/驱动测试共用的 allprop 形态；声明严格良构——apache
/// expat 对畸形声明回 400 是 WD0 副发现，桩不复制 rclone 的宽容）。
pub const ALLPROP_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8" ?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#
);

// ------------------------------------------------------------ 内存 VFS ---

/// 一条 VFS 条目的断言面快照形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VfsEntry {
    File { bytes: Vec<u8>, mtime: i64 },
    Dir { mtime: i64 },
}

/// 内存文件树：文件与目录都以「无尾斜杠的规范绝对路径」为键（根 = "/"）。
///
/// mtime 全部是确定性常量（seed 锚 [`MTIME_SEED`]/写效果锚
/// [`MTIME_WRITE`]）——PROPFIND 的 getlastmodified 断言无需时钟容忍。
#[derive(Clone)]
pub struct Vfs {
    files: BTreeMap<String, (Vec<u8>, i64)>,
    dirs: BTreeSet<String>,
}

impl Vfs {
    /// 空树（只含根目录）。故意不 derive Default：空 dirs 集会让 "/" 404。
    pub fn new() -> Vfs {
        Vfs {
            files: BTreeMap::new(),
            dirs: BTreeSet::from(["/".to_string()]),
        }
    }

    /// 放一个文件（缺省 mtime = [`MTIME_SEED`]；自动补全父目录）。
    pub fn seed_file(&mut self, path: &str, bytes: &[u8]) {
        self.seed_file_with_mtime(path, bytes, MTIME_SEED);
    }

    /// 放一个文件并指定 mtime（epoch 秒）。
    pub fn seed_file_with_mtime(&mut self, path: &str, bytes: &[u8], mtime: i64) {
        let path = normalize(path);
        self.ensure_parents(&path);
        self.files.insert(path, (bytes.to_vec(), mtime));
    }

    /// 建一个目录（自动补全父目录；已存在是幂等 no-op）。
    pub fn seed_dir(&mut self, path: &str) {
        let path = normalize(path);
        self.ensure_parents(&path);
        self.dirs.insert(path);
    }

    /// 取走一个文件的内容（消费语义：取后即删——断言「恰好传上来一次」
    /// 的组合面；目录返回 None）。
    pub fn take(&mut self, path: &str) -> Option<Vec<u8>> {
        self.files.remove(&normalize(path)).map(|(bytes, _)| bytes)
    }

    /// 路径存在（文件或目录）。
    pub fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        self.files.contains_key(&path) || self.dirs.contains(&path)
    }

    /// 全树快照（规范路径 → 条目；测试断言面）。
    pub fn snapshot(&self) -> BTreeMap<String, VfsEntry> {
        let mut out = BTreeMap::new();
        for (path, (bytes, mtime)) in &self.files {
            out.insert(
                path.clone(),
                VfsEntry::File {
                    bytes: bytes.clone(),
                    mtime: *mtime,
                },
            );
        }
        for path in &self.dirs {
            out.insert(path.clone(), VfsEntry::Dir { mtime: MTIME_DIR });
        }
        out
    }

    fn is_dir(&self, canonical: &str) -> bool {
        self.dirs.contains(canonical)
    }

    fn file(&self, canonical: &str) -> Option<&(Vec<u8>, i64)> {
        self.files.get(canonical)
    }

    /// 目录的直接子条目（名字序——BTreeMap/BTreeSet 的确定性迭代）：
    /// (名字, 是否目录, 文件长度, mtime)。
    fn children(&self, canonical: &str) -> Vec<(String, bool, usize, i64)> {
        let prefix = format!("{}/", canonical.trim_end_matches('/'));
        let mut out: Vec<(String, bool, usize, i64)> = Vec::new();
        for (path, (bytes, mtime)) in &self.files {
            if let Some(rest) = path.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    out.push((rest.to_string(), false, bytes.len(), *mtime));
                }
            }
        }
        for path in &self.dirs {
            if let Some(rest) = path.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    out.push((rest.to_string(), true, 0, MTIME_DIR));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn insert_file(&mut self, canonical: &str, bytes: Vec<u8>, mtime: i64) {
        self.files.insert(canonical.to_string(), (bytes, mtime));
    }

    fn insert_dir(&mut self, canonical: &str) {
        self.dirs.insert(canonical.to_string());
    }

    fn remove_subtree(&mut self, canonical: &str) {
        let prefix = format!("{canonical}/");
        self.files
            .retain(|path, _| path.as_str() != canonical && !path.starts_with(&prefix));
        self.dirs
            .retain(|path| path.as_str() != canonical && !path.starts_with(&prefix));
    }

    /// 整棵子树随路径键迁移（file 与 dir 同一机制；规范键前缀替换）。
    fn move_subtree(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let file_keys: Vec<String> = self
            .files
            .keys()
            .filter(|path| path.as_str() == from || path.starts_with(&prefix))
            .cloned()
            .collect();
        for key in file_keys {
            if let Some(value) = self.files.remove(&key) {
                self.files.insert(key.replacen(from, to, 1), value);
            }
        }
        let dir_keys: Vec<String> = self
            .dirs
            .iter()
            .filter(|path| path.as_str() == from || path.starts_with(&prefix))
            .cloned()
            .collect();
        for key in dir_keys {
            self.dirs.remove(&key);
            self.dirs.insert(key.replacen(from, to, 1));
        }
    }

    fn ensure_parents(&mut self, canonical: &str) {
        let mut current = parent_of(canonical);
        while current != "/" {
            self.dirs.insert(current.clone());
            current = parent_of(&current);
        }
    }
}

/// 路径归一：容忍缺省斜杠/双斜杠/尾斜杠 → 无尾斜杠规范绝对路径。
fn normalize(path: &str) -> String {
    let trimmed = path.trim();
    let body = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let segments: Vec<&str> = body.split('/').filter(|seg| !seg.is_empty()).collect();
    format!("/{}", segments.join("/"))
}

/// 父目录（"/a/b" → "/a"；"/a" 与 "/" → "/"）。
fn parent_of(canonical: &str) -> String {
    match canonical.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => canonical[..index].to_string(),
    }
}

// ------------------------------------------------------ href 编解码面 ---

/// 百分号解码（wire 形态 → VFS 键形态；非 UTF-8 字节 lossy——与驱动侧
/// 「list 产出即可寻址」的过滤面同一容忍度）。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// href 编码（VFS 键 → wire 形态）：unreserved + path 安全 sub-delims
/// 直通，其余按字节大写十六进制转义——空格→`%20`、`ü`→`%C3%BC`、
/// 字面 `%`→`%25`、`+`/`&` 直通（矩阵⑧ rclone 真形）。
fn href_encode(decoded: &str) -> String {
    let mut out = String::with_capacity(decoded.len());
    for &byte in decoded.as_bytes() {
        if byte == b'/' {
            out.push('/');
        } else if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'@'
                    | b':'
            )
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// XML 文本转义（编码后的 href 里只有 `&` 还可能存活——保持通用）。
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 条目的 wire href：文件无尾斜杠、目录带尾斜杠（双服务器一致真形）。
fn href_of(canonical: &str, is_dir: bool) -> String {
    let encoded = xml_escape(&href_encode(canonical));
    if is_dir && canonical != "/" {
        format!("{encoded}/")
    } else {
        encoded
    }
}

// --------------------------------------------------------- 时间格式面 ---

/// epoch 秒 → IMF-fixdate（getlastmodified 真形格式；httpdate 与生产
/// 同 crate）。
fn http_date(secs: i64) -> String {
    let time = UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64);
    httpdate::fmt_http_date(time)
}

/// epoch 秒 → ISO8601（apache creationdate 真形，矩阵⑧）。
fn iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// days-since-epoch → (年, 月, 日)（Howard Hinnant civil_from_days——
/// 无外部依赖的公历反解）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ------------------------------------------------------------ 认证面 ---

/// 桩的认证形态。
pub enum AuthMode {
    /// 无认证（缺省）。
    None,
    /// Basic：未带/错凭据 → 401 + `WWW-Authenticate: Basic realm="stub"`。
    Basic { user: String, pass: String },
    /// Digest（RFC 7616，服务端真实验证）：
    /// - 首请求 401 + challenge（realm/nonce/algorithm=MD5/qop="auth"，
    ///   nonce 为 base64 形态、恒含 `+` 与 `=`——矩阵⑨真机字符面）；
    /// - 正确 response → 放行并推进该 nonce 的 nc 高水位；
    /// - `enforce_nc = true`：nc 不严格递增（倒退/重放）→ 401（测客户端
    ///   nc 纪律）；false = apache 真形（矩阵⑨「nc 重放不查」）；
    /// - nonce 超过 `nonce_ttl` → 401 + 新 nonce；challenge 是否带
    ///   `stale=true` 由 `stale_after_expiry` 决定（apache 带——stale
    ///   再协商恰一次腿的载体）；
    /// - response-uri 必须逐字节等于 wire 请求 URI（apache 严格形态——
    ///   「uri 未转义」缺陷的桩侧暴露面）。
    Digest {
        user: String,
        pass: String,
        realm: String,
        nonce_ttl: Duration,
        enforce_nc: bool,
        stale_after_expiry: bool,
    },
    /// 恒回 `<scheme>` challenge、永不放行（如 NTLM——驱动明确不支持，
    /// 可行动拒绝文案的测试面）。
    Challenge { scheme: String },
}

/// 一个已签发 nonce 的服务端状态。
#[derive(Clone)]
struct NonceState {
    issued: Instant,
    /// 该 nonce 上见过的最大 nc（enforce_nc 的单调基准）。
    nc_high: Option<u64>,
}

// --------------------------------------------------------- 服务器形态 ---

/// multistatus 的 XML 命名空间风格。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NsStyle {
    /// rclone 真形：`<D:` 前缀 + `xmlns:D="DAV:"`（缺省）。
    #[default]
    Classic,
    /// apache 真形（矩阵⑧）：`<D:` 根 + 同文档 `ns0:`/`lp1:`/`g0:` 多
    /// 前缀并存（皆绑 "DAV:"）+ `lp2:` 绑 apache.org/dav/props/ +
    /// creationdate ISO8601——驱动解析必须按 local-name，前缀不可信。
    ApacheStyle,
}

/// 集合 URL 尾斜杠敏感度。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlashStyle {
    /// rclone 真形：尾斜杠全不敏感（目录/文件 ± 斜杠都 207）。
    #[default]
    SlashInsensitive,
    /// apache 真形：既有集合无尾斜杠 → 301 + Location（PROPFIND/GET/
    /// MKCOL/DELETE/MOVE 全动词不执行；文件 no-slash 正常）。
    SlashStrict,
}

/// MKCOL 撞已存在目录的回应（矩阵⑥）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MkcolExistsMode {
    /// rclone 真形：**201 幂等成功陷阱**（非 405——mkdir 必须预检）。
    Rclone201,
    /// RFC/apache 形态：405（缺省）。
    #[default]
    Rfc405,
}

/// PUT 到集合 URL 的回应（矩阵「附加观察」）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutCollectionMode {
    /// rclone 真形：404。
    Rclone404,
    /// apache 真形：409 "Cannot PUT to a collection"（缺省）。
    Apache409,
}

/// MOVE 缺 Overwrite 头且目标存在时的缺省语义（矩阵⑤：rclone 偏离 RFC）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverwriteDefault {
    /// rclone 真形：视同 F → 412。
    Rclone412,
    /// RFC 缺省：视同 T → 覆盖（缺省）。
    #[default]
    RfcT,
}

/// MOVE 目标父缺失的回应（矩阵⑤：rclone 403 / apache 500 非 409）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MoveMissingParent {
    /// rclone 真形：403（缺省）。
    #[default]
    Rclone403,
    /// apache 真形：500。
    Apache500,
    /// RFC 4918 形态：409（M13 注入面——客户端缺父三态处置的第三态
    /// 载体；矩阵未记 409 真形，按 RFC 4918 §9.9.4 语义建模）。
    Apache409,
}

/// PROPPATCH 的回应形态（矩阵①）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProppatchMode {
    /// rclone 真形：207 + 内层 403 + cannot-modify-protected-property
    /// （缺省——响亮的拒绝比假成功更适合做缺省）。
    #[default]
    Rclone403In207,
    /// apache lastmodified 形态：207 + 内层 200 但真实 mtime 不变（dead
    /// prop 假成功——桩从不改 VFS mtime，天然回放「假成功」）。
    ApacheDeadProp,
    /// apache getlastmodified 形态：207 + 内层 409（字面照记录
    /// `HTTP/1.1 409 (status)`——判码不判文案的活证据）。
    Apache409,
}

/// 服务器形态面（矩阵双服务器怪癖的可组合选择器）。
#[derive(Clone, Debug)]
pub struct StubStyle {
    pub ns: NsStyle,
    pub slash: SlashStyle,
    /// 开 = 一切 Range 头被无视回 200 全量（apache 倒序真形的强化版——
    /// 正常腿驱动不发倒序，但 200 截断回退路径要测；缺省 false）。
    pub range_ignore: bool,
    pub mkcol_exists: MkcolExistsMode,
    pub put_on_collection: PutCollectionMode,
    pub overwrite_default: OverwriteDefault,
    pub move_missing_parent: MoveMissingParent,
    pub proppatch: ProppatchMode,
    /// 开 = 相对 Destination → 400 拒（apache 真形；rclone 接受——
    /// 矩阵⑩；缺省 false）。
    pub reject_relative_destination: bool,
}

impl Default for StubStyle {
    /// 缺省 = RFC/从严形态（尾斜杠不敏感 + RFC 状态码 + 响亮拒绝）；
    /// 真机怪癖经 [`StubStyle::rclone`]/[`StubStyle::apache`] 预置或逐键开。
    fn default() -> Self {
        StubStyle {
            ns: NsStyle::default(),
            slash: SlashStyle::default(),
            range_ignore: false,
            mkcol_exists: MkcolExistsMode::default(),
            put_on_collection: PutCollectionMode::Apache409,
            overwrite_default: OverwriteDefault::default(),
            move_missing_parent: MoveMissingParent::default(),
            proppatch: ProppatchMode::default(),
            reject_relative_destination: false,
        }
    }
}

impl StubStyle {
    /// rclone serve webdav 真机预置（矩阵左列逐项）。
    pub fn rclone() -> Self {
        StubStyle {
            ns: NsStyle::Classic,
            slash: SlashStyle::SlashInsensitive,
            range_ignore: false,
            mkcol_exists: MkcolExistsMode::Rclone201,
            put_on_collection: PutCollectionMode::Rclone404,
            overwrite_default: OverwriteDefault::Rclone412,
            move_missing_parent: MoveMissingParent::Rclone403,
            proppatch: ProppatchMode::Rclone403In207,
            reject_relative_destination: false,
        }
    }

    /// Apache mod_dav 真机预置（矩阵右列逐项）。
    pub fn apache() -> Self {
        StubStyle {
            ns: NsStyle::ApacheStyle,
            slash: SlashStyle::SlashStrict,
            range_ignore: false,
            mkcol_exists: MkcolExistsMode::Rfc405,
            put_on_collection: PutCollectionMode::Apache409,
            overwrite_default: OverwriteDefault::RfcT,
            move_missing_parent: MoveMissingParent::Apache500,
            proppatch: ProppatchMode::ApacheDeadProp,
            reject_relative_destination: true,
        }
    }
}

// ---------------------------------------------------------- 故障旋钮 ---

/// 故障注入旋钮（全部可与任意 [`AuthMode`]/[`StubStyle`] 组合）。
///
/// 消耗语义：`kill_connections` 是原子计数（fetch_sub 消耗、可运行期经
/// [`StubHandle::knobs`] 再调整）；`transient_5xx`/`rate_limit_429`/
/// `lost_ack_after_effect` 是**启动时快照**（消耗副本在桩内递减——
/// lost-ACK 必须一次性，重放请求要能拿到正常响应才能测重放窗）。
#[derive(Default)]
pub struct Knobs {
    /// PROPFIND 回 207 + 硬畸形 XML（截断无闭合——真机 rclone 容忍
    /// apache 400 的强化版；驱动解析必须不崩不静默）。
    pub malformed_multistatus: bool,
    /// 计数大于 0 时逐请求消耗一次：响应头写出后 body 首块即错——hyper drop
    /// 连接（chunked 未闭合），客户端在 send 或读体处收到传输错误
    /// （连接建立期的 TCP reset 不在 axum 表达面——WD2b 直用未启桩/错
    /// 端口测 connect 族分类）。
    pub kill_connections: AtomicUsize,
    /// GET body 按块（4 KiB）延迟发送（窗口读不卡死/超时面）。
    pub slow_drip: Option<Duration>,
    /// 一次性（消费即复位）：**先执行效果再断连不回响应**——PUT/MOVE
    /// 的 lost-ACK 重放窗测试关键旋钮（K67 H2 同型）。
    pub lost_ack_after_effect: bool,
    /// 与 `lost_ack_after_effect` 组合：跳过前 N 个「效果型写请求」
    /// （PUT/MOVE）后才断下一个的 ACK——stager close 链（PUT .part →
    /// MOVE 固化）要把 lost-ACK 打在 **MOVE** 上（PUT 先行消耗首个槽）
    /// 的 WD3 断言面；0 = 现行语义（首个效果型请求即断）。
    pub lost_ack_after_effect_skip: usize,
    /// 一次性：下一个**文件目标的 PROPFIND**（Depth 0/1 的自条目）把
    /// getcontentlength 谎报为 `真实长度 + delta`（负值钳 0）——stager
    /// close 的 size 复核腿（不符 → Unavailable 不静默）注入面；服务器
    /// 谎报长度是同型真实故障形态。
    pub stat_size_delta: Option<i64>,
    /// 前 N 次 PROPFIND/GET 回 503（重试白名单自愈面）。
    pub transient_5xx: usize,
    /// 前 N 次 MOVE 回 503（WD3：rename 错误分类的注入面——K75-1 断言
    /// 腿「服务端瞬态 5xx 落在 MOVE 上」；与 `transient_5xx` 不共计数，
    /// 因 stat 预检的 PROPFIND 重试链会先吃光共享计数）。
    pub transient_5xx_move: usize,
    /// 计数大于 0 时逐 MOVE 递减；减到 0 的那次 MOVE 在处理前把**本次
    /// MOVE 的目标路径**种成文件——「建父窗内并发写手抢占目标」的竞态
    /// 建模（复审 M1 注入面：缺父 → 建父 → 重试撞 412 的构造面）。
    pub concurrent_target_on_move: usize,
    /// 与 `transient_5xx_move` 组合：先放过前 N 个 MOVE，之后
    /// `transient_5xx_move` 才开始生效——「503 恰落在**建父后的重试**
    /// 上」的注入面（复审 M4：重试臂错误折叠 NotFound 的红测构造）。
    pub transient_5xx_move_skip: usize,
    /// 计数大于 0 时逐 MOVE 递减（仅当目标父集合**存在**时消耗）：回
    /// 403——「rclone 对存在父的真 403 拒绝（认证类）落在重试上」的
    /// 注入面（复审 M4：重试臂 ParentSuspect 不得折叠 NotFound）。
    pub false_403_move_with_parent: usize,
    /// 前 N 次请求（全部动词）回 429，可带 Retry-After 秒数。
    pub rate_limit_429: Option<(usize, Option<u64>)>,
    /// 对**文件** PROPFIND 回 301 + Location（意外重定向的错误分类面；
    /// 目录不受影响）。
    pub unexpected_301: bool,
    /// OPTIONS 免认证（WD5 真机对策旋钮：rclone serve webdav 的 CORS
    /// preflight 语义——OPTIONS 不查凭据恒 200 + DAV/Allow 头，真机实证
    /// 错凭据亦然）。probe 的「Alive = 认证通过」判定必须由真认证动词
    /// 复核，本旋钮在桩上回放该真形。
    pub options_unauthenticated: bool,
    /// 一次性（启动快照，消费即复位）：下一个 PROPFIND 的**文件自条
    /// 目**或（目录 Depth 1 列举的）**首个子成员**改为「仅内层 500 块」
    /// 的失败成员——multistatus 里被列出但属性全数失败的真形（M7 注入
    /// 面）。目录自条目不受影响。
    pub member_500_once: bool,
    /// Digest 服务器的 challenge 改为**单头并置**形态（M6 注入面）：
    /// `Basic realm="stub", Digest realm=..., nonce=...`——RFC 7235 允许
    /// 的并置真形。缺省关（纯 Digest 单 challenge——矩阵⑨桩形态）。
    pub combined_basic_digest_challenge: bool,
    /// 文件 GET（200 全量/206 窗口）的**实际流出字节**计数（M3 观测
    /// 面）：客户端读多少（封顶读取 vs 全量整读）的可观测代理——读侧
    /// 中途停读时 hyper 停止拉流，计数即停。恒开启（纯观察）。
    pub served_bytes: AtomicUsize,
    /// Digest challenge 复用最近签发的 nonce（M1 注入面）：真形 = 时间
    /// 基 nonce 的服务器在同窗口内对**所有** challenge 发同一 nonce——
    /// 并发请求各自吃到的 401 携同一 nonce。缺省关（每 challenge 铸新
    /// nonce——矩阵⑨桩形态）。开启后 nonce 过期仍照常重铸（reuse 只在
    /// 现存 nonce 未过期时生效）。
    pub digest_challenge_reuse_nonce: bool,
    /// PROPFIND 专属连接杀（H1 注入面）：计数大于 0 时逐个 PROPFIND 消耗
    /// 一次「头已写、body 首块即断」。与 [`Knobs::kill_connections`]（全
    /// 动词）的差别：效果型写动词（PUT/MOVE）不消耗本计数——「MOVE 固
    /// 化之后的 stat 复核断连」只有 PROPFIND 专属计数才打得到（全动词
    /// 计数会先被 close 链上的 PUT/MOVE 吃掉）。运行期可调（同
    /// kill_connections）。
    pub kill_propfinds: AtomicUsize,
    /// 一次性（启动快照，消费即复位）：下一个**带 Range 头**的文件 GET
    /// 无条件回 416（Content-Range `bytes */total` 真形——satisfiable 与
    /// 否不影响）。416 复核决策腿的注入面（M10）：并发增长/收缩形态由
    /// 测试侧种入新版本后经真实 PROPFIND 复核浮现。无 Range 头的 GET 不
    /// 触发也不消耗。
    pub range_416_once: bool,
    /// 前 N 次 PROPFIND 回 403（M11 注入面——NAS 权限真形，apache GET
    /// 集合 403 的同族）。GET/写动词不消耗计数；403 不在重试白名单——
    /// 每次驱动面 PROPFIND 恰消耗 1。
    pub forbidden_propfinds: usize,
    /// 文件 GET 的 206 窗口把 Content-Range 相对请求**平移 +1**（M12 注
    /// 入面：头与请求不符——驱动 206 校验 mismatch 臂的载体；body 仍是
    /// 请求区间的真字节，撒谎只在头）。
    pub range_206_lie: bool,
    /// 文件 GET 的 206 窗口 Content-Range 正确但 **body 短一字节**（M12
    /// 注入面：驱动 206 长度校验臂的载体）。
    pub range_206_short: bool,
    /// 一次性（启动快照，消费即复位）：下一个 PUT 回 507 且**零效果**
    ///（先于任何 VFS 变更——配额满真形）。PUT 非幂等不入重试白名单，
    /// 每次驱动面 PUT 恰消耗 1。
    pub storage_full_once: bool,
}

/// 消耗型旋钮的运行副本（启动时从 [`Knobs`] 快照）。
struct FaultLedger {
    member_500: bool,
    transient_left: usize,
    transient_move_left: usize,
    transient_move_skip: usize,
    false_403_move_with_parent: usize,
    concurrent_target: usize,
    rate_left: Option<(usize, Option<u64>)>,
    lost_ack: bool,
    lost_ack_skip: usize,
    stat_size_delta: Option<i64>,
    range_416: bool,
    forbidden_left: usize,
    storage_full: bool,
}

// ------------------------------------------------------------ 记录器 ---

/// 一条已处理请求的记录（断言重试次数/动词序/协商轮数用）。
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    /// wire 形态路径（百分号编码原样、不含 query）。
    pub path: String,
    /// authorization 的打码形态：scheme + 凭据段前 8 字符 + `…`
    /// （`Basic c3Bpa2U6…`——形态可断言、值不落盘）。
    pub auth: Option<String>,
    pub body_len: usize,
    /// Authorization 里解析出的 Digest nc（nc 纪律断言面；非 Digest 请求
    /// 为 None）。
    pub digest_nc: Option<u64>,
    /// Authorization 里解析出的 nonce（服务器签发物，日志安全）。
    pub digest_nonce: Option<String>,
    /// `Overwrite` 头原样（WD3：恒显式 T/F 的断言面——矩阵⑤ rclone
    /// 缺头偏离行的驱动对策）。
    pub overwrite: Option<String>,
    /// `Destination` 头原样（WD3：恒绝对 URI 的断言面——矩阵⑩ apache
    /// 400 拒相对 URI 行的驱动对策）。
    pub destination: Option<String>,
    /// `X-OC-Mtime` 头原样（WD3：nextcloud 搭车断言面——D4/D2 修订；
    /// generic 恒 None）。
    pub x_oc_mtime: Option<String>,
    /// `Range` 头原样（M9：窗口 GET 逐窗核对 `bytes=a-b` 的断言面——
    /// 驱动丢 Range 头则 200 回退切出同字节、全绿假象的防线）。
    pub range: Option<String>,
}

fn mask_authorization(raw: &str) -> String {
    let (scheme, rest) = match raw.split_once(' ') {
        Some(parts) => parts,
        None => (raw, ""),
    };
    let head: String = rest.chars().take(8).collect();
    format!("{scheme} {head}…")
}

/// 从原始 Authorization 值里提取 `nc=XXXXXXXX`（打码面之外的独立观测）。
fn extract_nc(raw: &str) -> Option<u64> {
    let position = raw.find("nc=")?;
    let hex: String = raw[position + 3..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    u64::from_str_radix(&hex, 16).ok()
}

/// 从原始 Authorization 值里提取 nonce（引号内值）。
fn extract_nonce_param(raw: &str) -> Option<String> {
    let position = raw.find("nonce=")?;
    let rest = raw[position + "nonce=".len()..].strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// -------------------------------------------------------------- 状态 ---

struct Inner {
    vfs: Vfs,
    nonces: HashMap<String, NonceState>,
    nonce_counter: u64,
    /// 最近签发的 nonce（M1 reuse 旋钮的复用源；过期移除后自然失效）。
    last_nonce: Option<String>,
    requests: Vec<RecordedRequest>,
    ledger: FaultLedger,
}

/// 桩的共享状态（handler 与测试把手共用）。
pub struct StubState {
    pub knobs: Arc<Knobs>,
    style: StubStyle,
    auth: AuthMode,
    inner: Mutex<Inner>,
}

/// 单请求上下文（body 已集齐；后续全同步处理——锁不跨 await）。
struct Ctx {
    method: Method,
    /// wire 请求 URI（path+query，百分号编码原样）——Digest response-uri
    /// 严格校验的基准。
    wire_uri: String,
    wire_path: String,
    canonical: String,
    /// 请求 URL 是否带尾斜杠（SlashStrict 的 301 判据）。
    collection_form: bool,
    headers: HeaderMap,
    body: Bytes,
}

impl StubState {
    /// 毒锁恢复比卡死正确（ck-sftp 桩同款裁决）。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, method: &Method, wire_path: &str, headers: &HeaderMap, body_len: usize) {
        let raw_auth = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        // WD3 头观测面（纯观察，不参与任何路由/认证决策）。
        let header_of = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let record = RecordedRequest {
            method: method.as_str().to_string(),
            path: wire_path.to_string(),
            auth: raw_auth.as_deref().map(mask_authorization),
            body_len,
            digest_nc: raw_auth.as_deref().and_then(extract_nc),
            digest_nonce: raw_auth.as_deref().and_then(extract_nonce_param),
            overwrite: header_of("overwrite"),
            destination: header_of("destination"),
            x_oc_mtime: header_of("x-oc-mtime"),
            range: header_of("range"),
        };
        self.lock().requests.push(record);
    }

    // ------------------------------------------------------ 认证门 ---

    fn auth_gate(&self, ctx: &Ctx) -> Option<Response> {
        match &self.auth {
            AuthMode::None => None,
            AuthMode::Basic { user, pass } => {
                let sent = ctx
                    .headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(basic_credentials);
                match sent {
                    Some((sent_user, sent_pass)) if sent_user == *user && sent_pass == *pass => {
                        None
                    }
                    _ => Some(
                        Response::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .header("www-authenticate", r#"Basic realm="stub""#)
                            .body(Body::from("401 Unauthorized (stub)"))
                            .unwrap(),
                    ),
                }
            }
            AuthMode::Digest {
                user,
                pass,
                realm,
                nonce_ttl,
                enforce_nc,
                stale_after_expiry,
            } => {
                let authz = ctx
                    .headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                let reuse = self.knobs.digest_challenge_reuse_nonce;
                let combine = self.knobs.combined_basic_digest_challenge;
                let mut inner = self.lock();
                if !authz.to_ascii_lowercase().starts_with("digest ") {
                    // 含 Basic 预发（D1 首轮）与空手请求：一律 Digest challenge。
                    return Some(digest_challenge(&mut inner, realm, false, reuse, combine));
                }
                let params = parse_authorization_params(authz);
                let Some(nonce) = params.get("nonce").cloned() else {
                    return Some(digest_challenge(&mut inner, realm, false, reuse, combine));
                };
                let Some(nonce_state) = inner.nonces.get(&nonce).cloned() else {
                    // 未知 nonce：全新 challenge（不带 stale——凭据问题非过期）。
                    return Some(digest_challenge(&mut inner, realm, false, reuse, combine));
                };
                if Instant::now().duration_since(nonce_state.issued) > *nonce_ttl {
                    inner.nonces.remove(&nonce);
                    // 矩阵⑨：apache 过期 → 401 + stale=true + 新 nonce。
                    let stale = *stale_after_expiry;
                    return Some(digest_challenge(&mut inner, realm, stale, reuse, combine));
                }
                // nc 单调纪律（enforce_nc=true 时；apache 真机不查——false）。
                let nc = params
                    .get("nc")
                    .and_then(|value| u64::from_str_radix(value, 16).ok());
                if *enforce_nc {
                    if let (Some(nc), Some(high)) = (nc, nonce_state.nc_high) {
                        if nc <= high {
                            // nc 倒退/重放 = 凭据级拒绝（新 challenge 无 stale）。
                            return Some(digest_challenge(
                                &mut inner, realm, false, reuse, combine,
                            ));
                        }
                    }
                }
                // response-uri 严格校验（apache 形态）：必须逐字节等于 wire
                // 请求 URI（path+query）——「uri 未转义」缺陷的暴露面。
                if params.get("uri").map(String::as_str) != Some(ctx.wire_uri.as_str()) {
                    return Some(digest_challenge(&mut inner, realm, false, reuse, combine));
                }
                // RFC 7616 response 重算比对（qop 提供时 qop=auth 形态）。
                let expected = digest_response(
                    user,
                    pass,
                    realm,
                    ctx.method.as_str(),
                    &ctx.wire_uri,
                    &nonce,
                    nc.unwrap_or(0),
                    params.get("cnonce").map(String::as_str),
                    params.get("qop").map(String::as_str),
                );
                let response_ok = params
                    .get("response")
                    .is_some_and(|value| value.eq_ignore_ascii_case(&expected));
                let user_ok = params.get("username").is_some_and(|value| value == user);
                if !(response_ok && user_ok) {
                    return Some(digest_challenge(&mut inner, realm, false, reuse, combine));
                }
                if let Some(nc) = nc {
                    if let Some(state) = inner.nonces.get_mut(&nonce) {
                        state.nc_high = Some(state.nc_high.map_or(nc, |high| high.max(nc)));
                    }
                }
                None
            }
            AuthMode::Challenge { scheme } => Some(
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("www-authenticate", scheme.clone())
                    .body(Body::from("401 Unauthorized (stub)"))
                    .unwrap(),
            ),
        }
    }

    // ------------------------------------------------------ 故障门 ---

    /// 故障注入的检查序（组合时的优先级）：429 → 503 → 意外 301 → 连接杀。
    fn fault_gate(&self, ctx: &Ctx) -> Option<Response> {
        {
            let mut inner = self.lock();
            if let Some((left, retry_after)) = &mut inner.ledger.rate_left {
                if *left > 0 {
                    *left -= 1;
                    let mut builder = Response::builder().status(StatusCode::TOO_MANY_REQUESTS);
                    if let Some(seconds) = retry_after {
                        builder = builder.header("retry-after", seconds.to_string());
                    }
                    return Some(builder.body(Body::from("429 rate limited (stub)")).unwrap());
                }
            }
            // M11 旋钮：前 N 次 PROPFIND 回 403（权限真形；不在重试白名单
            // ——每次驱动面 PROPFIND 恰消耗 1；GET/写动词不消耗计数）。
            if ctx.method.as_str() == "PROPFIND" && inner.ledger.forbidden_left > 0 {
                inner.ledger.forbidden_left -= 1;
                return Some(
                    Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(Body::from("403 forbidden (stub)"))
                        .unwrap(),
                );
            }
            let idempotent_read = ctx.method.as_str() == "PROPFIND" || ctx.method.as_str() == "GET";
            if idempotent_read && inner.ledger.transient_left > 0 {
                inner.ledger.transient_left -= 1;
                return Some(
                    Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body(Body::from("503 transient (stub)"))
                        .unwrap(),
                );
            }
            // MOVE 专属瞬态（WD3）：与 transient_5xx 不共计数——rename 的
            // stat 预检在 MOVE 之前且 PROPFIND 重试链会把共享计数先吃光，
            // 「503 恰落在 MOVE 上」需要独立计数面。
            if ctx.method.as_str() == "MOVE" && inner.ledger.transient_move_left > 0 {
                if inner.ledger.transient_move_skip > 0 {
                    inner.ledger.transient_move_skip -= 1;
                } else {
                    inner.ledger.transient_move_left -= 1;
                    return Some(
                        Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::from("503 transient on MOVE (stub)"))
                            .unwrap(),
                    );
                }
            }
        }
        if self.knobs.unexpected_301 && ctx.method.as_str() == "PROPFIND" {
            let is_file = self.lock().vfs.file(&ctx.canonical).is_some();
            if is_file {
                // 指向「文件 + 尾斜杠」的畸形 Location——真机 301 都指集合，
                // 文件收到 301 本身就是意外重定向。
                let location = format!("{}/", href_encode(&ctx.canonical));
                return Some(redirect_response(&location));
            }
        }
        // PROPFIND 专属连接杀（H1 注入面）：PUT/MOVE 等写动词不消耗——
        // 「MOVE 固化后的 stat 复核断连」的定向注入面。
        if ctx.method.as_str() == "PROPFIND" {
            let current = self.knobs.kill_propfinds.load(Ordering::Acquire);
            if current > 0 {
                self.knobs.kill_propfinds.fetch_sub(1, Ordering::AcqRel);
                return Some(killed_response());
            }
        }
        let current = self.knobs.kill_connections.load(Ordering::Acquire);
        if current > 0 {
            self.knobs.kill_connections.fetch_sub(1, Ordering::AcqRel);
            return Some(killed_response());
        }
        None
    }

    // ------------------------------------------------------ 动词分发 ---

    fn route(&self, ctx: &Ctx) -> Response {
        match ctx.method.as_str() {
            "PROPFIND" => self.handle_propfind(ctx),
            "GET" => self.handle_get(ctx),
            "PUT" => self.handle_put(ctx),
            "MKCOL" => self.handle_mkcol(ctx),
            "DELETE" => self.handle_delete(ctx),
            "MOVE" => self.handle_move(ctx),
            "PROPPATCH" => self.handle_proppatch(ctx),
            "OPTIONS" => options_response(),
            // HEAD 等未建模动词：405 + Allow（驱动面不用 HEAD；矩阵无真值）。
            other => (
                StatusCode::METHOD_NOT_ALLOWED,
                format!("405 (stub): {other} not modeled"),
            )
                .into_response(),
        }
    }

    fn handle_propfind(&self, ctx: &Ctx) -> Response {
        let mut inner = self.lock();
        if !inner.vfs.exists(&ctx.canonical) {
            return (StatusCode::NOT_FOUND, "404 Not Found (stub)").into_response();
        }
        let is_dir = inner.vfs.is_dir(&ctx.canonical);
        if is_dir && self.style.slash == SlashStyle::SlashStrict && !ctx.collection_form {
            return redirect_response(&format!("{}/", href_encode(&ctx.canonical)));
        }
        if self.knobs.malformed_multistatus {
            // 硬畸形：截断无闭合（真机 rclone 只「容忍畸形声明」——桩按
            // 更坏形态回放，驱动解析必须不崩不静默）。
            return xml_response(
                StatusCode::MULTI_STATUS,
                concat!(
                    r#"<?xml version="1.0" encoding="utf-8"?>"#,
                    r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/trunc"#
                ),
            );
        }
        // quota 点名（RFC 4331）：请求体含 quota props → 自条目内层 404
        // propstat（矩阵⑪双服务器真形——驱动 quota None 降级的依据）。
        let body_text = String::from_utf8_lossy(&ctx.body);
        let quota_requested =
            body_text.contains("quota-used-bytes") || body_text.contains("quota-available-bytes");
        let depth0 = ctx
            .headers
            .get("depth")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim() == "0");

        let mut xml = String::new();
        xml.push_str(xml_declaration());
        xml.push_str(&multistatus_open(self.style.ns));
        if is_dir {
            // 目录条目的长度参数进内层 404 占位块（值语义无效）。
            xml.push_str(&propfind_entry(
                self.style.ns,
                &href_of(&ctx.canonical, true),
                true,
                MTIME_DIR,
                0,
                quota_requested,
            ));
            if !depth0 {
                for (name, child_is_dir, length, mtime) in inner.vfs.children(&ctx.canonical) {
                    let child = format!("{}/{}", ctx.canonical.trim_end_matches('/'), name);
                    // M7 旋钮（一次性）：首个子成员改为「仅内层 500 块」
                    // 的失败成员——list 面的成员失败映射注入腿。
                    if std::mem::take(&mut inner.ledger.member_500) {
                        xml.push_str(&propfind_entry_failed(&href_of(&child, child_is_dir)));
                        continue;
                    }
                    xml.push_str(&propfind_entry(
                        self.style.ns,
                        &href_of(&child, child_is_dir),
                        child_is_dir,
                        mtime,
                        length,
                        false,
                    ));
                }
            }
        } else {
            // M7 旋钮（一次性）：文件自条目改为「仅内层 500 块」的失败
            // 成员——stat 面的成员失败映射注入腿。
            if std::mem::take(&mut inner.ledger.member_500) {
                let href = href_of(&ctx.canonical, false);
                xml.push_str(&propfind_entry_failed(&href));
                xml.push_str("</D:multistatus>");
                return xml_response(StatusCode::MULTI_STATUS, &xml);
            }
            // vfs 借用在块内收口（stat_size_delta.take() 要独占 mutable
            // 借用 inner——先拷出 length/mtime 两标量再动 ledger）。
            let (mut length, mtime) = {
                let (bytes, mtime) = inner
                    .vfs
                    .file(&ctx.canonical)
                    .expect("exists checked above");
                (bytes.len(), *mtime)
            };
            // stat_size_delta（一次性）：谎报**文件目标**自条目的长度
            //（close 的 size 复核注入面；子条目不受影响——确定性优先）。
            if let Some(delta) = inner.ledger.stat_size_delta.take() {
                length = (length as i64 + delta).max(0) as usize;
            }
            xml.push_str(&propfind_entry(
                self.style.ns,
                &href_of(&ctx.canonical, false),
                false,
                mtime,
                length,
                quota_requested,
            ));
        }
        xml.push_str("</D:multistatus>");
        xml_response(StatusCode::MULTI_STATUS, &xml)
    }

    fn handle_get(&self, ctx: &Ctx) -> Response {
        let mut inner = self.lock();
        if !inner.vfs.exists(&ctx.canonical) {
            return (StatusCode::NOT_FOUND, "404 Not Found (stub)").into_response();
        }
        if inner.vfs.is_dir(&ctx.canonical) {
            if self.style.slash == SlashStyle::SlashStrict {
                if !ctx.collection_form {
                    return redirect_response(&format!("{}/", href_encode(&ctx.canonical)));
                }
                // apache 真形：无 autoindex → 403（矩阵「附加观察」）。
                return (
                    StatusCode::FORBIDDEN,
                    "403 Cannot GET a collection (stub, no autoindex)",
                )
                    .into_response();
            }
            // rclone 真形：200 + 自带浏览 UI 的 HTML 目录页。
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/html; charset=utf-8")
                .body(Body::from(format!(
                    "<html><body>stub index of {}</body></html>",
                    xml_escape(&ctx.canonical)
                )))
                .unwrap();
        }
        let (bytes, mtime) = inner
            .vfs
            .file(&ctx.canonical)
            .expect("exists checked above")
            .clone();
        // M10 旋钮（一次性，锁内消费）：下一个**带 Range 头**的文件 GET
        // 无条件 416 一次——复核决策腿注入面；无 Range 头不触发也不消耗。
        let force_416 =
            ctx.headers.get("range").is_some() && std::mem::take(&mut inner.ledger.range_416);
        drop(inner);
        let total = bytes.len() as u64;
        let last_modified = http_date(mtime);
        let range_header = ctx
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let drip = self.knobs.slow_drip;

        if self.style.range_ignore || range_header.is_none() {
            return file_response(bytes, &last_modified, drip, StatusCode::OK, &self.knobs);
        }
        if force_416 {
            // M10：无条件 416（Content-Range `bytes */total`——RFC 9110
            // 真形）。satisfiable 与否不影响——复核决策归客户端。
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{total}"))
                .body(Body::from("416 injected by range_416_once (stub)"))
                .unwrap();
        }
        match parse_range(range_header.as_deref().expect("checked above"), total) {
            RangeOutcome::Full => {
                file_response(bytes, &last_modified, drip, StatusCode::OK, &self.knobs)
            }
            RangeOutcome::Partial(start, end_inclusive) => {
                let mut slice = bytes[start as usize..=(end_inclusive as usize)].to_vec();
                let mut content_range = format!("bytes {start}-{end_inclusive}/{total}");
                // M12 旋钮（请求时读取，语义同 range_ignore）：撒谎只在
                // 头——Content-Range 相对请求整体平移 +1。
                if self.knobs.range_206_lie {
                    content_range = format!("bytes {}-{}/{}", start + 1, end_inclusive + 1, total);
                }
                // M12 旋钮：头正确、body 短窗口一字节。
                if self.knobs.range_206_short {
                    let keep = slice.len().saturating_sub(1);
                    slice.truncate(keep);
                }
                let builder = Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header("accept-ranges", "bytes")
                    .header("last-modified", last_modified)
                    .header("content-range", content_range);
                match drip {
                    // 慢滴流同样作用于 206 窗口（窗口读不卡死/超时面）。
                    Some(delay) => builder.body(Body::from_stream(DripStream::new(
                        slice,
                        delay,
                        Arc::clone(&self.knobs),
                    ))),
                    None => builder.body(Body::from_stream(CountingStream::new(
                        slice,
                        Arc::clone(&self.knobs),
                    ))),
                }
                .unwrap()
            }
            RangeOutcome::Unsatisfiable => Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{total}"))
                .body(Body::from("416 requested range not satisfiable (stub)"))
                .unwrap(),
        }
    }

    fn handle_put(&self, ctx: &Ctx) -> Response {
        let mut inner = self.lock();
        // M13 旋钮（一次性）：507 配额满——先于任何效果（拒绝语义，
        // VFS 零变更）。
        if std::mem::take(&mut inner.ledger.storage_full) {
            return (
                StatusCode::INSUFFICIENT_STORAGE,
                "507 insufficient storage (stub)",
            )
                .into_response();
        }
        if inner.vfs.is_dir(&ctx.canonical) {
            return match self.style.put_on_collection {
                PutCollectionMode::Rclone404 => {
                    (StatusCode::NOT_FOUND, "404 Not Found (stub)").into_response()
                }
                PutCollectionMode::Apache409 => (
                    StatusCode::CONFLICT,
                    "409 Cannot PUT to a collection (stub)",
                )
                    .into_response(),
            };
        }
        let parent = parent_of(&ctx.canonical);
        if !inner.vfs.is_dir(&parent) {
            // 从严（apache 形态）：漏建父目录的驱动缺陷必须在桩上现形。
            return (
                StatusCode::CONFLICT,
                "409 parent collection does not exist (stub)",
            )
                .into_response();
        }
        let existed = inner.vfs.exists(&ctx.canonical);
        inner
            .vfs
            .insert_file(&ctx.canonical, ctx.body.to_vec(), MTIME_WRITE);
        if inner.ledger.lost_ack {
            if inner.ledger.lost_ack_skip > 0 {
                // 让过这个效果型请求（slot 计数消耗），断下一个。
                inner.ledger.lost_ack_skip -= 1;
            } else {
                // lost-ACK：效果已落、响应即断（一次性——重放请求要能拿到
                // 正常响应才能测重放窗）。
                inner.ledger.lost_ack = false;
                return killed_response();
            }
        }
        if existed {
            (StatusCode::NO_CONTENT, "").into_response()
        } else {
            (StatusCode::CREATED, "").into_response()
        }
    }

    fn handle_mkcol(&self, ctx: &Ctx) -> Response {
        let mut inner = self.lock();
        if inner.vfs.file(&ctx.canonical).is_some() {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                "405 a resource exists at this URI (stub)",
            )
                .into_response();
        }
        if inner.vfs.is_dir(&ctx.canonical) {
            if self.style.slash == SlashStyle::SlashStrict && !ctx.collection_form {
                return redirect_response(&format!("{}/", href_encode(&ctx.canonical)));
            }
            return match self.style.mkcol_exists {
                MkcolExistsMode::Rclone201 => (StatusCode::CREATED, "").into_response(),
                MkcolExistsMode::Rfc405 => (
                    StatusCode::METHOD_NOT_ALLOWED,
                    "405 collection already exists (stub)",
                )
                    .into_response(),
            };
        }
        let parent = parent_of(&ctx.canonical);
        if !inner.vfs.is_dir(&parent) {
            return (
                StatusCode::CONFLICT,
                "409 parent collection does not exist (stub)",
            )
                .into_response();
        }
        inner.vfs.insert_dir(&ctx.canonical);
        (StatusCode::CREATED, "").into_response()
    }

    fn handle_delete(&self, ctx: &Ctx) -> Response {
        let mut inner = self.lock();
        if ctx.canonical == "/" {
            // 桩防线（真实服务器同拒根删除）：测试根误删会连桩一起带走。
            return (
                StatusCode::FORBIDDEN,
                "403 cannot DELETE the stub root (stub)",
            )
                .into_response();
        }
        if !inner.vfs.exists(&ctx.canonical) {
            return (StatusCode::NOT_FOUND, "404 Not Found (stub)").into_response();
        }
        if inner.vfs.is_dir(&ctx.canonical) {
            if self.style.slash == SlashStyle::SlashStrict && !ctx.collection_form {
                // apache 真形：no-slash 集合 301 且**不执行**（矩阵⑦）。
                return redirect_response(&format!("{}/", href_encode(&ctx.canonical)));
            }
            inner.vfs.remove_subtree(&ctx.canonical);
            return (StatusCode::NO_CONTENT, "").into_response();
        }
        inner.vfs.remove_subtree(&ctx.canonical);
        (StatusCode::NO_CONTENT, "").into_response()
    }

    fn handle_move(&self, ctx: &Ctx) -> Response {
        let Some(destination_raw) = ctx
            .headers
            .get("destination")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
        else {
            return (
                StatusCode::BAD_REQUEST,
                "400 Destination header is required (stub)",
            )
                .into_response();
        };
        let relative = !destination_raw.contains("://");
        if relative && self.style.reject_relative_destination {
            // apache 真形：相对 URI → 400 且移动不发生（矩阵⑩）。
            return (
                StatusCode::BAD_REQUEST,
                "400 Destination must be an absolute URI (stub)",
            )
                .into_response();
        }
        let destination = normalize(&percent_decode(&destination_path(&destination_raw)));
        let mut inner = self.lock();
        // M1 注入面（复审）：建父窗内的并发写手——计数递减到 0 的那次
        // MOVE 在处理前先把**本次目标**种成文件（真实竞态建模：首发缺
        // 父失败与隐式建父重试之间目标被他人创建 → 重试撞 412）。
        if inner.ledger.concurrent_target > 0 {
            inner.ledger.concurrent_target -= 1;
            if inner.ledger.concurrent_target == 0 {
                inner.vfs.seed_file(&destination, b"concurrent-writer");
            }
        }
        if !inner.vfs.exists(&ctx.canonical) {
            return (StatusCode::NOT_FOUND, "404 Not Found (stub)").into_response();
        }
        let source_is_dir = inner.vfs.is_dir(&ctx.canonical);
        if source_is_dir && self.style.slash == SlashStyle::SlashStrict && !ctx.collection_form {
            // apache 真形：目录源 no-slash → 301 且不执行（矩阵⑤）。
            return redirect_response(&format!("{}/", href_encode(&ctx.canonical)));
        }
        let destination_parent = parent_of(&destination);
        if inner.ledger.false_403_move_with_parent > 0 && inner.vfs.is_dir(&destination_parent) {
            // 父集合存在时的 403 = rclone 对真拒绝（认证类）的形态——
            // 复审 M4 注入面：重试臂撞此形态不得折叠 NotFound。
            inner.ledger.false_403_move_with_parent -= 1;
            return (
                StatusCode::FORBIDDEN,
                "403 denied with the parent present (stub)",
            )
                .into_response();
        }
        if !inner.vfs.is_dir(&destination_parent) {
            return match self.style.move_missing_parent {
                MoveMissingParent::Rclone403 => (
                    StatusCode::FORBIDDEN,
                    "403 destination parent does not exist (stub)",
                )
                    .into_response(),
                MoveMissingParent::Apache500 => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "500 destination parent does not exist (stub, apache form)",
                )
                    .into_response(),
                MoveMissingParent::Apache409 => (
                    StatusCode::CONFLICT,
                    "409 destination parent does not exist (stub, RFC form)",
                )
                    .into_response(),
            };
        }
        let destination_exists = inner.vfs.exists(&destination);
        let overwrite = match ctx
            .headers
            .get("overwrite")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
        {
            Some(value) if value.eq_ignore_ascii_case("F") => false,
            Some(value) if value.eq_ignore_ascii_case("T") => true,
            // 缺头语义按服务器形态（rclone 偏离 RFC 视同 F——矩阵⑤）。
            _ => self.style.overwrite_default == OverwriteDefault::RfcT,
        };
        if destination_exists && !overwrite {
            // apache 记录的 412 HTML 体形态（rclone 412 体未记——同体）。
            return Response::builder()
                .status(StatusCode::PRECONDITION_FAILED)
                .header("content-type", "text/html; charset=iso-8859-1")
                .body(Body::from(
                    "<!DOCTYPE HTML PUBLIC \"-//IETF//DTD HTML 2.0//EN\">\n\
                     <html><head><title>412 Precondition Failed</title></head>\n\
                     <body><h1>Destination is not empty</h1></body></html>",
                ))
                .unwrap();
        }
        if destination_exists {
            inner.vfs.remove_subtree(&destination);
        }
        inner.vfs.move_subtree(&ctx.canonical, &destination);
        if inner.ledger.lost_ack {
            if inner.ledger.lost_ack_skip > 0 {
                // 让过这个效果型请求（slot 计数消耗），断下一个。
                inner.ledger.lost_ack_skip -= 1;
            } else {
                // lost-ACK：效果已落、响应即断（一次性，K67 H2 重放窗）。
                inner.ledger.lost_ack = false;
                return killed_response();
            }
        }
        // 状态语义（spike 记录腿）：目标新建 → 201（目录腿文案
        // "Destination has been created" 是 apache 记录形态）；覆盖既有
        // 目标 → 204。文件/目录同规。
        if destination_exists {
            (StatusCode::NO_CONTENT, "").into_response()
        } else if source_is_dir {
            (StatusCode::CREATED, "Destination has been created (stub)").into_response()
        } else {
            (StatusCode::CREATED, "").into_response()
        }
    }

    fn handle_proppatch(&self, ctx: &Ctx) -> Response {
        let inner = self.lock();
        if !inner.vfs.exists(&ctx.canonical) {
            return (StatusCode::NOT_FOUND, "404 Not Found (stub)").into_response();
        }
        let is_dir = inner.vfs.is_dir(&ctx.canonical);
        let href = href_of(&ctx.canonical, is_dir);
        let propstat = match self.style.proppatch {
            // rclone 真形：内层 403 + cannot-modify-protected-property。
            ProppatchMode::Rclone403In207 => concat!(
                r#"<D:propstat><D:prop><D:getlastmodified/></D:prop>"#,
                r#"<D:status>HTTP/1.1 403 Forbidden</D:status>"#,
                r#"<D:responsedescription>cannot-modify-protected-property</D:responsedescription>"#,
                r#"</D:propstat>"#,
            ),
            // apache lastmodified 形态：内层 200 假成功（真实 mtime 不变——
            // 桩从不改 VFS mtime，天然回放）。
            ProppatchMode::ApacheDeadProp => concat!(
                r#"<D:propstat><D:prop><D:lastmodified/></D:prop>"#,
                r#"<D:status>HTTP/1.1 200 OK</D:status>"#,
                r#"</D:propstat>"#,
            ),
            // apache getlastmodified 形态：内层 409，字面照记录（判码不判
            // 文案的活证据——矩阵①）。
            ProppatchMode::Apache409 => concat!(
                r#"<D:propstat><D:prop><D:getlastmodified/></D:prop>"#,
                r#"<D:status>HTTP/1.1 409 (status)</D:status>"#,
                r#"</D:propstat>"#,
            ),
        };
        let body = format!(
            "{decl}<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>{href}</D:href>\
             {propstat}</D:response></D:multistatus>",
            decl = xml_declaration(),
        );
        xml_response(StatusCode::MULTI_STATUS, &body)
    }
}

// -------------------------------------------------------- digest 内部 ---

/// 生成并登记一个新 nonce（base64 形态，恒含 `+` 与 `=`——矩阵⑨真机
/// 字符面：材料 = 计数器 8 字节 + [0x00, 0xFB]，0xFB 落在末组首字节
///（index 9 ≡ 0 mod 3）→ 首六位组 0xFB>>2 = 62 = `+`；10 字节 ≡ 1
/// (mod 3) → `=` 填充）。`reuse_last`（M1 旋钮）开启且现存 nonce 未过
/// 期时复用最近签发的 nonce——并发 challenge 同 nonce 的真形回放。
/// `combine_basic`（M6 旋钮）把 challenge 改为单头并置形态
/// `Basic realm="stub", Digest ...`。
#[allow(clippy::too_many_arguments)]
fn digest_challenge(
    inner: &mut Inner,
    realm: &str,
    stale: bool,
    reuse_last: bool,
    combine_basic: bool,
) -> Response {
    let nonce = if reuse_last {
        inner
            .last_nonce
            .clone()
            .filter(|nonce| inner.nonces.contains_key(nonce))
    } else {
        None
    };
    let nonce = match nonce {
        Some(nonce) => nonce,
        None => {
            inner.nonce_counter += 1;
            let mut material = inner.nonce_counter.to_le_bytes().to_vec();
            material.extend_from_slice(&[0x00, 0xFB]);
            let nonce = BASE64.encode(material);
            inner.nonces.insert(
                nonce.clone(),
                NonceState {
                    issued: Instant::now(),
                    nc_high: None,
                },
            );
            inner.last_nonce = Some(nonce.clone());
            nonce
        }
    };
    // 形态照矩阵⑨记录：realm/nonce/algorithm=MD5/qop="auth"；
    // stale=true 位置不定（实测在 algorithm 后）——按实测位次回放。
    // M6 旋钮：并置头形态（Basic 段在前——RFC 7235 单头多 challenge）。
    let mut value = if combine_basic {
        r#"Basic realm="stub", "#.to_string()
    } else {
        String::new()
    };
    value.push_str(&format!(
        r#"Digest realm="{realm}", nonce="{nonce}", algorithm=MD5"#
    ));
    if stale {
        value.push_str(", stale=true");
    }
    value.push_str(r#", qop="auth""#);
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", value)
        .body(Body::from("401 Unauthorized (stub)"))
        .unwrap()
}

fn basic_credentials(header: &str) -> Option<(String, String)> {
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = BASE64.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// 解析 Authorization: Digest 参数（引号感知——qop 列表内逗号不裂；
/// 与驱动侧解析器同规则但独立实现，桩不 import 驱动符号）。
fn parse_authorization_params(header: &str) -> HashMap<String, String> {
    fn push_param(current: &mut String, out: &mut HashMap<String, String>) {
        if let Some((key, value)) = current.trim().split_once('=') {
            let value = value.trim();
            let value = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
                value[1..value.len() - 1].to_string()
            } else {
                value.to_string()
            };
            out.insert(key.trim().to_ascii_lowercase(), value);
        }
        current.clear();
    }
    let mut out = HashMap::new();
    let Some((_, rest)) = header.split_once(' ') else {
        return out;
    };
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if in_quotes && ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            push_param(&mut current, &mut out);
            continue;
        }
        current.push(ch);
    }
    push_param(&mut current, &mut out);
    out
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// RFC 7616 MD5 response（qop 提供时 qop=auth 形态，否则 RFC 2069 形态）。
#[allow(clippy::too_many_arguments)] // RFC 公式的参数面（WD1 src/auth.rs 同款先例）
fn digest_response(
    user: &str,
    pass: &str,
    realm: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    nc: u64,
    cnonce: Option<&str>,
    qop: Option<&str>,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match (qop, cnonce) {
        (Some(qop), Some(cnonce)) => {
            md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:{qop}:{ha2}"))
        }
        _ => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

// ------------------------------------------------------------- XML 面 ---

fn xml_declaration() -> &'static str {
    r#"<?xml version="1.0" encoding="utf-8"?>"#
}

fn multistatus_open(ns: NsStyle) -> String {
    match ns {
        NsStyle::Classic => r#"<D:multistatus xmlns:D="DAV:">"#.to_string(),
        // apache 真形（WD1 lib.rs 记录样本逐字节）：D:/ns0:/lp1:/g0: 皆绑
        // "DAV:"，lp2 绑 apache 私有 props 命名空间。
        NsStyle::ApacheStyle => concat!(
            r#"<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:" xmlns:lp1="DAV:" "#,
            r#"xmlns:g0="DAV:" xmlns:lp2="http://apache.org/dav/props/">"#
        )
        .to_string(),
    }
}

/// 一个 response 条目。目录的 getcontentlength 在内层 404 propstat 是
/// 双服务器一致真形（矩阵⑧）；apache 风格 404 块在 200 块**之前**、
/// 且前缀按记录样本混用（ns0/lp1/g0）——驱动解析必须按 local-name。
fn propfind_entry(
    ns: NsStyle,
    href: &str,
    is_dir: bool,
    mtime: i64,
    file_length: usize,
    quota_404: bool,
) -> String {
    let last_modified = http_date(mtime);
    let mut out = format!("<D:response><D:href>{href}</D:href>");
    match ns {
        NsStyle::Classic => {
            if is_dir {
                out.push_str(
                    r#"<D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype>"#,
                );
                out.push_str(&format!(
                    "<D:getlastmodified>{last_modified}</D:getlastmodified>"
                ));
                out.push_str(r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#);
                out.push_str(&format!(
                    "<D:propstat><D:prop><D:getcontentlength>{DIR_LENGTH_PLACEHOLDER}\
                     </D:getcontentlength></D:prop>\
                     <D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"
                ));
            } else {
                out.push_str(r#"<D:propstat><D:prop><D:resourcetype/>"#);
                out.push_str(&format!(
                    "<D:getcontentlength>{file_length}</D:getcontentlength>\
                     <D:getlastmodified>{last_modified}</D:getlastmodified>"
                ));
                out.push_str(r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#);
            }
            if quota_404 {
                out.push_str(concat!(
                    r#"<D:propstat><D:prop><D:quota-used-bytes/>"#,
                    r#"<D:quota-available-bytes/></D:prop>"#,
                    r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"#,
                ));
            }
        }
        NsStyle::ApacheStyle => {
            if is_dir {
                // 404 块在前（记录样本顺序）。
                out.push_str(&format!(
                    "<D:propstat><D:prop><g0:getcontentlength>{DIR_LENGTH_PLACEHOLDER}\
                     </g0:getcontentlength></D:prop>\
                     <D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"
                ));
                out.push_str(
                    r#"<D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype>"#,
                );
                out.push_str(&format!(
                    "<lp1:getlastmodified>{last_modified}</lp1:getlastmodified>\
                     <lp1:creationdate>{}</lp1:creationdate>",
                    iso8601(mtime)
                ));
                out.push_str(
                    r#"<lp2:executable F/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#,
                );
            } else {
                out.push_str(r#"<D:propstat><D:prop><ns0:resourcetype/>"#);
                out.push_str(&format!(
                    "<lp1:getcontentlength>{file_length}</lp1:getcontentlength>\
                     <lp1:getlastmodified>{last_modified}</lp1:getlastmodified>\
                     <lp1:creationdate>{}</lp1:creationdate>",
                    iso8601(mtime)
                ));
                out.push_str(r#"</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"#);
            }
            if quota_404 {
                out.push_str(concat!(
                    r#"<D:propstat><D:prop><g0:quota-used-bytes/>"#,
                    r#"<g0:quota-available-bytes/></D:prop>"#,
                    r#"<D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"#,
                ));
            }
        }
    }
    out.push_str("</D:response>");
    out
}

/// 失败成员的 response 形态（M7 旋钮载体）：仅一个非 2xx propstat 块
/// ——成员「在 multistatus 里被列出但属性全数失败」的真形。
fn propfind_entry_failed(href: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href>         <D:propstat><D:prop><D:resourcetype/>         <D:getcontentlength>0</D:getcontentlength></D:prop>         <D:status>HTTP/1.1 500 Internal Server Error</D:status></D:propstat>         </D:response>"
    )
}

fn xml_response(status: StatusCode, xml: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/xml; charset=utf-8")
        .body(Body::from(xml.to_string()))
        .unwrap()
}

fn redirect_response(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("location", location)
        .body(Body::empty())
        .unwrap()
}

fn options_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            "allow",
            "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE",
        )
        .header("dav", "1")
        .body(Body::empty())
        .unwrap()
}

/// 200 全量文件响应（慢滴流时按块延迟流出；否则走计数流——M3
/// served_bytes 观测面挂在无延迟路径上）。
fn file_response(
    bytes: Vec<u8>,
    last_modified: &str,
    drip: Option<Duration>,
    status: StatusCode,
    knobs: &Arc<Knobs>,
) -> Response {
    let builder = Response::builder()
        .status(status)
        .header("accept-ranges", "bytes")
        .header("last-modified", last_modified);
    if let Some(delay) = drip {
        builder
            .body(Body::from_stream(DripStream::new(
                bytes,
                delay,
                Arc::clone(knobs),
            )))
            .unwrap()
    } else {
        builder
            .body(Body::from_stream(CountingStream::new(
                bytes,
                Arc::clone(knobs),
            )))
            .unwrap()
    }
}

// ------------------------------------------------------------ Range 面 ---

enum RangeOutcome {
    /// Range 不可用/被忽略 → 200 全量。
    Full,
    /// 206（start..=end_inclusive，end 已钳制）。
    Partial(u64, u64),
    /// 416（越 EOF 起点/倒序/零长度后缀）。
    Unsatisfiable,
}

/// 单区间 `bytes=a-b` 解析（多区间与不可解析按 Full 处理——简化声明）。
fn parse_range(header: &str, total: u64) -> RangeOutcome {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Full;
    };
    if spec.contains(',') {
        return RangeOutcome::Full;
    }
    let Some((start_text, end_text)) = spec.split_once('-') else {
        return RangeOutcome::Full;
    };
    // 后缀形态 `bytes=-n`（最后 n 字节；n=0 → 416；n≥size → 全量）。
    if start_text.is_empty() {
        let Ok(suffix) = end_text.trim().parse::<u64>() else {
            return RangeOutcome::Full;
        };
        if suffix == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        let start = total.saturating_sub(suffix);
        if start >= total {
            return RangeOutcome::Unsatisfiable;
        }
        return RangeOutcome::Partial(start, total - 1);
    }
    let Ok(start) = start_text.trim().parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if start >= total {
        // 越 EOF 起点 → 416（RFC 9110 §14.2；矩阵④真机同形）。
        return RangeOutcome::Unsatisfiable;
    }
    if end_text.trim().is_empty() {
        return RangeOutcome::Partial(start, total - 1);
    }
    let Ok(end) = end_text.trim().parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if end < start {
        // 倒序区间：rclone 真形 416（apache 200 全量经 range_ignore 旋钮
        // 覆盖——正常腿驱动不发倒序）。
        return RangeOutcome::Unsatisfiable;
    }
    // 末端钳制（矩阵④：90-999999 → 90-99/100）。
    RangeOutcome::Partial(start, end.min(total - 1))
}

/// Destination 头 → 路径段（剥 scheme://authority 与 query/fragment；
/// 相对形态原样取路径——rclone 接受腿）。
fn destination_path(destination: &str) -> String {
    let base = destination.split(['?', '#']).next().unwrap_or(destination);
    match base.find("://") {
        Some(scheme_end) => {
            let authority = &base[scheme_end + 3..];
            match authority.find('/') {
                Some(path_start) => authority[path_start..].to_string(),
                None => "/".to_string(),
            }
        }
        None => base.to_string(),
    }
}

// ------------------------------------------------------------ 流式 body ---

/// 逐请求消耗的「连接被杀」响应：头已写出、body 首块即错——hyper drop
/// 连接（chunked 未闭合）。客户端在 send 或读体处收到传输错误。
fn killed_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from_stream(KillStream))
        .unwrap()
}

/// 首 poll 出错随即终止的 body 流（连接杀的实现载体）。
struct KillStream;

impl Stream for KillStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(Some(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "stub: connection killed by knob",
        ))))
    }
}

/// 慢滴流：每块（4 KiB）发送前睡 `delay`（首块也睡——慢服务器的真实
/// 形态；headers 先行到达）。Sleep 自带 PhantomPinned，装箱固定；
/// DripStream 本体因此保持 Unpin（`get_mut` 可用）。
struct DripStream {
    data: Bytes,
    pos: usize,
    delay: Duration,
    pending: Option<Pin<Box<tokio::time::Sleep>>>,
    /// M3 served_bytes 观测面（限速路径同样计数——慢滴下服务端自限，
    /// 计数贴近客户端真实读取量）。
    knobs: Arc<Knobs>,
}

impl DripStream {
    fn new(data: Vec<u8>, delay: Duration, knobs: Arc<Knobs>) -> DripStream {
        DripStream {
            data: Bytes::from(data),
            pos: 0,
            delay,
            pending: None,
            knobs,
        }
    }
}

impl Stream for DripStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.pos >= this.data.len() {
            return Poll::Ready(None);
        }
        if this.pending.is_none() {
            this.pending = Some(Box::pin(tokio::time::sleep(this.delay)));
        }
        let sleep = this.pending.as_mut().expect("just set");
        if sleep.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        let end = (this.pos + DRIP_CHUNK).min(this.data.len());
        let chunk = this.data.slice(this.pos..end);
        this.pos = end;
        this.knobs
            .served_bytes
            .fetch_add(chunk.len(), Ordering::Relaxed);
        this.pending = None;
        Poll::Ready(Some(Ok(chunk)))
    }
}

/// 计数流（M3 观测面）：把 body 按固定块发出并累计**实际流出**的字节
/// 数——客户端读多少（封顶读取 vs 全量整读）的可观测代理。客户端中途
/// 停读时 hyper 停止拉流（body drop），计数即停在真实交付量附近（块
/// 粒度以内）。
struct CountingStream {
    data: Bytes,
    pos: usize,
    knobs: Arc<Knobs>,
}

impl CountingStream {
    fn new(data: Vec<u8>, knobs: Arc<Knobs>) -> CountingStream {
        CountingStream {
            data: Bytes::from(data),
            pos: 0,
            knobs,
        }
    }
}

impl Stream for CountingStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.pos >= this.data.len() {
            return Poll::Ready(None);
        }
        let end = (this.pos + COUNT_CHUNK).min(this.data.len());
        let chunk = this.data.slice(this.pos..end);
        this.pos = end;
        this.knobs
            .served_bytes
            .fetch_add(chunk.len(), Ordering::Relaxed);
        Poll::Ready(Some(Ok(chunk)))
    }
}

// -------------------------------------------------------------- 分发 ---

async fn dispatch(State(state): State<Arc<StubState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("stub: body read failed: {error}"),
            )
                .into_response()
        }
    };
    let wire_path = parts.uri.path().to_string();
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let decoded = percent_decode(&wire_path);
    let collection_form = decoded.ends_with('/');
    let ctx = Ctx {
        method: parts.method,
        wire_uri: format!("{wire_path}{query}"),
        wire_path,
        canonical: normalize(&decoded),
        collection_form,
        headers: parts.headers,
        body,
    };
    // 记录先于认证门：401/挑战轮数本身是断言面（矩阵⑨协商轮次）。
    state.record(&ctx.method, &ctx.wire_path, &ctx.headers, ctx.body.len());
    // WD5 旋钮：rclone 对 OPTIONS 免认证（CORS preflight）——认证门前放行。
    let options_exempt = state.knobs.options_unauthenticated && ctx.method.as_str() == "OPTIONS";
    if !options_exempt {
        if let Some(response) = state.auth_gate(&ctx) {
            return response;
        }
    }
    if let Some(response) = state.fault_gate(&ctx) {
        return response;
    }
    state.route(&ctx)
}

// -------------------------------------------------------------- 启动器 ---

/// 桩把手：URL + 观测面 + 优雅停机。
pub struct StubHandle {
    /// 桩基地址（尾斜杠形态——直接作为驱动的 webdav_url）。
    pub url: String,
    /// 旋钮把手（kill_connections 可运行期调整；其余为启动快照语义）。
    pub knobs: Arc<Knobs>,
    state: Arc<StubState>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl StubHandle {
    /// 请求记录（到达序）。
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().requests.clone()
    }

    /// VFS 全树快照（断言面）。
    pub fn snapshot(&self) -> BTreeMap<String, VfsEntry> {
        self.state.lock().vfs.snapshot()
    }

    pub fn exists(&self, path: &str) -> bool {
        self.state.lock().vfs.exists(path)
    }

    /// 消费语义的文件读取（取后即删）。
    pub fn take(&self, path: &str) -> Option<Vec<u8>> {
        self.state.lock().vfs.take(path)
    }

    /// 起桩后补种文件/目录（测试准备面）。
    pub fn seed_file(&self, path: &str, bytes: &[u8]) {
        self.state.lock().vfs.seed_file(path, bytes);
    }

    pub fn seed_dir(&self, path: &str) {
        self.state.lock().vfs.seed_dir(path);
    }

    /// 优雅停机（5s 预算——慢滴流在途连接不悬挂测试进程；超时后放弃
    /// 等待，任务随测试进程退出）。
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
        }
    }
}

/// 起桩：`127.0.0.1:0` 随机端口 + tokio spawn + graceful shutdown
/// （ck-pan115 stub_common 形态）。
pub async fn spawn_stub(vfs: Vfs, auth: AuthMode, knobs: Knobs, style: StubStyle) -> StubHandle {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("stub local addr");
    let url = format!("http://{addr}/");
    let knobs = Arc::new(knobs);
        let ledger = FaultLedger {
            member_500: knobs.member_500_once,
            transient_left: knobs.transient_5xx,
            transient_move_left: knobs.transient_5xx_move,
            transient_move_skip: knobs.transient_5xx_move_skip,
            false_403_move_with_parent: knobs.false_403_move_with_parent,
            concurrent_target: knobs.concurrent_target_on_move,
        rate_left: knobs.rate_limit_429,
        lost_ack: knobs.lost_ack_after_effect,
        lost_ack_skip: knobs.lost_ack_after_effect_skip,
        stat_size_delta: knobs.stat_size_delta,
        range_416: knobs.range_416_once,
        forbidden_left: knobs.forbidden_propfinds,
        storage_full: knobs.storage_full_once,
    };
    let state = Arc::new(StubState {
        knobs: knobs.clone(),
        style,
        auth,
        inner: Mutex::new(Inner {
            vfs,
            nonces: HashMap::new(),
            nonce_counter: 0,
            last_nonce: None,
            requests: Vec::new(),
            ledger,
        }),
    });
    let app = Router::new().fallback(dispatch).with_state(state.clone());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .expect("stub serve loop");
    });
    StubHandle {
        url,
        knobs,
        state,
        shutdown_tx: Some(tx),
        join: Some(join),
    }
}
