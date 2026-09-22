//! HTTP 时间三格式解析（Phase 7 / WD1a；计划 §4.5-5——OpenList
//! parseModified 单格式静默回 epoch 的正面修法）。
//!
//! RFC 9110 §5.6.7 允许服务器发三种日期形态，真机（附录 C ⑧）以
//! IMF-fixdate 为主但实现离散：
//!
//! 1. **IMF-fixdate** `Mon, 21 Sep 2026 10:51:05 GMT`——httpdate crate
//!    （严格形态，ck-pan115 同款依赖）；
//! 2. **RFC 850** `Monday, 21-Sep-26 10:51:05 GMT`——手写小解析器
//!    （两位年按 RFC 9110 §5.6.7：0-49→2000s，50-99→1900s）；
//! 3. **asctime** `Mon Sep 21 10:51:05 2026`——手写小解析器（日数字
//!    允许空格垫宽：`Sun Nov  6 ...`）。
//!
//! 失败返回 `None`——调用方（WD2 读路径）debug 日志记录原文，**绝不
//! 静默当 epoch 0**（OpenList 负面前科：mtime 谎报 0 会污染同步判定）。

use std::time::UNIX_EPOCH;

/// 解析三格式之一的 HTTP 时间为 epoch 秒；失败 `None`。
///
/// 周名不参与判定（格式固定字段，日期本身自证）。IMF-fixdate 的
/// 早于 epoch 值不可表示（httpdate 经 `SystemTime`）——按 `None`
/// 处理；RFC 850/asctime 的手写路径是 i64 算术，负 epoch 自然成立。
pub fn parse_http_time(input: &str) -> Option<i64> {
    let input = input.trim();
    if let Ok(time) = httpdate::parse_http_date(input) {
        return time
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs() as i64);
    }
    parse_rfc850(input).or_else(|| parse_asctime(input))
}

/// RFC 850 形态：`Weekday, DD-Mon-YY HH:MM:SS GMT`。
fn parse_rfc850(input: &str) -> Option<i64> {
    let (weekday, rest) = input.split_once(',')?;
    if weekday.is_empty() || !rest.starts_with(' ') {
        return None;
    }
    let mut fields = rest.split_whitespace();
    let date = fields.next()?;
    let clock = fields.next()?;
    let zone = fields.next()?;
    if fields.next().is_some() || !zone.eq_ignore_ascii_case("gmt") {
        return None;
    }
    let mut date_parts = date.split('-');
    let day: u32 = date_parts.next()?.parse().ok()?;
    let month = month_from_name(date_parts.next()?)?;
    let year = {
        let yy: u32 = date_parts.next()?.parse().ok()?;
        if yy > 99 {
            return None; // RFC 850 是两位年
        }
        expand_two_digit_year(yy)
    };
    let (hour, minute, second) = parse_clock(clock)?;
    epoch_of(year, month, day, hour, minute, second)
}

/// asctime 形态：`Www Mmm DD HH:MM:SS YYYY`（日允许空格垫宽——
/// whitespace 切分天然吸收）。
fn parse_asctime(input: &str) -> Option<i64> {
    let mut fields = input.split_whitespace();
    let weekday = fields.next()?;
    if !weekday.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let month = month_from_name(fields.next()?)?;
    let day: u32 = fields.next()?.parse().ok()?;
    let (hour, minute, second) = parse_clock(fields.next()?)?;
    let year: i64 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    epoch_of(year, month, day, hour, minute, second)
}

/// `HH:MM:SS`（各段域校验：0-23 / 0-59 / 0-59）。
fn parse_clock(clock: &str) -> Option<(u32, u32, u32)> {
    let mut parts = clock.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second))
}

/// 月份缩写 → 月号（大小写宽收）。
fn month_from_name(name: &str) -> Option<u32> {
    let lowered = name.to_ascii_lowercase();
    Some(match lowered.as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// 两位年展开（RFC 9110 §5.6.7）：0-49 → 2000-2049，50-99 →
/// 1950-1999。
fn expand_two_digit_year(yy: u32) -> i64 {
    if yy < 50 {
        2000 + yy as i64
    } else {
        1900 + yy as i64
    }
}

/// (y, m, d) 当月天数（含闰年——非法组合如 2 月 30 日的拒收依据）。
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// 公历 (y, m, d) → 自 epoch 的天数（Howard Hinnant days_from_civil
/// 算法——pan123 models.rs 同款，不为此开 chrono）。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (i64::from(month) + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 域校验 + epoch 装配（两个手写解析器的公共收口）。
fn epoch_of(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * 86_400
            + i64::from(hour) * 3_600
            + i64::from(minute) * 60
            + i64::from(second),
    )
}
