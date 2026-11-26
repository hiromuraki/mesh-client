use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// init 把日志落到 <安装根>/log/launcher.log。安装失败发生在客户端已经退出之后，
/// 没有这份日志就只剩一个错误框。
pub fn init(root: &Path) {
    let dir = root.join("log");
    let _ = fs::create_dir_all(&dir);
    if let Ok(mut guard) = PATH.lock() {
        *guard = Some(dir.join("launcher.log"));
    }
}

pub fn line(msg: &str) {
    let text = format!("{} {}\n", timestamp(), msg);
    eprint!("{text}");
    if let Ok(guard) = PATH.lock() {
        if let Some(path) = guard.as_ref() {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(text.as_bytes());
            }
        }
    }
}

/// timestamp 是 UTC 的 ISO-8601 秒级时间戳。本地时区要额外依赖，而日志只用来排先后。
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// civil_from_days 把“1970-01-01 起的天数”换成公历日期（Howard Hinnant 的算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}
