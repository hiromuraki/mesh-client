//! config/ui.json：面板地址的落盘位置，供第二次启动时唤出面板。
//!
//! Go 在 main.go 里写与读它；Rust 侧把格式收进面板 crate，client 只调用
//! write_url / read_url。格式跟 Go 的 json.MarshalIndent 一致：两空格缩进、0600。

use serde_json::Value;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// 写入面板地址（覆盖旧值）。
pub fn write_url(path: &Path, url: &str) -> io::Result<()> {
    let raw = serde_json::to_string_pretty(&serde_json::json!({ "url": url }))
        .map_err(io::Error::other)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(raw.as_bytes())
}

/// 读回面板地址；文件不存在、格式不对或地址为空都当没有记录。
pub fn read_url(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let url = value.get("url")?.as_str()?.to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}
