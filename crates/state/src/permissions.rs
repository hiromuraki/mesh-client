//! 安装目录的权限校验（Unix）。
//!
//! 客户端以 root 运行时，能改写安装根或槽内可执行文件的人就能换掉下一次启动的
//! 客户端。权限本身由部署决定（安装包 0755、属主 root），这里只验证它没有被放宽：
//! 不合格即拒绝启动，不尝试修正。

use mesh_text::id;
use std::path::{Path, PathBuf};

/// 组或其他用户可写的位。
const SHARED_WRITE: u32 = 0o022;
/// 任一可执行位。
const ANY_EXECUTE: u32 = 0o111;

/// 权限校验未通过的项。每一项都指向具体的路径与判据。
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    #[error(
        "{}",
        mesh_text::format(id::STATE_ERROR_INSTALL_METADATA, &[&path.display(), &source])
    )]
    Metadata {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "{}",
        mesh_text::format(id::STATE_ERROR_INSTALL_OWNER, &[&path.display(), &owner, &euid])
    )]
    Owner {
        path: PathBuf,
        owner: u32,
        euid: u32,
    },
    #[error(
        "{}",
        mesh_text::format(
            id::STATE_ERROR_INSTALL_SHARED_WRITE,
            &[&path.display(), &format!("{:04o}", mode & 0o777)]
        )
    )]
    SharedWrite { path: PathBuf, mode: u32 },
    #[error(
        "{}",
        mesh_text::format(id::STATE_ERROR_INSTALL_NOT_EXECUTABLE, &[&path.display()])
    )]
    NotExecutable { path: PathBuf },
}

/// 校验安装根、`bin/`、当前槽目录与其两个可执行文件。
///
/// 属主必须是 root 或当前有效用户；目录与可执行文件都不得被组或其他用户写；
/// 客户端与 core 必须有可执行位。
pub fn verify_install(root: &Path, slot_dir: &Path) -> Result<(), AccessError> {
    let euid = effective_uid();
    check_path(root, euid, false)?;
    // `bin/` 是根与槽之间的一级：它能被他人写，就能整体换掉槽目录。
    if let Some(parent) = slot_dir.parent() {
        if parent != root && parent.starts_with(root) {
            check_path(parent, euid, false)?;
        }
    }
    check_path(slot_dir, euid, false)?;
    check_path(
        &slot_dir.join(mesh_install::platform::client_exe()),
        euid,
        true,
    )?;
    check_path(
        &slot_dir.join(mesh_install::platform::core_name()),
        euid,
        true,
    )?;
    Ok(())
}

fn check_path(path: &Path, euid: u32, executable: bool) -> Result<(), AccessError> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(path).map_err(|source| AccessError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    let owner = meta.uid();
    if owner != 0 && owner != euid {
        return Err(AccessError::Owner {
            path: path.to_path_buf(),
            owner,
            euid,
        });
    }
    let mode = meta.mode();
    if mode & SHARED_WRITE != 0 {
        return Err(AccessError::SharedWrite {
            path: path.to_path_buf(),
            mode,
        });
    }
    if executable && mode & ANY_EXECUTE == 0 {
        return Err(AccessError::NotExecutable {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// 有效用户 ID。Unix 上 std 没有对应接口，只能问 libc。
fn effective_uid() -> u32 {
    // SAFETY: geteuid 没有参数与前置条件，失败路径也不存在。
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-state-permissions-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 一个权限全部合格的安装：root/bin/alice 与两个 0755 的可执行文件。
    fn install(tag: &str) -> (PathBuf, PathBuf) {
        let root = temp_root(tag);
        let slot = root.join("bin").join("alice");
        fs::create_dir_all(&slot).unwrap();
        for name in [
            mesh_install::platform::client_exe(),
            mesh_install::platform::core_name(),
        ] {
            let path = slot.join(name);
            fs::write(&path, b"").unwrap();
            set_mode(&path, 0o755);
        }
        set_mode(&root, 0o755);
        set_mode(&root.join("bin"), 0o755);
        set_mode(&slot, 0o755);
        (root, slot)
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_well_formed_install_passes() {
        let (root, slot) = install("ok");
        verify_install(&root, &slot).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_group_writable_root_is_rejected() {
        let (root, slot) = install("root-group");
        set_mode(&root, 0o775);
        assert!(matches!(
            verify_install(&root, &slot),
            Err(AccessError::SharedWrite { ref path, .. }) if path == &root
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_group_writable_bin_is_rejected() {
        let (root, slot) = install("bin-group");
        let bin = root.join("bin");
        set_mode(&bin, 0o775);
        assert!(matches!(
            verify_install(&root, &slot),
            Err(AccessError::SharedWrite { ref path, .. }) if path == &bin
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_group_writable_slot_is_rejected() {
        let (root, slot) = install("slot-group");
        set_mode(&slot, 0o775);
        assert!(matches!(
            verify_install(&root, &slot),
            Err(AccessError::SharedWrite { ref path, .. }) if path == &slot
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_other_writable_executable_is_rejected() {
        let (root, slot) = install("core-other");
        let core = slot.join(mesh_install::platform::core_name());
        set_mode(&core, 0o757);
        assert!(matches!(
            verify_install(&root, &slot),
            Err(AccessError::SharedWrite { ref path, .. }) if path == &core
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_client_without_execute_bits_is_rejected() {
        let (root, slot) = install("client-noexec");
        let client = slot.join(mesh_install::platform::client_exe());
        set_mode(&client, 0o644);
        assert!(matches!(
            verify_install(&root, &slot),
            Err(AccessError::NotExecutable { ref path }) if path == &client
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_core_is_reported_as_a_metadata_error() {
        let (root, slot) = install("core-missing");
        fs::remove_file(slot.join(mesh_install::platform::core_name())).unwrap();
        assert!(matches!(
            verify_install(&root, &slot),
            Err(AccessError::Metadata { .. })
        ));
        let _ = fs::remove_dir_all(&root);
    }

    /// chown 需要 root；这里直接喂一个不匹配的有效 uid，覆盖属主判据。
    /// 以 root 跑测试时文件属主就是 0，而 0 按设计总是合格，改测另一条分支。
    #[test]
    fn an_owner_that_is_neither_root_nor_current_is_rejected() {
        let (root, slot) = install("owner");
        let client = slot.join(mesh_install::platform::client_exe());
        let owner = fs::metadata(&client).unwrap().uid();
        if owner == 0 {
            check_path(&client, 1, true).unwrap();
        } else {
            let other = owner.wrapping_add(1);
            assert!(matches!(
                check_path(&client, other, true),
                Err(AccessError::Owner { owner: seen, euid, .. }) if seen == owner && euid == other
            ));
        }
        let _ = fs::remove_dir_all(&root);
    }
}
