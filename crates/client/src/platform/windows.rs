//! Windows 的桌面动作：附加控制台、消息框、Shell 打开、控制台事件。
//!
//! 客户端是 windowsgui 子系统：双击启动没有控制台，stderr 写进去没人看得见，
//! 所以错误要弹消息框；从控制台启动时 AttachConsole 之后写 CONOUT$，日志才回得去。

use crate::APP_TITLE;
use mesh_engine::Notice;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use windows_sys::Win32::Foundation::BOOL;
use windows_sys::Win32::System::Console::{
    AttachConsole, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_SETFOREGROUND, MB_SYSTEMMODAL,
};

/// 父控制台的写句柄。attach_console 成功之后所有输出都走它。
static CONSOLE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
/// 控制台事件（Ctrl+C、关闭窗口）要唤醒的主循环通道。
static CTRL_TX: OnceLock<Mutex<Option<Sender<()>>>> = OnceLock::new();

/// 让 windowsgui 子系统的进程在从控制台启动时能写回父控制台。
/// 双击启动时没有父控制台，AttachConsole 失败，保持静默即可（与 Go 一致）。
pub(crate) fn attach_console() {
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }
    let Ok(file) = std::fs::OpenOptions::new().write(true).open("CONOUT$") else {
        return;
    };
    let slot = CONSOLE.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(file);
    }
}

fn console_write(bytes: &[u8]) -> bool {
    let Some(slot) = CONSOLE.get() else {
        return false;
    };
    let Ok(mut slot) = slot.lock() else {
        return false;
    };
    let Some(file) = slot.as_mut() else {
        return false;
    };
    let _ = file.write_all(bytes);
    let _ = file.flush();
    true
}

/// 写一行到控制台（没附加控制台时退回到标准输出）。
pub(crate) fn say(text: &str) {
    emit_out_raw(text.as_bytes());
    emit_out_raw(b"\n");
}

/// 写一行错误；同样优先走附加的控制台，让 --headless 在 cmd 里看得见。
pub(crate) fn say_err(text: &str) {
    if console_write(text.as_bytes()) && console_write(b"\n") {
        return;
    }
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.write_all(b"\n");
    let _ = stderr.flush();
}

/// 原样写字节（headless 的日志跟随用）。
pub(crate) fn emit_out_raw(bytes: &[u8]) {
    if console_write(bytes) {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

/// 桌面形态没有控制台：失败必须弹框，否则用户看到的是「双击毫无反应」。
pub(crate) fn notify_error(text: &str) {
    let title = wide(APP_TITLE);
    let body = wide(text);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_ICONERROR,
        );
    }
}

/// 安装目录检查拦下启动：专用提示，说明本次不启动的原因与建议。
///
/// 用 `MB_SYSTEMMODAL | MB_SETFOREGROUND`：双击启动时没有控制台，普通错误框
/// 可能落在别的窗口后面，用户看到的仍是「双击没反应」。
pub(crate) fn notify_startup_blocked(reason: &str) {
    let title = wide(mesh_text::get(
        mesh_text::id::CLIENT_NOTICE_STARTUP_BLOCKED_TITLE,
    ));
    let body = format!(
        "{}\n\n{}",
        mesh_text::format(mesh_text::id::CLIENT_NOTICE_STARTUP_BLOCKED, &[&reason]),
        mesh_text::get(mesh_text::id::CLIENT_NOTICE_STARTUP_BLOCKED_ADVICE),
    );
    let body = wide(&body);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_ICONERROR | MB_SYSTEMMODAL | MB_SETFOREGROUND,
        );
    }
}

/// 用系统默认浏览器打开地址。
///
/// 客户端是 requireAdministrator 的高完整性进程，直接 ShellExecute 会被 UIPI 挡在
/// 「把 URL 交给已运行的浏览器」这一步。explorer.exe 是中等完整性的 shell 代理，
/// 把请求转给它执行，效果与 Go 走 Shell.Application 的 IShellDispatch 相同。
pub(crate) fn open_browser(url: &str) -> Result<(), String> {
    open_with_explorer(url)
}

/// 在资源管理器里打开目录；不需要也不该等它退出。
pub(crate) fn open_dir(dir: &Path) -> Result<(), String> {
    open_with_explorer(&dir.display().to_string())
}

fn open_with_explorer(target: &str) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|err| mesh_text::format(mesh_text::id::CLIENT_ERROR_START_EXPLORER, &[&err]))
}

pub(crate) fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// 把用户动作失败转成一次消息框（引擎的 Notice）。
pub(crate) fn spawn_notice_thread(rx: Receiver<Notice>) {
    let _ = thread::Builder::new()
        .name("mesh-notice".to_string())
        .spawn(move || {
            for notice in rx {
                match notice {
                    Notice::ActionFailed(message) => notify_error(&message),
                }
            }
        });
}

/// 注册控制台事件：Ctrl+C / 关闭控制台时唤醒 headless 主循环，走正常收尾。
pub(crate) fn spawn_signal_thread(tx: Sender<()>) -> Result<(), String> {
    let slot = CTRL_TX.get_or_init(|| Mutex::new(None));
    *slot
        .lock()
        .map_err(|_| mesh_text::get(mesh_text::id::CLIENT_ERROR_SIGNAL_CHANNEL).to_string())? =
        Some(tx);
    if unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) } == 0 {
        return Err(mesh_text::format(
            mesh_text::id::CLIENT_ERROR_REGISTER_CONSOLE_HANDLER,
            &[&std::io::Error::last_os_error()],
        ));
    }
    Ok(())
}

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    if let Some(slot) = CTRL_TX.get() {
        if let Ok(slot) = slot.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(());
            }
        }
    }
    1
}

/// UTF-16 的窄字符串，所有 Win32 W 函数都要它。
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
