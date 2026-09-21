//! serde 载荷模型（Phase 6 / 123-1）——字段双拼与时间双态。
//!
//! 123pan 的服务端在代际间（2026-08-29 端点重组前后）与端点族间混用
//! PascalCase / camelCase / snake_case（123-0 真机实证：list 条目
//! `InfoList` 大写而 info 端点 `infoList` 小写；`/v2` 完成响应回
//! snake_case `file_info`）——模型层一律 `rename` 主形态 + `alias`
//! 次形态（计划 §5.10 硬纪律 10）。
//!
//! 时间字段双态（§5.16）：`CreateAt/UpdateAt` = int Unix 秒或 ISO8601
//! 字符串（真机 list 实证 `2026-09-20T12:20:15+08:00`，mkdir 的 Info
//! 带纳秒精度）——[`deserialize_unix_secs`] 自定义反序列化统一归
//! Unix 秒。

use serde::Deserialize;

/// `/b/api/user/info` 的 data（123-0 真机采样：uid / 空间与流量字段
/// 都在此端点——计划 §3 quota 行的真身；`report/info` 只回会员档位）。
///
/// 字段双拼：`UID/uid`（123panNextGen `CloudUserInfoModel.from_dict`
/// 的双形态）；其余 tolerate 缺省（服务端字段增删不炸解析）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserInfo {
    /// 账号 uid——VolumeId `pan123:<uid>` 的 key。
    #[serde(rename = "UID", alias = "uid", default)]
    pub uid: i64,
    /// 永久空间总量（字节；真机采样 2199023255552 = 2TiB）。
    #[serde(rename = "SpacePermanent", alias = "spacePermanent", default)]
    pub space_permanent: i64,
    /// 已用空间（字节，实时）。
    #[serde(rename = "SpaceUsed", alias = "spaceUsed", default)]
    pub space_used: i64,
    /// 直链流量（字节；免费账号 0）。
    #[serde(rename = "DirectTraffic", alias = "directTraffic", default)]
    pub direct_traffic: i64,
    /// 分享流量（字节）。
    #[serde(rename = "ShareTraffic", alias = "shareTraffic", default)]
    pub share_traffic: i64,
    /// 会员标志（真机免费账号 false；`unlimited` 键未见——挂账）。
    #[serde(rename = "Vip", alias = "vip", default)]
    pub vip: bool,
}

/// `file/download/traffic/check` 的 data（123-0 真机采样：免费日额
/// ≈10GiB——`originalRemainTraffic` 实测 10735321088B）。
///
/// 阻断判据 = [`TrafficStatus::is_traffic_exceeded`]；`is_blocked` 含义
/// 未明（流量未超下正常下载——真机观察，跟踪单挂账）**不作为阻断依据**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrafficStatus {
    #[serde(rename = "isTrafficExceeded", alias = "is_traffic_exceeded", default)]
    pub is_traffic_exceeded: bool,
    /// 每日下载流量余量（字节；doctor/123-4 透出面）。
    #[serde(
        rename = "originalRemainTraffic",
        alias = "original_remain_traffic",
        default
    )]
    pub original_remain_traffic: i64,
    /// 含义未明（真机 `true` 且下载正常）——只透出，不阻断。
    #[serde(rename = "isBlocked", alias = "is_blocked", default)]
    pub is_blocked: bool,
}

/// list / info 条目行（123-2 的 list/stat 面消费；123-1 先钉解析契约：
/// 双拼字段 + 时间双态——桩测试覆盖）。
///
/// `Type`：0 = 文件，1 = 目录（真机 mkdir/list 实证；pan123-rs
/// `is_dir() = type != 0` 同源）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileEntry {
    #[serde(rename = "FileId", alias = "fileId", default)]
    pub file_id: i64,
    #[serde(rename = "ParentFileId", alias = "parentFileId", default)]
    pub parent_file_id: i64,
    #[serde(rename = "FileName", alias = "fileName", default)]
    pub file_name: String,
    #[serde(rename = "Type", alias = "type", default)]
    pub entry_type: i64,
    #[serde(rename = "Size", alias = "size", default)]
    pub size: i64,
    #[serde(rename = "Etag", alias = "etag", default)]
    pub etag: String,
    /// download_info 载荷需要的 S3 键标志（mkdir/list 的 Info 行真机
    /// 携带 `"S3KeyFlag": "<uid>-0"` 形态）。
    #[serde(rename = "S3KeyFlag", alias = "s3keyFlag", default)]
    pub s3_key_flag: String,
    /// 回收站标志（info 端点可见；list 恒查非回收站面）。delete 回读
    /// 校验的消费面（§5.11 数据完整性纪律）。
    #[serde(rename = "Trashed", alias = "trashed", default)]
    pub trashed: bool,
    #[serde(
        rename = "UpdateAt",
        alias = "updateAt",
        default,
        deserialize_with = "deserialize_unix_secs"
    )]
    pub update_at: i64,
    #[serde(
        rename = "CreateAt",
        alias = "createAt",
        default,
        deserialize_with = "deserialize_unix_secs"
    )]
    pub create_at: i64,
}

impl FileEntry {
    /// 1 = 目录（0 = 文件；真机实证）。
    pub fn is_dir(&self) -> bool {
        self.entry_type == 1
    }
}

// ---------------------------------------------------------------------------
// 时间双态（§5.16）
// ---------------------------------------------------------------------------

/// 时间字段反序列化：int Unix 秒（含浮点形态，截断）或 ISO8601 字符串
/// （`2026-09-20T12:20:15+08:00`；纳秒精度截断；`Z` 与 `±HH:MM` 偏移
/// 都认）——统一归 Unix 秒；**显式 null 容错为 0**（M3/K78——服务端
/// 单条目回 `"UpdateAt": null` 不得毒死整页 list；键缺失由字段级
/// `#[serde(default)]` 兜 0）。
pub fn deserialize_unix_secs<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an integer Unix timestamp or an ISO8601 string")
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<i64, E> {
            Ok(v.min(i64::MAX as u64) as i64)
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<i64, E> {
            Ok(v as i64)
        }
        /// 显式 null（JSON null / Option None 形态）→ 0（M3/K78）。
        fn visit_unit<E: serde::de::Error>(self) -> Result<i64, E> {
            Ok(0)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<i64, E> {
            Ok(0)
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<i64, E> {
            parse_iso8601_unix(v)
                .ok_or_else(|| E::custom(format!("not an ISO8601 timestamp: {v:?}")))
        }
    }
    deserializer.deserialize_any(V)
}

/// ISO8601 → Unix 秒（`YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM]`）。
///
/// 真机形态恒带 `+08:00` 偏移；无偏移的裸本地时间按歧义拒绝
/// （`None`）。纯函数、无外部时间库（chrono 不引——计划 §2 依赖面）。
pub(crate) fn parse_iso8601_unix(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None; // 最短形态 19 字符 + 至少一个时区字符
    }
    let num = |range: &str| -> Option<i64> { range.parse::<i64>().ok() };
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None; // 日期/时间分隔（宽松：空格同构）
    }
    let year = num(s.get(0..4)?)?;
    let month = num(s.get(5..7)?)?;
    let day = num(s.get(8..10)?)?;
    let hour = num(s.get(11..13)?)?;
    let minute = num(s.get(14..16)?)?;
    let second = num(s.get(17..19)?)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let mut rest = s.get(19..)?;
    // 小数秒（mkdir Info 的纳秒精度形态）：验证为数字后丢弃（截断）
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits_end = stripped
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(stripped.len());
        if digits_end == 0 {
            return None; // "." 后无数字
        }
        rest = &stripped[digits_end..];
    }
    // 时区偏移：真机恒 +08:00；Z/小写 z 与 ±HH:MM 都认；裸本地拒绝
    let offset_secs = match rest {
        "Z" | "z" => 0,
        _ => {
            let rb = rest.as_bytes();
            if rb.len() != 6 || rb[3] != b':' {
                return None;
            }
            let sign = match rb[0] {
                b'+' => 1i64,
                b'-' => -1i64,
                _ => return None,
            };
            let oh = num(rest.get(1..3)?)?;
            let om = num(rest.get(4..6)?)?;
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(year, month as u32, day as u32);
    Some(days * 86400 + hour * 3600 + minute * 60 + second - offset_secs)
}

/// Howard Hinnant `days_from_civil`（1970-03-01 纪元的 proleptic
/// Gregorian 天数——无外部库的时间地基）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + (d as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_info_parses_the_sampled_field_shapes() {
        let info: UserInfo = serde_json::from_str(
            r#"{"UID":123456,"SpacePermanent":2199023255552,"SpaceUsed":1048576,
                "DirectTraffic":0,"ShareTraffic":0,"Vip":false}"#,
        )
        .expect("sampled shape parses");
        assert_eq!(info.uid, 123456);
        assert_eq!(info.space_permanent, 2199023255552);
        assert_eq!(info.space_used, 1048576);
        assert!(!info.vip);

        let dual: UserInfo = serde_json::from_str(r#"{"uid":42}"#).expect("dual-cased uid");
        assert_eq!(dual.uid, 42);
    }

    #[test]
    fn file_entry_parses_dual_spelled_fields_and_dual_form_times() {
        let pascal: FileEntry = serde_json::from_str(
            r#"{"FileId":64379791,"ParentFileId":0,"FileName":"a.txt","Type":0,
                "Size":2097152,"Etag":"0123abcd",
                "UpdateAt":"2026-09-20T12:20:15+08:00","CreateAt":1789878015}"#,
        )
        .expect("PascalCase + string/int time mix parses");
        assert_eq!(pascal.file_id, 64379791);
        assert_eq!(pascal.file_name, "a.txt");
        assert_eq!(pascal.entry_type, 0);
        assert_eq!(pascal.size, 2097152);
        assert_eq!(
            pascal.update_at, 1789878015,
            "ISO8601 +08:00 == the int twin"
        );
        assert_eq!(pascal.create_at, 1789878015);

        let camel: FileEntry = serde_json::from_str(
            r#"{"fileId":2,"fileName":"d","type":1,"size":0,
                "updateAt":1789878015.5,"createAt":"2026-09-20T12:20:15.123456789+08:00"}"#,
        )
        .expect("camelCase + fractional forms parse");
        assert_eq!(camel.file_id, 2);
        assert_eq!(camel.entry_type, 1, "1 = directory");
        assert_eq!(camel.update_at, 1789878015, "fractional secs truncate");
        assert_eq!(
            camel.create_at, 1789878015,
            "nanosecond precision truncates"
        );
    }

    #[test]
    fn iso8601_parser_covers_the_observed_and_defensive_shapes() {
        // 真机实证形态
        assert_eq!(
            parse_iso8601_unix("2026-09-20T12:20:15+08:00"),
            Some(1789878015)
        );
        // 纳秒精度（mkdir Info 形态）截断
        assert_eq!(
            parse_iso8601_unix("2026-09-20T12:20:15.123456789+08:00"),
            Some(1789878015)
        );
        // 纪元锚点 + Z/负偏移
        assert_eq!(parse_iso8601_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_unix("1970-01-01T01:00:00+01:00"),
            Some(0),
            "same instant expressed west of UTC"
        );
        // 拒绝形态：裸本地时间（歧义）、垃圾
        assert_eq!(parse_iso8601_unix("2026-09-20T12:20:15"), None);
        assert_eq!(parse_iso8601_unix("not a date"), None);
        assert_eq!(parse_iso8601_unix(""), None);
    }

    /// M3（K78）：时间字段**显式 null** 容错为 0——服务端任何一条目回
    /// `"UpdateAt": null` 不得毒死整页 list（`#[serde(default)]` 只管
    /// 键缺失，显式 null 走 Visitor 的 unit 形态）。
    #[test]
    fn explicit_null_time_fields_tolerate_as_zero_without_poisoning_the_page() {
        let row: FileEntry =
            serde_json::from_str(r#"{"FileId":9,"FileName":"n.txt","Type":0,"UpdateAt":null}"#)
                .expect("explicit null tolerates as 0");
        assert_eq!(row.file_id, 9);
        assert_eq!(row.update_at, 0, "explicit null -> 0");
        assert_eq!(row.create_at, 0, "missing key keeps the field default");

        // 整页形态：一条 null 时间条目不炸整个 Vec（list_page 单道解析）
        let page: Vec<FileEntry> = serde_json::from_str(
            r#"[
                {"FileId":1,"FileName":"a","Type":0,"UpdateAt":1789878015,"CreateAt":1789878015},
                {"FileId":2,"FileName":"b","Type":1,"UpdateAt":null,"CreateAt":null}
            ]"#,
        )
        .expect("one null-time row must not poison the whole page");
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].update_at, 1789878015);
        assert_eq!(page[1].update_at, 0);
        assert_eq!(page[1].create_at, 0);
    }
}
