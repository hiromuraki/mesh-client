//! --boot-check 标记：安装器靠它判断新版本「能不能正常跑起来」。
//!
//! 引擎与面板都已经就位时写，网络通不通不算失败——那不该触发回退。

use std::path::Path;

/// 在安装根写 .boot-ok-<标记>。
pub(crate) fn write_marker(root: &Path, token: &str) -> Result<(), String> {
    let marker = root.join(format!(".boot-ok-{token}"));
    std::fs::write(&marker, b"ok").map_err(|err| {
        mesh_text::format(
            mesh_text::id::CLIENT_ERROR_WRITE_BOOT_MARKER,
            &[&marker.display(), &err],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_token_named_marker() {
        let root = std::env::temp_dir().join(format!("mesh-boot-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        write_marker(&root, "2026.11.26-21").expect("写启动标记");
        assert_eq!(
            std::fs::read_to_string(root.join(".boot-ok-2026.11.26-21")).unwrap(),
            "ok"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
