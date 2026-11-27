//! 安装根目录的磁盘事实。两个二进制（客户端与启动器）共享的地板。
//!
//! 这里合并的原本是三份实现：Go 客户端的 `internal/{layout,lockfile,platform,version}`
//! 与 Rust 启动器里的 `version.rs`/`failures.rs`/`manifest.rs`。合并的理由是
//! “不比当前新不装”与“同版本失败两次就停手”这两条判据两边都要用，各写一份迟早分叉。
//!
//! 本 crate 不依赖 workspace 内任何其它 crate：它没有上层概念（不知道心跳、不知道面板），
//! 只有“安装根长什么样、版本号怎么比、锁怎么拿”。

pub mod failures;
pub mod layout;
pub mod lock;
pub mod manifest;
pub mod platform;
pub mod signing;
pub mod version;

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 本 crate 的错误。变体只有三种，因为上层要据此分支的也只有这三种：
/// 清单读不出来（安装根不完整/包坏了，版本号非法也归在这里）、普通 IO 错误、
/// 以及“锁被占着”——最后一条必须独立，上层要走“唤起已有实例”而不是报错。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{}",
        mesh_text::format(
            mesh_text::id::INSTALL_ERROR_MANIFEST_READ,
            &[&path.display(), &reason]
        )
    )]
    ManifestRead { path: PathBuf, reason: String },

    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{}", mesh_text::get(mesh_text::id::INSTALL_ERROR_ALREADY_RUNNING))]
    AlreadyRunning,
}

/// 临时文件名的唯一后缀。pid 会复用、时钟会被拨，所以两者都带上再加一个进程内计数器。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 原子替换：写同目录临时文件 → 改名覆盖目标。
///
/// 与 Go 版 `writeFileAtomic` 一样，先写临时文件再 rename：进程被杀只会留下一个
/// 未命名的临时文件，目标位置永远是“旧的”或“新的”，不会是一半。
///
/// 保证的是**进程级**原子：内容只 `flush` 不 `fsync`，改名之后也不同步父目录。
/// 掉电后状态文件与目录变更的先后可能反过来，所以安装事务的持久性承诺只到
/// 「进程中断可恢复」（见 `docs/contract/updates.md`「持久性」）。
/// `perm` 只在 Unix 上生效（Windows 的权限位来自 ACL，跟这个数没有关系）。
pub fn atomic_replace(path: &Path, bytes: &[u8], perm: u32) -> io::Result<()> {
    #[cfg(not(unix))]
    let _ = perm;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                mesh_text::get(mesh_text::id::INSTALL_ERROR_NO_PARENT_DIR),
            )
        })?;
    let mut last_error = None;
    for _ in 0..8 {
        let tmp = dir.join(temp_name());
        let mut file = match create_private(&tmp, perm) {
            Ok(file) => file,
            // 撞名只可能是 pid 复用 + 时钟回拨 + 计数器绕回，换个名字再来就是。
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(err);
                continue;
            }
            Err(err) => return Err(err),
        };
        let written = file.write_all(bytes).and_then(|()| file.flush());
        drop(file);
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
        return match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                Err(err)
            }
        };
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::other(mesh_text::get(
            mesh_text::id::INSTALL_ERROR_TEMP_NAME_COLLISION,
        ))
    }))
}

fn create_private(path: &Path, perm: u32) -> io::Result<File> {
    #[cfg(not(unix))]
    let _ = perm;
    let mut opts = OpenOptions::new();
    // create_new：临时文件必须是自己新建的，覆盖别人的文件会把并发写搅在一起。
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(perm);
    }
    opts.open(path)
}

fn temp_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".tmp-{}-{nanos:x}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-install-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_replace_leaves_no_temp_files() {
        let dir = temp_dir("atomic");
        let path = dir.join("client.json");
        atomic_replace(&path, b"first", 0o600).unwrap();
        atomic_replace(&path, b"second", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件没有清干净: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_replace_reports_a_missing_parent() {
        let err = atomic_replace(Path::new(""), b"x", 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
