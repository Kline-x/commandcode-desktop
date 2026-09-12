//! 时间工具：epoch 毫秒 ↔ 公历日期的换算与格式化。
//!
//! 抽成独立模块的原因：这个换算被三处需要（错误信息里的可读时间、上游
//! config.date 的 YYYY-MM-DD、配额窗口的重置时刻展示），而它**很容易写错**
//! ——civil_from_days 的年份修正（m <= 2 时要 +1）漏掉就会整体偏移一年。
//! 重复实现意味着同一个 bug 要在三处各修一次。
//!
//! 不引入 chrono：本项目目标是单二进制，而这个算法的实现成本很低
//! （Howard Hinnant 的 civil_from_days，已被广泛验证）。

/// 一天的毫秒数。
pub const MS_PER_DAY: i64 = 86_400_000;

/// 把「自 1970-01-01 起的天数」转成 (年, 月, 日)。
///
/// 算法来自 Howard Hinnant 的 chrono-compatible low-level date algorithms。
/// 注意末尾的年份修正：**1 月与 2 月要归入上一年的第 13、14 月**参与计算，
/// 因此需要把年份加回来（漏掉这一步会让 1969/1970 的结果差一年）。
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把 epoch 毫秒格式化成可读的 UTC 时间（用于错误信息与日志）。
pub fn format_epoch_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// 把 epoch 毫秒格式化成 YYYY-MM-DD（上游 config.date 需要）。
pub fn date_string(ms: i64) -> String {
    let (y, mo, d) = civil_from_days(ms.div_euclid(MS_PER_DAY));
    format!("{y:04}-{mo:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 闰日：2024-02-29 是 2024-01-01 之后的第 59 天
        assert_eq!(civil_from_days(19_723 + 59), (2024, 2, 29));
        // 年末：跨到 12 月
        assert_eq!(civil_from_days(19_723 + 365), (2024, 12, 31));
    }

    #[test]
    fn date_string_formats_known_epochs() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(1_704_067_200_000), "2024-01-01");
        // 靠近零点的时刻不应因时区/取整而偏移到前一天
        assert_eq!(date_string(1_704_067_199_999), "2023-12-31");
    }

    #[test]
    fn format_epoch_ms_is_utc_and_zero_padded() {
        assert_eq!(format_epoch_ms(0), "1970-01-01 00:00:00 UTC");
        // 2024-01-01T05:06:07Z
        assert_eq!(
            format_epoch_ms(1_704_085_567_000),
            "2024-01-01 05:06:07 UTC"
        );
    }
}
