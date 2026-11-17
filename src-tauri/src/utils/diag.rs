//! 粘贴链路诊断日志(`<程序目录>/config/paste.log`)。
//!
//! 背景:整个粘贴链路的诊断输出目前全是 `eprintln!`,而 release 版是
//! GUI 子系统程序(没有控制台),那些输出**全部丢失** —— 现场反馈
//! "第一次点击不粘贴"时,除了"再点一次"没有任何可取证的信息。
//! 这里把关键节点落成文件,用户复现一次就能把日志发回来定位根因。
//!
//! 写法刻意与 [`crate::services::settings_service`] 里的 `log_autostart`
//! 保持一致(追加、超限截断、UTC 时间戳、IO 失败一律吞掉),不引新依赖:
//! 日志自身出问题绝不能影响粘贴主流程。

use std::io::Write;

/// 诊断日志文件名(位于 `config/` 下)
const PASTE_LOG_FILE: &str = "paste.log";

/// 超过该体积直接截断重写:诊断日志只需要最近一段,避免长期运行无限增长
const PASTE_LOG_MAX_BYTES: u64 = 256 * 1024;

/// 追加一行粘贴诊断日志到 `<程序目录>/config/paste.log`。
///
/// 一次调用 = 一行,行首带 UTC 时间戳。所有 IO 错误吞掉(不能因为磁盘
/// 只读/被占用就让粘贴失败)。
pub fn log_paste(msg: &str) {
    let path = crate::app_config_dir().join(PASTE_LOG_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > PASTE_LOG_MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let _ = std::fs::remove_file(&path);
    }
    let line = format_line(unix_now(), msg);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// 一行日志的完整格式(纯函数,便于单测):
/// `[2026-11-17 12:00:00 UTC] activate id=123 do_paste=true handling=HideFirst`
fn format_line(secs: u64, msg: &str) -> String {
    format!("[{} UTC] {}\n", format_utc(secs), msg)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix 秒 → "YYYY-MM-DD HH:MM:SS"(UTC)。
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, h, mi, s)
}

/// 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days
/// (与 settings_service 里的自启动日志同款算法,不引 chrono)
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 时间戳换算与已知纪元对齐(与 settings_service 的同类断言一致)
    #[test]
    fn format_utc_matches_known_epochs() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09 01:46:40");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13:20");
    }

    /// 一行日志的格式:方括号 UTC 时间戳 + 空格 + 消息 + 换行
    #[test]
    fn format_line_is_timestamped_single_line() {
        let line = format_line(
            1_700_000_000,
            "activate id=123 do_paste=true handling=HideFirst",
        );
        assert_eq!(
            line,
            "[2023-11-14 22:13:20 UTC] activate id=123 do_paste=true handling=HideFirst\n"
        );
        // 恰好一个换行(追加写入时不会产生空行)
        assert_eq!(line.matches('\n').count(), 1);
    }
}
