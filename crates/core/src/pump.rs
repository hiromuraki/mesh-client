//! 按行转交 core 的 stdout/stderr。
//!
//! 与 Go 的 `coreproc.pump` 一致的两条行为，都是踩过的坑：
//!
//! - 单行上限 4MiB。core 的 panic backtrace 偶尔超过 1MiB；无上限会在内存里
//!   攒一整行，有上限但“读满就停”会让管道写满、core 永久阻塞（表现成“进程还在
//!   但不再有任何输出”）。所以超限时报告一句、把剩下的管道读空、然后停。
//! - 行尾的空白与 `\r` 去掉、整行空白丢弃，但**保留行首缩进**（多行错误里的
//!   `Caused by:` 分支要靠它看层次）。

use std::io::Read;
use std::sync::{Arc, Mutex};

/// 单行上限（Go 是 `4 << 20`）。
pub(crate) const MAX_LINE: usize = 4 << 20;

/// 行回调。两个输出泵线程共用一份，互斥只保护回调本身（写日志），不保护缓冲区。
pub(crate) type LineSink = Arc<Mutex<Box<dyn Fn(&str) + Send>>>;

pub(crate) fn pump<R: Read>(mut reader: R, sink: &LineSink) {
    let mut chunk = [0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut overflowed = false;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                // 管道出错时也要把剩下的读空，否则 core 写满缓冲就卡住。
                emit(
                    sink,
                    &mesh_text::format(mesh_text::id::CORE_ERROR_OUTPUT_INTERRUPTED, &[&err]),
                );
                break;
            }
        };
        for &byte in &chunk[..read] {
            if byte == b'\n' {
                emit_line(sink, &pending);
                pending.clear();
                continue;
            }
            if pending.len() < MAX_LINE {
                pending.push(byte);
            } else {
                overflowed = true;
            }
        }
        if overflowed {
            break;
        }
    }
    if overflowed {
        // 与 Go 一样：报一句之后不再逐行转交，但必须把管道读空。
        emit(
            sink,
            &mesh_text::format(mesh_text::id::CORE_ERROR_LINE_TOO_LONG, &[&MAX_LINE]),
        );
        let mut sink_bytes = [0u8; 64 * 1024];
        while matches!(reader.read(&mut sink_bytes), Ok(n) if n > 0) {}
        return;
    }
    // 最后一行没有换行结尾时也要交出去（core 被强杀时常见）。
    if !pending.is_empty() {
        emit_line(sink, &pending);
    }
}

fn emit_line(sink: &LineSink, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let line = text.trim_end_matches([' ', '\t', '\r']);
    if line.trim().is_empty() {
        return;
    }
    emit(sink, line);
}

fn emit(sink: &LineSink, line: &str) {
    let callback = match sink.lock() {
        Ok(callback) => callback,
        // 回调里 panic 过：这行日志丢掉，但输出泵不能跟着死——它负责读空管道。
        Err(poisoned) => poisoned.into_inner(),
    };
    callback(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sink(lines: Arc<Mutex<Vec<String>>>) -> LineSink {
        Arc::new(Mutex::new(Box::new(move |line: &str| {
            lines.lock().unwrap().push(line.to_string());
        })))
    }

    #[test]
    fn splits_lines_and_trims_only_the_trailing_whitespace() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let input = b"  indented  \r\n\n   \nlast without newline";
        pump(Cursor::new(input.to_vec()), &sink(Arc::clone(&lines)));
        assert_eq!(
            *lines.lock().unwrap(),
            vec!["  indented".to_string(), "last without newline".to_string()]
        );
    }

    #[test]
    fn an_oversized_line_is_reported_and_the_pipe_is_drained() {
        // Go 单测同款：5MiB 单行超过上限，必须报一次“输出中断”并把剩余读空。
        let lines = Arc::new(Mutex::new(Vec::new()));
        let mut input = vec![b'a'; 5 << 20];
        input.extend_from_slice(b"\nnext line\n");
        let cursor = Cursor::new(input);
        pump(cursor, &sink(Arc::clone(&lines)));
        let got = lines.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "只应报告一次中断: {got:?}");
        assert!(got[0].contains("core 输出中断"), "got = {got:?}");
    }

    #[test]
    fn an_oversized_line_still_drains_to_the_end() {
        struct Counting {
            bytes: Vec<u8>,
            read: usize,
        }
        impl Read for Counting {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let left = &self.bytes[self.read..];
                let n = left.len().min(out.len());
                out[..n].copy_from_slice(&left[..n]);
                self.read += n;
                Ok(n)
            }
        }
        let mut bytes = vec![b'x'; MAX_LINE + 1];
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(&vec![b'y'; 1 << 20]);
        let total = bytes.len();
        let reader = Counting { bytes, read: 0 };
        // 把 reader 包起来只为在 pump 之后读它的 read 位置。
        let mut reader = reader;
        pump(&mut reader, &sink(Arc::new(Mutex::new(Vec::new()))));
        assert_eq!(
            reader.read,
            total,
            "剩余 {} 字节没被读空",
            total - reader.read
        );
    }
}
