//! 启动成功时提交 `bin/current`。
//!
//! 只有客户端知道引擎与面板（桌面版还有托盘）是否真的就位，所以提交权在它手里。
//! 启动器只修复 `current`（缺失、值非法或指向的槽无效），从不提交。

use mesh_install::layout;
use std::path::Path;

/// 把「正在跑的这个槽」写进 `bin/current`。
///
/// 父目录名按 ASCII 小写比较，只认 `alice` / `bob`；不是槽名（例如从别处手工运行）
/// 就什么都不做。写失败只影响这一次提交：`current` 保持旧值，下一轮按选槽判据重试。
pub(crate) fn commit_current(root: &Path, program_dir: &Path) -> Result<(), String> {
    let Some(slot) = program_dir
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(layout::slot_name)
    else {
        return Ok(());
    };
    layout::write_current(root, slot).map_err(|err| {
        mesh_text::format(
            mesh_text::id::CLIENT_ERROR_WRITE_CURRENT,
            &[&layout::current_path(root).display(), &err],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-client-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn commits_the_slot_named_by_the_program_directory() {
        let root = temp_dir("commit");
        let dir = layout::slot_dir(&root, layout::SLOT_ALICE);
        std::fs::create_dir_all(&dir).unwrap();
        commit_current(&root, &dir).unwrap();
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 大小写不同的父目录名归一成规范槽名（只做 ASCII 小写比较）。
    #[test]
    fn an_uppercase_parent_still_commits_the_canonical_name() {
        let root = temp_dir("commit-case");
        let upper = root.join(layout::BIN).join("BOB");
        std::fs::create_dir_all(&upper).unwrap();
        commit_current(&root, &upper).unwrap();
        assert_eq!(
            std::fs::read_to_string(layout::current_path(&root)).unwrap(),
            "bob\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_program_directory_outside_a_slot_commits_nothing() {
        let root = temp_dir("commit-outside");
        let dir = root.join(layout::BIN);
        std::fs::create_dir_all(&dir).unwrap();
        commit_current(&root, &dir).unwrap();
        assert_eq!(layout::read_current(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
