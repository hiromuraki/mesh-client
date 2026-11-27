use crate::layout;
use crate::Error;
use fs4::fs_std::FileExt;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// `instance.lock` 的独占句柄。
///
/// 用的是操作系统的文件锁（Windows 的 `LockFileEx`、Unix 的 `flock`），不是
/// “文件存在即运行中”：客户端崩溃时内核会释放锁，不会留下一个要人工删的文件。
/// 客户端退出即释放，句柄的生命周期就是“这一份 config 被我占着”的生命周期。
pub struct Lock {
    file: File,
}

/// 在 `path` 上加非阻塞独占锁。已有进程持锁时返回 [`Error::AlreadyRunning`]。
pub fn acquire(path: &Path) -> Result<Lock, Error> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Lock { file }),
        Err(err) if is_contended(&err) => Err(Error::AlreadyRunning),
        Err(err) => Err(Error::Io(err)),
    }
}

/// 判断错误是不是“锁在别人手里”。
///
/// 两个平台报的不是同一个 errno（Unix `EWOULDBLOCK`、Windows
/// `ERROR_LOCK_VIOLATION`），fs4 把平台那一份统一成了 `WouldBlock`；两者都比，
/// 免得某个后端只做了其中一半。
fn is_contended(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    match (
        err.raw_os_error(),
        fs4::lock_contended_error().raw_os_error(),
    ) {
        (Some(got), Some(expected)) => got == expected,
        _ => false,
    }
}

impl Lock {
    /// 锁文件的路径由调用方决定；这里只提供安装根下的标准位置。
    pub fn path(root: &Path) -> std::path::PathBuf {
        root.join(layout::LOCK)
    }

    /// 安装根写权的锁文件。启动器持有整个安装事务，客户端提交更新包时也走它。
    pub fn owner_path(root: &Path) -> std::path::PathBuf {
        root.join(layout::INSTALL_LOCK)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // 显式解锁再关文件：正常退出时立刻让位，不必等进程结束（启动器在安装
        // 前后都要读同一份安装根）。没有持锁时 unlock 会失败，忽略即可。
        //
        // 必须写全路径：std 从 1.89 起给 File 加了同名的固有方法，直接
        // `self.file.unlock()` 会解析到它，把我们 rust-version = 1.83 的底线抬上去。
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temp_dir;

    #[test]
    fn second_acquire_reports_already_running() {
        let root = temp_dir("lock-busy");
        let path = Lock::path(&root);
        let held = acquire(&path).unwrap();
        assert!(matches!(acquire(&path), Err(Error::AlreadyRunning)));
        drop(held);
        // 让位之后同一个进程还能再拿到，说明释放是真的释放了。
        assert!(acquire(&path).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
}
