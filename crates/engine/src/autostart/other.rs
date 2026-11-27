//! 其它平台不提供开机自启：Linux 上客户端本来就是 systemd 的 `ExecStart`，
//! 自启归服务管理器管，客户端再插一手只会和它打架。

use std::path::Path;

pub(crate) const SUPPORTED: bool = false;

/// 永远为假：自启由 systemd 那类服务管理器负责。
pub(crate) fn enabled() -> Result<bool, String> {
    Ok(false)
}

/// 非 Windows 上没有“开机自启”这个能力。
pub(crate) fn enable(_launcher: &Path) -> Result<(), String> {
    Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_AUTOSTART_UNSUPPORTED).to_string())
}

/// 非 Windows 上没有“开机自启”这个能力。
pub(crate) fn disable() -> Result<(), String> {
    Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_AUTOSTART_UNSUPPORTED).to_string())
}
