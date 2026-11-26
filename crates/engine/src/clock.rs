//! 时间：日志里的本地时间列与 Unix 秒。
//!
//! 日志格式是 `15:04:05 [app] ...`，与 Go 的 `time.Now().Format("15:04:05")` 一致，
//! 是**本地时间**。Rust 标准库只有 UTC，引 `chrono` 又太重（它会拖进时区数据库），
//! 所以这里各自调一次系统 API：Windows 的 `GetLocalTime` 直接给本地时间，Unix 用
//! `localtime_r`（它自己处理夏令时）。

use std::time::{SystemTime, UNIX_EPOCH};

/// 本地时间 `HH:MM:SS`。
pub(crate) fn hms() -> String {
    imp::hms()
}

/// Unix 秒。
pub(crate) fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs() as i64,
        // 系统时间被拨到 1970 之前：日志时间列会难看，但别让整个客户端崩。
        Err(_) => 0,
    }
}

/// 本机与服务端时间的差（秒）。正数表示本机快。
pub(crate) fn skew_seconds(server_time: SystemTime) -> i64 {
    match SystemTime::now().duration_since(server_time) {
        Ok(ahead) => ahead.as_secs() as i64,
        Err(behind) => -(behind.duration().as_secs() as i64),
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    pub(crate) fn hms() -> String {
        let mut now: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut now) };
        format!("{:02}:{:02}:{:02}", now.wHour, now.wMinute, now.wSecond)
    }
}

#[cfg(unix)]
mod imp {
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(crate) fn hms() -> String {
        // 别写 `libc::time_t`：musl 1.2 起它是 64 位，libc crate 就给这个别名挂了
        // deprecated。写死 `i64` 又会在 32 位目标上错，所以让类型从 `localtime_r` 的签名推。
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as _)
            .unwrap_or(0);
        let mut local: libc::tm = unsafe { std::mem::zeroed() };
        // localtime_r 不是线程安全的反向：它是可重入版本，缓冲区由调用方提供。
        if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
            return "00:00:00".to_string();
        }
        format!(
            "{:02}:{:02}:{:02}",
            local.tm_hour, local.tm_min, local.tm_sec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_has_the_expected_shape() {
        let now = hms();
        assert_eq!(now.len(), 8, "now = {now}");
        let parts: Vec<&str> = now.split(':').collect();
        assert_eq!(parts.len(), 3);
        for part in parts {
            assert!(part.parse::<u32>().is_ok(), "now = {now}");
        }
    }

    #[test]
    fn skew_is_positive_when_the_local_clock_is_ahead() {
        let server = SystemTime::now() - std::time::Duration::from_secs(600);
        let skew = skew_seconds(server);
        assert!((599..=601).contains(&skew), "skew = {skew}");
        let server = SystemTime::now() + std::time::Duration::from_secs(600);
        let skew = skew_seconds(server);
        assert!(skew <= -599, "skew = {skew}");
    }
}
