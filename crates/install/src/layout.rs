//! 安装根的形状。两个平台同一套路径，路径不随版本变化——防火墙规则、systemd 的
//! `ExecStart`、监控里盯着的路径都不会每换一版就换一次。完整约定见
//! `contract/updates.md`。

use crate::manifest;
use crate::Error;
use std::path::{Path, PathBuf};

/// 当前版本：主程序、`easytier-core`、依赖库与 `version.json`。
pub const BIN: &str = "bin";
/// 上一版本，只在一次安装的窗口里存在。它存在**不**等于“上次安装没走完”：
/// 那个判据由 `INSTALL_STATE` 表达。
pub const BIN_OLD: &str = "bin.old";
/// 回滚时被丢下的新版本，只在一次回滚的窗口里存在。换回旧版本要先把它挪开，
/// 不能就地删：改名和删除不是一次操作，中间断掉时旧版本必须还在。
pub const BIN_STALE: &str = "bin.stale";
/// 一次安装的事务状态（`fresh` / `replaced` / `installed` / `restoring` /
/// `committed`）。没有这个文件就等于没有在途的安装，`bin/` 是已确认能启动的那一份。
pub const INSTALL_STATE: &str = "install.state";
/// 下载好的更新包。文件本身就是“请安装”的信号。
pub const UPDATE_FILE: &str = "update.tar.zst";
/// 按版本记的连续启动失败次数。启动器写，客户端读。
pub const FAILURES: &str = ".update-failures.json";
/// 单实例锁：同一份 `config/` 不能被两个客户端同时用。
pub const LOCK: &str = "instance.lock";
/// 安装根的写权：`install.state`、`bin/`、`bin.old/`、待装包同时只能有一个写者。
/// 与 `LOCK` 是两个问题——那一个问「客户端是不是只有一个」，这一个问「谁在改安装根」。
/// 启动器在一次事务之内持有它；Linux 的常驻启动器持有到进程结束。
pub const INSTALL_LOCK: &str = "install.lock";
/// 版本清单，记在本 crate 里是为了和 `bin/` 相对位置绑在一起。
pub const MANIFEST: &str = "version.json";
/// 配置与身份。跨版本共享，`bin/` 整个换掉时它不动。
pub const CONFIG: &str = "config";
/// 启动器自报的版本号。控制面只能给摘要，摘要回答不了「这份是不是比现在的新」，
/// 所以「盘上这份启动器是哪一版」由启动器自己写在安装根里。
pub const LAUNCHER_VERSION: &str = "launcher.json";
/// 日志。同样跨版本共享。
pub const LOG: &str = "log";

/// `launcher.json` 的内容。字段只有版本号：它描述的是安装根下那份启动器。
#[derive(serde::Deserialize, serde::Serialize)]
struct LauncherRecord {
    version: String,
}

/// 写下「盘上这份启动器是哪一版」。只有启动器自己调用：它是唯一确定自己就是
/// 那个文件的进程。写失败是调用方的事——调用点只记日志，不拦启动。
pub fn write_launcher_version(root: &Path, version: &str) -> std::io::Result<()> {
    let record = LauncherRecord {
        version: version.to_string(),
    };
    let body = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
    crate::atomic_replace(&root.join(LAUNCHER_VERSION), &body, 0o644)
}

/// 读启动器自报的版本号。文件不在或内容不认识都是 None（还没被新启动器运行过），
/// 调用方据此放行一次：没有记录时不判降级。
pub fn read_launcher_version(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(LAUNCHER_VERSION)).ok()?;
    let record: LauncherRecord = serde_json::from_str(&text).ok()?;
    Some(record.version)
}

/// `bin/version.json` 的路径。
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(BIN).join(MANIFEST)
}

/// 返回当前版本号：唯一来源是 `<root>/bin/version.json`。
///
/// 读不到、解析不了、版本号不合法都是错误——没有“猜一个默认版本”的余地：
/// 版本号错一位会让控制面把设备钉在“有更新”上，或者让启动器把降级当成升级。
/// 形状只有 [`crate::version::parse`] 一份规则，这里不重复判断。
pub fn read_version(root: &Path) -> Result<String, Error> {
    Ok(manifest::read(&manifest_path(root))?.version().to_string())
}

/// 从主程序的路径定位安装根：主程序在 `<root>/bin` 里，根是 `bin` 的上一级。
///
/// 只有客户端用得到（启动器自己就在根下，它取自身目录）。调用方传的必须是绝对
/// 路径（`std::env::current_exe()`），相对路径会按当前目录解析出来一个别的根。
pub fn find_root(exe: &Path) -> PathBuf {
    let exe_dir = match exe.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => return PathBuf::from("."),
    };
    match exe_dir.parent() {
        Some(root) if !root.as_os_str().is_empty() => root.to_path_buf(),
        // 单层路径（`mesh-client.exe` 就地运行）：退回它自己所在目录，不猜。
        _ => exe_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    #[test]
    fn reads_the_version_from_bin_version_json() {
        let root = temp_dir("layout-read");
        std::fs::create_dir_all(root.join(BIN)).unwrap();
        std::fs::write(
            root.join(BIN).join(MANIFEST),
            format!(
                r#"{{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{{"mesh-client.exe":"{}"}},"signature":"{}"}}"#,
                "ab".repeat(32),
                "cd".repeat(64)
            ),
        )
        .unwrap();
        assert_eq!(read_version(&root).unwrap(), "2026.11.26-13");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_broken_manifests_are_errors() {
        let root = temp_dir("layout-broken");
        assert!(matches!(
            read_version(&root).unwrap_err(),
            Error::ManifestRead { .. }
        ));
        std::fs::create_dir_all(root.join(BIN)).unwrap();
        std::fs::write(root.join(BIN).join(MANIFEST), "{").unwrap();
        assert!(matches!(
            read_version(&root).unwrap_err(),
            Error::ManifestRead { .. }
        ));
        // 版本号不合法由清单解析先拦下：形状规则只有 `valid_version` 一份。
        std::fs::write(
            root.join(BIN).join(MANIFEST),
            format!(
                r#"{{"version":"bad/version","slot":"client","platform":"windows/amd64","files":{{"a.exe":"{}"}},"signature":"{}"}}"#,
                "ab".repeat(32),
                "cd".repeat(64)
            ),
        )
        .unwrap();
        assert!(matches!(
            read_version(&root).unwrap_err(),
            Error::ManifestRead { .. }
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_root_is_the_parent_of_bin() {
        let root = temp_dir("layout-root");
        let exe = root.join(BIN).join(crate::platform::client_exe());
        assert_eq!(find_root(&exe), root);
        let _ = std::fs::remove_dir_all(&root);
    }
}
