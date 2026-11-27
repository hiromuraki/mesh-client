//! 安装根的形状。两个平台同一套路径，路径不随版本变化——防火墙规则、systemd 的
//! `ExecStart`、监控里盯着的路径都不会每换一版就换一次。完整约定见
//! `contract/updates.md`。

use crate::manifest;
use crate::Error;
use std::path::{Path, PathBuf};

/// 当前版本：主程序、`easytier-core`、依赖库与 `version.json`。
pub const BIN: &str = "bin";
/// 上一版本，只在一次安装的窗口里存在。它存在就等于“上次安装没走完”。
pub const BIN_OLD: &str = "bin.old";
/// 下载好的更新包。文件本身就是“请安装”的信号。
pub const UPDATE_FILE: &str = "update.tar.zst";
/// 按版本记的连续启动失败次数。启动器写，客户端读。
pub const FAILURES: &str = ".update-failures.json";
/// 单实例锁：同一份 `config/` 不能被两个客户端同时用。
pub const LOCK: &str = "instance.lock";
/// 版本清单，记在本 crate 里是为了和 `bin/` 相对位置绑在一起。
pub const MANIFEST: &str = "version.json";
/// 配置与身份。跨版本共享，`bin/` 整个换掉时它不动。
pub const CONFIG: &str = "config";
/// 日志。同样跨版本共享。
pub const LOG: &str = "log";

/// 版本号长度上限：它同时是 `bin/old` 里目录名与仓库里 tag 的形状。
pub const MAX_VERSION_LEN: usize = 64;

/// 版本号直接当文件名用，所以只接受不会逃出安装目录的字符。
///
/// 与 Go 版的 `^[0-9A-Za-z][0-9A-Za-z._-]*$` 等价：首字符必须字母数字，
/// 后面允许 `.`、`_`、`-`；非 ASCII 一律拒绝。
pub fn valid_version(v: &str) -> bool {
    if v.is_empty() || v.len() > MAX_VERSION_LEN {
        return false;
    }
    let mut bytes = v.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// `bin/version.json` 的路径。
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(BIN).join(MANIFEST)
}

/// 返回当前版本号：唯一来源是 `<root>/bin/version.json`。
///
/// 读不到、解析不了、版本号不合法都是错误——没有“猜一个默认版本”的余地：
/// 版本号错一位会让控制面把设备钉在“有更新”上，或者让启动器把降级当成升级。
/// 版本号形状由清单解析统一把关（`Manifest::validate` 用的就是本模块的
/// `valid_version`），这里不再重复判断：两处各写一份规则，迟早分叉。
pub fn read_version(root: &Path) -> Result<String, Error> {
    Ok(manifest::read(&manifest_path(root))?.version)
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
    fn version_charset_matches_the_go_regexp() {
        for ok in ["2026.11.26-13", "a", "2026.11.26_13", "A-1"] {
            assert!(valid_version(ok), "{ok:?} 应该合法");
        }
        for bad in [
            "",
            ".2026.11.26-13",
            "-2026.11.26-13",
            "2026/11/26-13",
            "2026.11.26 13",
            "2026.11.26-13\n",
            "2026.11.26-13\\",
            "版本",
        ] {
            assert!(!valid_version(bad), "{bad:?} 不该合法");
        }
        assert!(valid_version(&"a".repeat(MAX_VERSION_LEN)));
        assert!(!valid_version(&"a".repeat(MAX_VERSION_LEN + 1)));
    }

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
