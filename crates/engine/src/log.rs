//! 客户端唯一的行式日志：core 的输出与客户端事件写进同一个文件，界面按时间顺序读尾部。
//!
//! 对应 Go 的 `internal/logbook`。两条与审计相关的约束：
//!
//! - **高低水位裁剪（4MB / 2MB，只保留完整行）**：日志是无限增长的，而面板只读尾部
//!   200 行；裁剪必须按字节且不能切在半行上。Windows 上目标文件还开着就替换不了，
//!   所以顺序是 close → rename → reopen。
//! - **相邻重复行合并**：内核在打洞重试的窗口里会连着刷同一条 WARN，一屏几十行全是它。
//!   合并不丢信息：时间取最后一次，次数记在 [`Line::count`]。

use crate::clock;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 超过这个大小就把文件收缩到 [`LOW_WATERMARK`]。
const HIGH_WATERMARK: u64 = 4 << 20;
/// 收缩之后保留的尾部字节数。
const LOW_WATERMARK: u64 = HIGH_WATERMARK / 2;
/// 每次读尾部只读这么多字节：日志可能已经有几 MB，读整个文件没有意义。
const TAIL_BYTES: u64 = 256 << 10;
/// 面板一次要的行数（Go 的 `logTailLines`）。
const TAIL_LINES: usize = 200;

/// 一行日志。`count` 只在合并相邻重复行之后有意义（0 表示没有合并过）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub time: String,
    pub source: String,
    pub text: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub count: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// 追加式日志文件，可并发写入（core 的两个输出泵 + 引擎线程）。
pub struct Log {
    path: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    file: Option<File>,
    size: u64,
}

impl Log {
    /// 打开（或创建）日志文件，所在目录不存在时一并创建。
    pub fn open(path: &Path) -> io::Result<Log> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let log = Log {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                file: None,
                size: 0,
            }),
        };
        log.reopen(&mut log.lock())?;
        Ok(log)
    }

    /// 日志打不开时用的空实现：写入直接丢弃，引擎照常跑（Go 的 logErr 也是这样）。
    pub(crate) fn unavailable(path: &Path) -> Log {
        Log {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                file: None,
                size: 0,
            }),
        }
    }

    /// 追加一行。`source` 标记来源（`app` / `core`）。
    pub fn add(&self, source: &str, message: &str) {
        // 一条日志必须占一行：core 的多行错误展开成单行，换行替换成空格。
        let message = message.replace('\n', " ");
        // 密钥类字段在这里统一打码：app 与 core 的行、写盘与面板读出来的都是同一份。
        let message = crate::redact::line(&message);
        let line = format!("{} [{}] {}\n", clock::hms(), source, message);
        let mut inner = self.lock();
        if inner.size >= HIGH_WATERMARK {
            self.rotate(&mut inner);
        }
        let Some(file) = inner.file.as_mut() else {
            return;
        };
        // 写失败只丢这一行：日志不可用不该让引擎停摆。
        if file.write_all(line.as_bytes()).is_ok() {
            inner.size += line.len() as u64;
        }
    }

    /// 关闭文件句柄。进程退出前调用；不调用也不会丢已写入的内容。
    pub fn close(&self) {
        let mut inner = self.lock();
        inner.file = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reopen(&self, inner: &mut Inner) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        inner.size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        inner.file = Some(file);
        Ok(())
    }

    /// 把文件裁剪到最近的低水位长度，只保留完整行。
    ///
    /// 先写临时文件再替换：裁剪与写回之间被强制结束时，原日志仍然是完整的。
    fn rotate(&self, inner: &mut Inner) {
        let (window, has_sentinel) = match read_tail(&self.path, LOW_WATERMARK) {
            Ok(found) => found,
            Err(_) => return,
        };
        let (offset, count) = complete_records(&window, has_sentinel);
        if count == 0 {
            return;
        }
        let temp = self.path.with_extension("log.rotating");
        if fs::write(&temp, &window[offset..offset + count]).is_err() {
            let _ = fs::remove_file(&temp);
            return;
        }
        // Windows 上目标文件还开着就替换不了，先让出写句柄。
        inner.file = None;
        if fs::rename(&temp, &self.path).is_err() {
            let _ = fs::remove_file(&temp);
        }
        // 无论替换是否成功都要重新打开：失败时继续往原文件追加，不丢日志。
        let _ = self.reopen(inner);
    }
}

/// 读文件末尾约 `n` 字节；`has_sentinel` 表示窗口前面还有内容。
fn read_tail(path: &Path, n: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(n);
    let has_sentinel = start > 0;
    // 多读一个字节当哨兵：用它判断窗口是不是正好落在行首。
    let read_from = if has_sentinel { start - 1 } else { 0 };
    file.seek(SeekFrom::Start(read_from))?;
    let mut window = Vec::with_capacity((size - read_from) as usize);
    file.read_to_end(&mut window)?;
    Ok((window, has_sentinel))
}

/// 在窗口里找出整行区间；窗口首尾各可能截断一行，都丢掉。
fn complete_records(window: &[u8], has_sentinel: bool) -> (usize, usize) {
    let mut offset = 0usize;
    if has_sentinel {
        if window.is_empty() {
            return (0, 0);
        }
        if window[0] == b'\n' {
            // 哨兵是换行：窗口首字节就是新一行的开头。
            offset = 1;
        } else {
            match window[1..].iter().position(|byte| *byte == b'\n') {
                Some(first) => offset = first + 2,
                None => return (0, 0),
            }
        }
    }
    let last = match window.iter().rposition(|byte| *byte == b'\n') {
        Some(last) => last,
        None => return (0, 0),
    };
    if last < offset {
        return (0, 0);
    }
    (offset, last + 1 - offset)
}

/// 读 `path` 末尾最多 `n` 行；文件不存在时返回空。
pub(crate) fn tail_lines(path: &Path, n: usize) -> Vec<Line> {
    if n == 0 {
        return Vec::new();
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let size = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => return Vec::new(),
    };
    let offset = size.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }
    let mut raw = Vec::new();
    if file.read_to_end(&mut raw).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
    let mut rows: Vec<&str> = text.split('\n').collect();
    // 窗口不是从文件头开始：首行可能被截断，丢掉。
    if offset > 0 && !rows.is_empty() {
        rows.remove(0);
    }
    let mut lines: Vec<Line> = rows
        .into_iter()
        .filter(|row| !row.is_empty())
        .map(parse_line)
        .collect();
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    lines
}

/// 面板日志：最近 `n` 行（`n` 越界时用默认 200）。
pub fn logs(path: &Path, n: usize) -> Vec<Line> {
    let n = if n == 0 || n > TAIL_LINES {
        TAIL_LINES
    } else {
        n
    };
    collapse(tail_lines(path, n))
}

/// “最近事件”：客户端事件与内核告警，最多 12 条。
///
/// 内核每条连接都会打 INFO，直接取尾部会被握手日志淹没，所以在完整的尾部窗口里
/// 筛选，而不是先截断再筛。
pub fn events(path: &Path, n: usize) -> Vec<Line> {
    let n = if n == 0 || n > 50 { 12 } else { n };
    let mut out: Vec<Line> = tail_lines(path, TAIL_LINES)
        .into_iter()
        .filter(|line| {
            (line.source == "app" || line.text.contains("WARN") || line.text.contains("ERROR"))
                && !noise(&line.text)
        })
        .collect();
    if out.len() > n {
        out.drain(..out.len() - n);
    }
    collapse(out)
}

/// 合并相邻的同一句话：时间取最后一次，出现次数记在 `count` 上。
fn collapse(lines: Vec<Line>) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::with_capacity(lines.len());
    for line in lines {
        match out.last_mut() {
            Some(last) if last.source == line.source && last.text == line.text => {
                last.count += 1;
                last.time = line.time;
            }
            _ => {
                out.push(Line { count: 1, ..line });
            }
        }
    }
    out
}

/// 内核自己也会一直重试的那几句话：属于常态噪声，不占「最近事件」的版面。
/// 它们照样进日志文件（全部日志里能看到）。
fn noise(text: &str) -> bool {
    const NOISE: [&str; 5] = [
        "peer rpc transport read aborted",
        "rpc transport read aborted",
        "creating udp hole punching listener",
        "udp hole punching listener started",
        "failed to connect with socket",
    ];
    NOISE.iter().any(|needle| text.contains(needle))
}

/// 解析 `HH:MM:SS [source] text`；对不上就整行当正文（例如被外部追加的内容）。
fn parse_line(row: &str) -> Line {
    let bytes = row.as_bytes();
    let looks_like_time = bytes.len() >= 10
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2] == b':'
        && bytes[3].is_ascii_digit()
        && bytes[4].is_ascii_digit()
        && bytes[5] == b':'
        && bytes[6].is_ascii_digit()
        && bytes[7].is_ascii_digit()
        && bytes[8] == b' '
        && bytes[9] == b'[';
    if !looks_like_time {
        return Line {
            text: row.to_string(),
            ..Line::default()
        };
    }
    let Some(close) = row[9..].find("] ") else {
        return Line {
            text: row.to_string(),
            ..Line::default()
        };
    };
    let close = 9 + close;
    Line {
        time: row[..8].to_string(),
        source: row[10..close].to_string(),
        text: row[close + 2..].to_string(),
        count: 0,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-engine-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_and_tail_round_trip() {
        let dir = temp_dir("log");
        let path = dir.join("client.log");
        let log = Log::open(&path).unwrap();
        log.add("app", "客户端启动");
        log.add("core", "multi\nline message");
        let lines = logs(&path, 10);
        assert_eq!(lines.len(), 2, "lines = {lines:?}");
        assert_eq!(lines[0].source, "app");
        assert_eq!(lines[0].text, "客户端启动");
        // 多行消息折成一行：一条日志必须占一行。
        assert_eq!(lines[1].text, "multi line message");
        assert_eq!(lines[0].time.len(), 8);
        log.close();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adjacent_repeats_are_collapsed() {
        let dir = temp_dir("collapse");
        let path = dir.join("client.log");
        let log = Log::open(&path).unwrap();
        for _ in 0..3 {
            log.add("core", "peer rpc transport read aborted");
        }
        log.add("app", "换个话题");
        log.add("core", "peer rpc transport read aborted");
        let lines = logs(&path, 10);
        assert_eq!(lines.len(), 3, "lines = {lines:?}");
        assert_eq!(lines[0].count, 3);
        assert_eq!(lines[1].count, 1);
        assert_eq!(lines[2].count, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_keeps_complete_lines_only() {
        let dir = temp_dir("rotate");
        let path = dir.join("client.log");
        // 直接写一个超过高水位的文件：每行 64 字节，足够让裁剪动起来。
        let mut bulk = String::new();
        for index in 0..200_000 {
            bulk.push_str(&format!("12:00:00 [app] line {index:06}\n"));
        }
        fs::write(&path, &bulk).unwrap();
        assert!(fs::metadata(&path).unwrap().len() > HIGH_WATERMARK);
        let log = Log::open(&path).unwrap();
        log.add("app", "after rotate");
        let size = fs::metadata(&path).unwrap().len();
        // 裁剪保留的是“最近 2MB 的完整行”，再加上刚写的那一行，所以允许一点点超出。
        assert!(
            size <= LOW_WATERMARK + 4096,
            "裁剪后大小 = {size}，应回到低水位附近"
        );
        let lines = logs(&path, 3);
        assert_eq!(lines.last().unwrap().text, "after rotate");
        // 首行必须是完整的一行（形如 "12:00:00 [app] line ..."）。
        let first = tail_lines(&path, 100_000).remove(0);
        assert!(first.text.starts_with("line "), "first = {first:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_skip_noise_and_keep_app_lines() {
        let dir = temp_dir("events");
        let path = dir.join("client.log");
        let log = Log::open(&path).unwrap();
        log.add("app", "已连接服务端");
        log.add("core", "INFO creating udp hole punching listener");
        log.add("core", "WARN something broke");
        let events = events(&path, 10);
        assert_eq!(events.len(), 2, "events = {events:?}");
        assert_eq!(events[0].text, "已连接服务端");
        assert_eq!(events[1].text, "WARN something broke");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_reads_as_empty() {
        let dir = temp_dir("missing");
        assert!(logs(&dir.join("nope.log"), 10).is_empty());
        assert!(events(&dir.join("nope.log"), 10).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
