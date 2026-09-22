//! URL 构造与防御（Phase 7 / WD1a；计划 §4.5-11 正面资产移植自
//! rs-f4ss `build_url`，按本仓风格重构）。
//!
//! WebDAV 的「路径」经 URL path 段传输——构造面的两条纪律：
//!
//! - **路径穿越防御**：组件拒 `..`/`.`/`\0` + 百分号解码后等于 `..`/
//!   `.` 的**编码穿越**拒（`%2e%2e`/`%2E%2e`——服务器侧解码后才判
//!   断的穿越不能靠编码形态蒙混）+ 双斜杠折叠（空段跳过）。词汇层
//!   （`RelPath`）已拒这些形态，这里是第二道门（Destination 头等
//!   直接拼 URL 的面不经 `RelPath`）；
//! - **恒定编码形态**：每段按 URL path 段字符集（unreserved + 子
//!   分隔符 + `:@`）百分号编码——空格→`%20`、非 ASCII→UTF-8 百分号
//!   形态、**字面 `%`→`%25`**（防服务器把字面百分号当转义解）。
//!
//! 服务器怪癖面（附录 C）：⑧ 集合（collection）操作对无尾斜杠 URL
//! apache 回 301 不执行——集合 URL 恒经 [`collection_url`] 补尾斜杠；
//! ⑩ apache 对相对 `Destination` 回 400——[`destination_uri`] 恒产
//! 绝对 URI。

use url::Url;

/// 单个十六进制字符的数值。
fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 手写百分号解码（`%XX` → 字节；非法/残缺序列原样保留，宽收）。
///
/// `url` crate 不 re-export `percent-encoding`——驱动面需要的就是这
/// 一个函数，不为此加直接依赖（xml.rs 的 href 解码共用）。
pub(crate) fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex_val),
                bytes.get(i + 2).copied().and_then(hex_val),
            ) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// URL path 段安全字符集：unreserved（`A-Za-z0-9-._~`）+ 子分隔符
/// （`!$&'()*+,;=`）+ `:@`。其余字节（含字面 `%`、空格、非 ASCII）
/// 一律 `%XX` 大写形态。
fn encode_path_segment(segment: &str) -> String {
    fn is_safe(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
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
                    | b':'
                    | b'@'
            )
    }
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if is_safe(byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 基地址 + 卷内相对路径 → 完整 URL（防御 + 编码，纯函数）。
///
/// - `path` 是 `/` 分隔的卷内相对路径（`RelPath::as_str()` 形态；
///   前导/尾随斜杠与空段容忍——空段折叠）；
/// - 每段过 [`encode_path_segment`]；base 的尾斜杠折叠后单斜杠连接；
/// - 空路径 → base 的尾斜杠形态（卷根的集合 URL 语义）。
pub fn join_path(base_url: &Url, path: &str) -> Result<Url, String> {
    let mut encoded = Vec::new();
    for raw in path.split('/') {
        if raw.is_empty() {
            continue; // 双斜杠/首尾斜杠折叠
        }
        if raw == ".." {
            return Err(format!(
                "webdav path segment {raw:?} is `..` — traversal beyond the volume root is \
                 rejected"
            ));
        }
        if raw == "." {
            return Err(format!(
                "webdav path segment {raw:?} is `.` — dot segments are not addressable"
            ));
        }
        if raw.contains('\0') {
            return Err(format!(
                "webdav path segment {raw:?} contains a NUL byte — not addressable"
            ));
        }
        let decoded = percent_decode_lossy(raw);
        if decoded == ".." || decoded == "." {
            return Err(format!(
                "webdav path segment {raw:?} percent-decodes to {decoded:?} — encoded \
                 traversal/dot segments are rejected"
            ));
        }
        encoded.push(encode_path_segment(raw));
    }
    let base_path = base_url.path().trim_end_matches('/');
    let full = if encoded.is_empty() {
        format!("{base_path}/")
    } else {
        format!("{base_path}/{}", encoded.join("/"))
    };
    let mut out = base_url.clone();
    out.set_path(&full);
    Ok(out)
}

/// 集合（collection）URL：确保尾斜杠（附录 C ⑧——apache 对无尾斜杠
/// 集合的 PROPFIND/MKCOL/DELETE/MOVE 回 301 不执行；rclone 全不敏感，
/// 多一带无害）。
pub fn collection_url(url: &Url) -> Url {
    if url.path().ends_with('/') {
        return url.clone();
    }
    let mut out = url.clone();
    let slashed = format!("{}/", out.path());
    out.set_path(&slashed);
    out
}

/// `Destination` 头的绝对 URI（附录 C ⑩——apache 拒相对 URI 400；
/// rclone 虽接受，恒绝对形态对两家皆真）。
///
/// 目录腿的 Destination 须带尾斜杠（附录 C ⑤）——调用方（WD3 写路径）
/// 以 [`collection_url`] 组合实现，本函数只管绝对 URI + 编码。
pub fn destination_uri(base: &Url, path: &str) -> Result<String, String> {
    Ok(join_path(base, path)?.to_string())
}
