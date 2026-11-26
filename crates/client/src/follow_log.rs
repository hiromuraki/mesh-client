//! headless 的控制台日志跟随：把日志文件的新增内容抄到 stdout。
//!
//! 端口与格式跟 Go 的 followLog/copyNewLines 一致：300ms 一次、只抄完整行、日志
//! 被裁剪回低水位时偏移作废。

use crate::platform;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 把日志抄到控制台的轮询间隔（Go 的 logPollInterval）。
pub(crate) const POLL: Duration = Duration::from_millis(300);

/// 文件大小；不存在就是 0。
pub(crate) fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// 从 offset 处取出完整行写到 out，返回新的偏移。
pub(crate) fn copy_new_lines(path: &Path, offset: u64, out: &mut dyn Write) -> u64 {
    let Ok(mut file) = File::open(path) else {
        return offset;
    };
    let Ok(metadata) = file.metadata() else {
        return offset;
    };
    let size = metadata.len();
    // 日志被裁剪回低水位时偏移作废：跳过已经裁掉的部分。
    let offset = if size < offset { size } else { offset };
    if size == offset {
        return offset;
    }
    let mut buffer = vec![0u8; (size - offset) as usize];
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let read = match file.read(&mut buffer) {
        Ok(read) => read,
        Err(_) => return offset,
    };
    let Some(end) = buffer[..read].iter().rposition(|byte| *byte == b'\n') else {
        return offset;
    };
    if out.write_all(&buffer[..=end]).is_err() {
        return offset + read as u64;
    }
    offset + end as u64 + 1
}

/// 跟随日志直到 stop 置位。
pub(crate) fn follow(path: &Path, offset: u64, stop: &AtomicBool) {
    let mut offset = offset;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        offset = copy_new_lines(path, offset, &mut ConsoleSink);
    }
}

/// 把 stdout 当成 Write 用；Windows 上由 platform 决定写到父控制台。
struct ConsoleSink;

impl Write for ConsoleSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        platform::emit_out_raw(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_log(name: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mesh-follow-log-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("client.log");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn copies_only_complete_lines() {
        let path = temp_log(
            "complete",
            "10:00:00 [app] one\n10:00:01 [app] two\n10:00:0",
        );
        let mut out = Vec::new();
        let next = copy_new_lines(&path, 0, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "10:00:00 [app] one\n10:00:01 [app] two\n"
        );
        // 半截行留在文件里，下次再看。
        assert_eq!(next, file_size(&path) - "10:00:0".len() as u64);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn continues_from_the_previous_offset() {
        let path = temp_log("continue", "aaa\n");
        let mut out = Vec::new();
        let offset = copy_new_lines(&path, 0, &mut out);
        assert_eq!(offset, 4);
        fs::write(&path, "aaa\nbbb\n").unwrap();
        let mut more = Vec::new();
        let offset = copy_new_lines(&path, offset, &mut more);
        assert_eq!(String::from_utf8(more).unwrap(), "bbb\n");
        assert_eq!(offset, 8);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn nothing_to_do_when_the_file_is_empty_or_missing() {
        let path = temp_log("empty", "");
        let mut out = Vec::new();
        assert_eq!(copy_new_lines(&path, 0, &mut out), 0);
        assert!(out.is_empty());
        assert_eq!(
            copy_new_lines(&path.with_extension("missing"), 0, &mut out),
            0
        );
        assert_eq!(file_size(&path.with_extension("missing")), 0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_truncated_log_restarts_from_the_new_size() {
        let path = temp_log("truncated", "one line\ntwo line\n");
        let old = file_size(&path);
        // 模拟日志被裁剪到低水位：文件变小，旧偏移不再有效。
        fs::write(&path, "tail\n").unwrap();
        let mut out = Vec::new();
        let next = copy_new_lines(&path, old, &mut out);
        assert_eq!(next, 5);
        assert!(out.is_empty(), "偏移落在新文件末尾时什么都不抄");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
