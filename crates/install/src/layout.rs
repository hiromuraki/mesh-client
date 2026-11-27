//! 安装根的形状。两个平台同一套路径，路径不随版本变化——防火墙规则、systemd 的
//! `ExecStart`、监控里盯着的路径都不会每换一版就换一次。完整约定见
//! `contract/updates.md`。

use crate::manifest;
use crate::Error;
use std::io;
use std::path::{Path, PathBuf};

/// 两个槽的父目录。槽目录是它下面的 `alice` / `bob`。
pub const BIN: &str = "bin";
/// 槽名。固定的两个，永不含版本号：路径集合恒定，防火墙与杀软规则才不增不减。
pub const SLOT_ALICE: &str = "alice";
/// 另一个槽。
pub const SLOT_BOB: &str = "bob";
/// 当前槽。一行文本，值只能是两个槽名之一。安装根的持久安装状态只有这一个文件。
pub const CURRENT: &str = "current";
/// 下载好的更新包。文件本身就是“请安装”的信号。
pub const UPDATE_FILE: &str = "update.tar.zst";
/// 按版本记的连续启动失败次数。启动器写，客户端读。
pub const FAILURES: &str = ".update-failures.json";
/// 单实例锁：同一份 `config/` 不能被两个客户端同时用。
pub const LOCK: &str = "instance.lock";
/// 安装根的写权：槽目录、`bin/current`、待装包同时只能有一个写者。
/// 与 `LOCK` 是两个问题——那一个问「客户端是不是只有一个」，这一个问「谁在改安装根」。
/// 启动器在一次安装事务之内持有它；Linux 的常驻启动器每轮从选槽握到确认。
pub const INSTALL_LOCK: &str = "install.lock";
/// 版本清单，记在本 crate 里是为了和槽目录绑在一起。
pub const MANIFEST: &str = "version.json";
/// 配置与身份。跨版本共享，槽整个换掉时它不动。
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

/// 把目录名归一成槽名：只做 ASCII 小写比较，别的形状一律不认。
///
/// `alice` 与 `bob` 都是 ≤8 字符的合法 8.3 名，系统不会为它们生成短名，所以这里
/// 不处理 8.3；客户端据此判断自己的父目录是不是槽目录。
pub fn slot_name(dir_name: &str) -> Option<&'static str> {
    [SLOT_ALICE, SLOT_BOB]
        .into_iter()
        .find(|slot| dir_name.eq_ignore_ascii_case(slot))
}

/// 一个槽的目录：`<root>/bin/<槽名>`。
pub fn slot_dir(root: &Path, slot: &str) -> PathBuf {
    root.join(BIN).join(slot)
}

/// `bin/current` 的路径。
pub fn current_path(root: &Path) -> PathBuf {
    root.join(BIN).join(CURRENT)
}

/// 读当前槽。文件不在、读不出来、内容不是两个槽名之一都是 None——「没有提交过」
/// 与「值非法」在判据上同义，都由启动器的修复写收口。
///
/// 这里的值必须是**规范槽名**本身（首尾空白忽略）：大小写规范化只用于判断
/// 可执行文件所在的目录名（[slot_name]），不用来解释这个文件的内容。
pub fn read_current(root: &Path) -> Option<&'static str> {
    let text = std::fs::read_to_string(current_path(root)).ok()?;
    let text = text.trim();
    [SLOT_ALICE, SLOT_BOB]
        .into_iter()
        .find(|slot| text == *slot)
}

/// 原子写当前槽：写同目录临时文件再改名，重复写同值是 no-op。
///
/// 保证的是进程级原子：内容只 flush 不 fsync，掉电后 `current` 与槽内容可能不一致，
/// 那种形状由「槽的有效性每次现判」兜住（见 `contract/updates.md`「持久性」）。
/// 提交不取 `install.lock`：启动器等确认时正持有它，取锁就永远提交不了。
pub fn write_current(root: &Path, slot: &str) -> io::Result<()> {
    let slot = slot_name(slot).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            mesh_text::format(mesh_text::id::INSTALL_ERROR_INVALID_SLOT, &[&slot]),
        )
    })?;
    if read_current(root) == Some(slot) {
        return Ok(());
    }
    let body = format!("{slot}\n");
    crate::atomic_replace(&current_path(root), body.as_bytes(), 0o644)
}

/// 返回某个槽的版本号：来源是该槽的 `version.json`。
///
/// 客户端读的是**自己所在的槽**：启动确认之前 `bin/current` 还指向旧槽（首次安装
/// 时甚至不存在），那一刻唯一能回答「我在跑哪一版」的就是自己目录里的清单。
///
/// 读不到、解析不了、版本号不合法都是错误——没有“猜一个默认版本”的余地：
/// 版本号错一位会让控制面把设备钉在“有更新”上，或者让启动器把降级当成升级。
/// 形状只有 [`crate::version::parse`] 一份规则，这里不重复判断。
pub fn read_version(slot_dir: &Path) -> Result<String, Error> {
    Ok(manifest::read(&slot_dir.join(MANIFEST))?
        .version()
        .to_string())
}

/// 从主程序的路径定位安装根：主程序在 `<root>/bin/<槽>` 里，根是槽目录往上第三级。
///
/// 只有客户端用得到（启动器自己就在根下，它取自身目录）。调用方传的必须是绝对
/// 路径（`std::env::current_exe()`）。形状不对（例如从别处手工运行）时不猜根，
/// 退回可执行文件所在目录：调用方随后的读清单会给出准确路径。
pub fn find_root(exe: &Path) -> PathBuf {
    let exe_dir = match exe.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => return PathBuf::from("."),
    };
    let in_slot = exe_dir
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(slot_name)
        .is_some();
    let bin_dir = exe_dir
        .parent()
        .filter(|dir| dir.file_name().is_some_and(|name| name == BIN));
    match (in_slot, bin_dir.and_then(Path::parent)) {
        (true, Some(root)) if !root.as_os_str().is_empty() => root.to_path_buf(),
        _ => exe_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    fn write_slot_manifest(root: &Path, slot: &str) {
        let dir = slot_dir(root, slot);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            format!(
                r#"{{"version":"2026.11.26-13","slot":"client","platform":"windows/amd64","files":{{"mesh-client.exe":"{}"}},"signature":"{}"}}"#,
                "ab".repeat(32),
                "cd".repeat(64)
            ),
        )
        .unwrap();
    }

    /// 版本来自槽自己的清单，不依赖 `bin/current`：首次安装时客户端先于提交运行。
    #[test]
    fn reads_the_version_from_the_slot_itself() {
        let root = temp_dir("layout-read");
        write_slot_manifest(&root, SLOT_ALICE);
        assert_eq!(read_current(&root), None);
        assert_eq!(
            read_version(&slot_dir(&root, SLOT_ALICE)).unwrap(),
            "2026.11.26-13"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_broken_manifests_are_errors() {
        let root = temp_dir("layout-broken");
        let dir = slot_dir(&root, SLOT_BOB);
        std::fs::create_dir_all(&dir).unwrap();
        write_current(&root, SLOT_BOB).unwrap();
        assert!(matches!(
            read_version(&dir).unwrap_err(),
            Error::ManifestRead { .. }
        ));
        std::fs::write(dir.join(MANIFEST), "{").unwrap();
        assert!(matches!(
            read_version(&dir).unwrap_err(),
            Error::ManifestRead { .. }
        ));
        // 版本号不合法由清单解析先拦下：形状规则只有 `valid_version` 一份。
        std::fs::write(
            dir.join(MANIFEST),
            format!(
                r#"{{"version":"bad/version","slot":"client","platform":"windows/amd64","files":{{"a.exe":"{}"}},"signature":"{}"}}"#,
                "ab".repeat(32),
                "cd".repeat(64)
            ),
        )
        .unwrap();
        assert!(matches!(
            read_version(&dir).unwrap_err(),
            Error::ManifestRead { .. }
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_two_slot_names_are_recognized() {
        assert_eq!(slot_name("alice"), Some(SLOT_ALICE));
        assert_eq!(slot_name("bob"), Some(SLOT_BOB));
        assert_eq!(slot_name("Alice"), Some(SLOT_ALICE));
        assert_eq!(slot_name("BOB"), Some(SLOT_BOB));
        assert_eq!(slot_name(" carol"), None);
        assert_eq!(slot_name("alice "), None);
        assert_eq!(slot_name("alice."), None);
    }

    #[test]
    fn current_is_trimmed_and_written_atomically() {
        let root = temp_dir("layout-current");
        std::fs::create_dir_all(root.join(BIN)).unwrap();
        write_current(&root, SLOT_BOB).unwrap();
        assert_eq!(
            std::fs::read_to_string(current_path(&root)).unwrap(),
            "bob\n"
        );
        std::fs::write(current_path(&root), "  alice  \n").unwrap();
        assert_eq!(read_current(&root), Some(SLOT_ALICE));
        std::fs::write(current_path(&root), "ALICE\n").unwrap();
        assert_eq!(read_current(&root), None, "值必须是规范槽名");
        std::fs::write(current_path(&root), "carol\n").unwrap();
        assert_eq!(read_current(&root), None);
        let leftovers: Vec<_> = std::fs::read_dir(root.join(BIN))
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with(".tmp-"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "临时文件没有清干净: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writing_an_unknown_slot_is_an_error() {
        let root = temp_dir("layout-bad-slot");
        std::fs::create_dir_all(root.join(BIN)).unwrap();
        let err = write_current(&root, "carol").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!current_path(&root).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_root_is_three_levels_up_from_the_client() {
        let root = temp_dir("layout-root");
        let exe = slot_dir(&root, SLOT_ALICE).join(crate::platform::client_exe());
        assert_eq!(find_root(&exe), root);
        // 不在槽里的路径不猜根：退回可执行文件所在目录。
        let stray = root.join("elsewhere").join("mesh-client");
        assert_eq!(find_root(&stray), root.join("elsewhere"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
