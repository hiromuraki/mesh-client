//! Unix 的动作：没有托盘、没有消息框，退出信号从 systemd 或 Ctrl+C 来。

use mesh_engine::Notice;
use std::io::Write;
use std::sync::mpsc::{Receiver, Sender};

/// Unix 上没有需要附加的控制台（进程本来就继承终端的标准输出）。
pub(crate) fn attach_console() {}

pub(crate) fn say(text: &str) {
    emit_out_raw(text.as_bytes());
    emit_out_raw(b"\n");
}

pub(crate) fn say_err(text: &str) {
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.write_all(b"\n");
    let _ = stderr.flush();
}

pub(crate) fn emit_out_raw(bytes: &[u8]) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

/// headless 上错误已经写到 stderr，不需要额外的提示。
pub(crate) fn notify_error(_text: &str) {}

/// 安装目录检查只在 Windows 桌面弹框；Unix 的 stderr 已经写明原因。
pub(crate) fn notify_startup_blocked(_reason: &str) {}

/// 只有 Windows 版有托盘菜单（Go 的 opendir_other.go 同）。
pub(crate) fn open_browser(_url: &str) -> Result<(), String> {
    Err(mesh_text::get(mesh_text::id::CLIENT_TEXT_NO_TRAY).to_string())
}

pub(crate) fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default()
}

pub(crate) fn spawn_signal_thread(tx: Sender<()>) -> Result<(), String> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])
    .map_err(|err| mesh_text::format(mesh_text::id::CLIENT_ERROR_REGISTER_SIGNALS, &[&err]))?;
    std::thread::Builder::new()
        .name("mesh-signal".to_string())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = tx.send(());
            }
        })
        .map_err(|err| {
            mesh_text::format(mesh_text::id::CLIENT_ERROR_START_SIGNAL_THREAD, &[&err])
        })?;
    Ok(())
}

/// Unix 没有消息框；用户动作失败在面板里已经是一句 HTTP 错误。
pub(crate) fn spawn_notice_thread(rx: Receiver<Notice>) {
    drop(rx);
}
