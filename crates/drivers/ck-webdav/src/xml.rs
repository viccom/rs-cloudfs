//! multistatus 解析（Phase 7 / WD1a；自 WD0 spike `xml.rs` 移植并增强）。
//!
//! **local-name 解析纪律**（附录 C ⑧）：同一文档里 `D:`/`ns0:`/`lp1:`
//! /`g0:` 多前缀并存（皆绑 `DAV:`——apache 按属性轮换前缀），前缀不可
//! 信——元素名在**最后一个 `:` 之后**的部分判定语义。结构注意：
//! `<propstat>` 内 `<prop>` 先于 `<status>`（RFC 4918 顺序），per-prop
//! 行记 propstat 序号，status 收尾后回填关联。
//!
//! 相对 spike 的两处增强（本批 TDD 钉死）：
//!
//! 1. **href 反转义 + 解码**：实体反转义（quick-xml unescape：`&amp;`
//!    → `&`）后百分号解码（`%20`→空格、`%C3%BC`→`ü`；**`+` 不动**——
//!    WD0 实证 href 里 `+` 是字面量，只有 query 串才解释为空格）；
//! 2. **非 2xx propstat 行不进条目投影**：rclone 对目录的
//!    `getcontentlength` 在内层 404 propstat（附录 C ⑧）——404 块的
//!    空值（乃至错值）不能覆盖 200 块的真值（投影按 propstat 状态
//!    过滤，顺序无关）。
//!
//! 配套 [`is_addressable_name`]（K67 过滤纪律）：「list 产出即可寻址」
//! ——`\`/`\0`/非 UTF-8 lossy 形态（U+FFFD）的名字不可见。

use std::collections::HashMap;

use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::Reader;

use crate::urls::percent_decode_lossy;

/// 一条展平的 prop 观察：所属 href、propstat 状态行、prop 名、文本
/// 内容、`resourcetype` 是否带 `collection` 子元素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PropRow {
    pub href: String,
    pub propstat_status: Option<String>,
    pub prop: String,
    pub text: String,
    pub collection_child: bool,
}

/// 一个 PROPFIND response 条目在驱动需要字段上的投影。
///
/// `quota_*` 两字段是 RFC 4331 quota 点名的承载（WD2b quota 面）：内层
/// 404 propstat 的行不进投影（[`entries`] 的过滤纪律）→ `None` 即驱动
/// 的「降级到未知」信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PropfindEntry {
    pub href: String,
    pub is_collection: bool,
    pub content_length: Option<u64>,
    pub last_modified: Option<String>,
    pub quota_used_bytes: Option<u64>,
    pub quota_available_bytes: Option<u64>,
}

/// 元素限定名 → local name（最后一个 `:` 之后；无前缀原样——
/// 多段式前缀也不可混淆判定）。
fn local_name(qname: QName<'_>) -> String {
    let raw = String::from_utf8_lossy(qname.as_ref());
    match raw.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => raw.into_owned(),
    }
}

/// href 的实体反转义 + 百分号解码（增强 ①）。
///
/// 实体反转义失败（非法实体）原样保留——宽收：怪名走
/// [`is_addressable_name`] 与驱动面兜底，不在解析层硬失败。
fn decode_href(raw: &str) -> String {
    let unescaped = match unescape(raw) {
        Ok(cow) => cow.into_owned(),
        Err(_) => raw.to_string(),
    };
    percent_decode_lossy(&unescaped)
}

/// propstat 状态行里的 HTTP 码（`HTTP/1.1 404 Not Found` → 404；缺失或
/// 不可解析 → None——按权威行处置，与 [`row_is_authoritative`] 同判据）。
fn row_status_code(row: &PropRow) -> Option<u16> {
    row.propstat_status
        .as_deref()
        .and_then(|status| status.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
}

/// prop 行是否可采信：propstat 状态行缺失（裸 prop 形态）或 2xx。
fn row_is_authoritative(row: &PropRow) -> bool {
    row_status_code(row)
        .map(|code| (200..300).contains(&code))
        .unwrap_or(true)
}

/// 仅非 2xx propstat 块的 response 成员（M7）：href + 首个非 2xx 状态
/// 码。这些成员不进 [`entries`] 投影（既有纪律），但也不再静默消失
/// ——调用方按成员映射（内层 404 → NotFound、其余 → Unavailable 带码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedMember {
    pub href: String,
    pub status: u16,
}

/// M7：逐 response href 扫描——没有任何权威行（2xx/缺状态行）的成员即
/// 失败成员，状态取行序里首个非 2xx 码。href → 归属 `HashMap` 一次建
/// 立（M5 同纪律：成员数万级时不做二次方扫描）；产出按首见序。
pub(crate) fn failed_members(rows: &[PropRow]) -> Vec<FailedMember> {
    let mut order: Vec<String> = Vec::new();
    // href -> (有权威行?, 首个非 2xx 码)
    let mut members: HashMap<String, (bool, Option<u16>)> = HashMap::new();
    for row in rows {
        let entry = members.entry(row.href.clone()).or_insert_with(|| {
            order.push(row.href.clone());
            (false, None)
        });
        match row_status_code(row) {
            None => entry.0 = true,
            Some(code) if (200..300).contains(&code) => entry.0 = true,
            Some(code) => {
                if entry.1.is_none() {
                    entry.1 = Some(code);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|href| match members.get(&href) {
            Some((false, first_error)) => Some(FailedMember {
                href,
                // 失败成员必然至少有一行已解析的非 2xx 码（不可解析状
                // 态行按权威行处置）；防御缺省不触发。
                status: first_error.unwrap_or(500),
            }),
            _ => None,
        })
        .collect()
}

/// Text/CData 的公共下沉（三个汇聚槽：href / status / 当前 prop 文本）。
#[allow(clippy::too_many_arguments)]
fn sink_text(
    chunk: &str,
    href_sinking: bool,
    status_sinking: bool,
    prop_sinking: bool,
    href: &mut String,
    status_buf: &mut String,
    cur_text: &mut String,
) {
    if href_sinking {
        href.push_str(chunk);
    } else if status_sinking {
        status_buf.push_str(chunk);
    } else if prop_sinking {
        cur_text.push_str(chunk);
    }
}

/// 207 multistatus 体 → 展平 prop 行（非 multistatus 体产出空集；
/// quick-xml 错误转 `String` 载荷）。
///
/// **截断检测（WD2b 增强，桩 malformed_multistatus 旋钮的行为面）**：
/// quick-xml 对无闭合的截断文档在 Eof 处温和收尾（不报错）——纯解析
/// 会把半份 multistatus 当空集/半集吞掉（「不崩不静默」的静默半边）。
/// 此处在 Eof 上检查元素栈非空 → `Err`（截断即失败，片段进 `Io` 载荷
/// 由调用方带上）。良构的非 multistatus 体（如 HTML 错误页）栈在 Eof
/// 时为空 → 维持「空集宽收」的既有契约（WD1a 测试钉死，零漂移）。
pub(crate) fn parse_multistatus(xml: &str) -> Result<Vec<PropRow>, String> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut href = String::new();
    let mut href_sinking = false;
    let mut status_buf = String::new();
    let mut status_sinking = false;
    // statuses[propstat_n] 在该 propstat 的 <status> 收尾时回填。
    let mut statuses: Vec<Option<String>> = Vec::new();
    let mut propstat_n: usize = 0;
    let mut cur_prop: Option<String> = None;
    let mut cur_text = String::new();
    let mut collection_child = false;
    // (href, propstat 序号, prop, text, collection_child)
    let mut raw: Vec<(String, usize, String, String, bool)> = Vec::new();

    loop {
        match reader.read_event().map_err(|error| error.to_string())? {
            Event::Start(event) => {
                let name = local_name(event.name());
                let parent = stack.last().cloned().unwrap_or_default();
                if name == "response" {
                    href.clear();
                } else if name == "propstat" {
                    propstat_n = statuses.len();
                    statuses.push(None);
                } else if name == "href" && parent == "response" {
                    href_sinking = true;
                } else if name == "status" && parent == "propstat" {
                    status_sinking = true;
                    status_buf.clear();
                } else if name == "collection" && parent == "resourcetype" {
                    collection_child = true;
                } else if parent == "prop" && cur_prop.is_none() {
                    cur_prop = Some(name.clone());
                    cur_text.clear();
                    collection_child = false;
                }
                stack.push(name);
            }
            Event::Empty(event) => {
                let name = local_name(event.name());
                let parent = stack.last().cloned().unwrap_or_default();
                if parent == "prop" && cur_prop.is_none() {
                    raw.push((href.clone(), propstat_n, name, String::new(), false));
                } else if parent == "resourcetype" && name == "collection" {
                    collection_child = true;
                }
            }
            Event::Text(text) => {
                let chunk = String::from_utf8_lossy(text.as_ref()).trim().to_string();
                sink_text(
                    &chunk,
                    href_sinking,
                    status_sinking,
                    cur_prop.is_some(),
                    &mut href,
                    &mut status_buf,
                    &mut cur_text,
                );
            }
            Event::CData(text) => {
                let chunk = String::from_utf8_lossy(text.as_ref()).trim().to_string();
                sink_text(
                    &chunk,
                    href_sinking,
                    status_sinking,
                    cur_prop.is_some(),
                    &mut href,
                    &mut status_buf,
                    &mut cur_text,
                );
            }
            Event::End(_) => {
                let name = stack.pop().unwrap_or_default();
                if name == "href" {
                    href_sinking = false;
                    href = decode_href(&href);
                } else if name == "status" {
                    status_sinking = false;
                    if propstat_n < statuses.len() {
                        statuses[propstat_n] = Some(status_buf.trim().to_string());
                    }
                } else if let Some(prop) = cur_prop.clone() {
                    if prop == name {
                        raw.push((
                            href.clone(),
                            propstat_n,
                            prop,
                            cur_text.trim().to_string(),
                            collection_child,
                        ));
                        cur_prop = None;
                        cur_text.clear();
                        collection_child = false;
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    // 截断检测（见函数文档）：Eof 时栈非空 = 有未闭合元素。
    if let Some(unclosed) = stack.last() {
        return Err(format!(
            "truncated multistatus body: <{unclosed}> is never closed ({} open elements)",
            stack.len()
        ));
    }

    Ok(raw
        .into_iter()
        .map(|(href, ordinal, prop, text, collection_child)| PropRow {
            href,
            propstat_status: statuses.get(ordinal).cloned().flatten(),
            prop,
            text,
            collection_child,
        })
        .collect())
}

/// prop 行 → 每响应 href 一条目（首见序；增强 ②：非 2xx propstat 行
/// 不进投影——内层 404 的 prop 不覆盖 200 块的真值）。
///
/// href 在 `</href>` 收尾时经 [`decode_href`] 解码（增强 ①）——
/// `PropRow::href` 与 [`PropfindEntry::href`] 均为**解码后**形态。
///
/// M5：href → 输出索引的 `HashMap` 一次构建——万级文件目录上逐行
/// `position()` 线性扫是 O(n²)（≈5×10⁸ 次比较，rebuild 面可观测劣
/// 化）；投影序（首见序）与行为不变。
pub(crate) fn entries(rows: &[PropRow]) -> Vec<PropfindEntry> {
    let mut out: Vec<PropfindEntry> = Vec::new();
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for row in rows.iter().filter(|row| row_is_authoritative(row)) {
        let index = match index_of.get(row.href.as_str()) {
            Some(index) => *index,
            None => {
                out.push(PropfindEntry {
                    href: row.href.clone(),
                    is_collection: false,
                    content_length: None,
                    last_modified: None,
                    quota_used_bytes: None,
                    quota_available_bytes: None,
                });
                index_of.insert(row.href.as_str(), out.len() - 1);
                out.len() - 1
            }
        };
        let entry = &mut out[index];
        if row.prop == "resourcetype" && row.collection_child {
            entry.is_collection = true;
        }
        if row.prop == "getcontentlength" {
            entry.content_length = row.text.parse().ok();
        }
        if row.prop == "getlastmodified" && !row.text.is_empty() {
            entry.last_modified = Some(row.text.clone());
        }
        if row.prop == "quota-used-bytes" {
            entry.quota_used_bytes = row.text.parse().ok();
        }
        if row.prop == "quota-available-bytes" {
            entry.quota_available_bytes = row.text.parse().ok();
        }
    }
    out
}

/// 组件名可寻址性（K67 过滤纪律，ck-sftp `name_is_addressable` 同源）：
/// `\`/`\0`/U+FFFD（非 UTF-8 lossy 替换形态——真名并非替换串，对它
/// 的任何寻址都是 NotFound）三类不可见。
pub fn is_addressable_name(name: &str) -> bool {
    !name.contains('\\') && !name.contains('\0') && !name.contains('\u{FFFD}')
}
