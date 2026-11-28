//! 启动前的安装目录安全闸门。
//!
//! Windows 收紧安装根与 `config/` 的 DACL；Unix 只校验既有权限。两者不通过时
//! 先写一条 ERROR 日志，再由调用方终止进程。安装根可写等于能换掉下一次以提权
//! 身份启动的可执行文件，不能带着这种状态继续跑。

use crate::log::Log;
use mesh_text::{format, id};
use std::path::Path;

/// 启动被安装目录检查拦下。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupBlocked {
    /// 写进日志、也写进 stderr 的完整一行（含 ERROR 与「本次不启动」）。
    pub report: String,
    /// 给用户提示的原因，不含日志级别与日志模板。
    pub reason: String,
}

impl StartupBlocked {
    fn new(log: &Log, report: String, reason: String) -> StartupBlocked {
        log.add("app", &report);
        StartupBlocked { report, reason }
    }
}

/// Windows：收紧安装根与 `config/`；任一失败即拦下启动。
///
/// `config_harden_error` 来自 [`mesh_state::Store::open`]：`config/` 的收紧在打开
/// 状态目录时做，失败原因由它带出来。
#[cfg(windows)]
pub(crate) fn check(
    root: &Path,
    data: &Path,
    _slot_dir: &Path,
    config_harden_error: Option<&str>,
    log: &Log,
) -> Result<(), StartupBlocked> {
    check_with(
        root,
        data,
        config_harden_error,
        log,
        mesh_state::harden_install_root,
    )
}

/// 可注入收紧动作的检查入口。测试用它模拟失败，不碰真实 ACL。
#[cfg(windows)]
pub(crate) fn check_with(
    root: &Path,
    data: &Path,
    config_harden_error: Option<&str>,
    log: &Log,
    harden: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), StartupBlocked> {
    if let Err(err) = harden(root) {
        let reason = format(id::ENGINE_ERROR_HARDEN_ROOT, &[&root.display(), &err]);
        return Err(StartupBlocked::new(
            log,
            format(id::ENGINE_LOG_HARDEN_FAILED, &[&reason]),
            reason,
        ));
    }
    if let Some(err) = config_harden_error {
        let config = data.join(mesh_install::layout::CONFIG);
        let reason = format(id::ENGINE_ERROR_HARDEN_CONFIG, &[&config.display(), &err]);
        return Err(StartupBlocked::new(
            log,
            format(id::ENGINE_LOG_HARDEN_FAILED, &[&reason]),
            reason,
        ));
    }
    Ok(())
}

/// Unix：校验安装根、`bin/`、当前槽目录与两个可执行文件；权限不合格即拦下启动。
#[cfg(unix)]
pub(crate) fn check(
    root: &Path,
    _data: &Path,
    slot_dir: &Path,
    _config_harden_error: Option<&str>,
    log: &Log,
) -> Result<(), StartupBlocked> {
    match mesh_state::permissions::verify_install(root, slot_dir) {
        Ok(()) => Ok(()),
        Err(err) => {
            let reason = err.to_string();
            Err(StartupBlocked::new(
                log,
                format(id::ENGINE_LOG_INSTALL_PERMISSIONS_FAILED, &[&reason]),
                reason,
            ))
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-engine-startup-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_failed_root_harden_blocks_startup_and_is_logged_as_an_error() {
        let dir = temp_dir("root");
        let path = dir.join("client.log");
        let log = Log::open(&path).unwrap();
        let blocked = check_with(&dir, &dir, None, &log, |_| {
            Err(io::Error::other("access denied"))
        })
        .unwrap_err();
        assert!(blocked.report.starts_with("ERROR "), "{}", blocked.report);
        assert!(
            blocked.reason.contains("access denied"),
            "{}",
            blocked.reason
        );
        log.close();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("ERROR "), "{text}");
        assert!(text.contains("access denied"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_config_harden_blocks_startup() {
        let dir = temp_dir("config");
        let log = Log::open(&dir.join("client.log")).unwrap();
        let blocked = check_with(&dir, &dir, Some("access denied"), &log, |_| Ok(())).unwrap_err();
        assert!(blocked.report.starts_with("ERROR "), "{}", blocked.report);
        assert!(
            blocked.reason.contains("access denied"),
            "{}",
            blocked.reason
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-engine-startup-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn unsafe_permissions_block_startup_and_name_the_failing_path() {
        let root = temp_dir("unix-root");
        let slot = root.join("bin").join("alice");
        fs::create_dir_all(&slot).unwrap();
        for name in [
            mesh_install::platform::client_exe(),
            mesh_install::platform::core_name(),
        ] {
            fs::write(slot.join(name), b"").unwrap();
            set_mode(&slot.join(name), 0o755);
        }
        set_mode(&slot, 0o755);
        set_mode(&root, 0o777);
        let path = root.join("client.log");
        let log = Log::open(&path).unwrap();

        let blocked = check(&root, &slot, None, &log).unwrap_err();
        assert!(blocked.report.starts_with("ERROR "), "{}", blocked.report);
        assert!(blocked.reason.contains(&root.display().to_string()));
        log.close();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("ERROR "), "{text}");
        let _ = fs::remove_dir_all(&root);
    }
}
