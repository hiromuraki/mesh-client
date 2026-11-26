//! 随操作系统变化的文件名与平台标识。
//!
//! 平台标识是 `GOOS/GOARCH` 形式（`windows/amd64`）：服务端就按这个键索引发布包，
//! 报错了会拿到别的平台的包。所以只列这两种平台，其它目标直接编译失败，
//! 不猜一个“差不多”的值。

#[cfg(windows)]
const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const EXE_SUFFIX: &str = "";

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const PLATFORM_ID: &str = "windows/amd64";
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
const PLATFORM_ID: &str = "windows/arm64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PLATFORM_ID: &str = "linux/amd64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const PLATFORM_ID: &str = "linux/arm64";
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
)))]
compile_error!("未支持的目标平台：平台标识必须与发布包的 GOOS/GOARCH 槽位一致");

/// 版本目录里的主程序名（`<root>/bin/mesh-client.exe`）。
pub fn client_exe() -> &'static str {
    const NAME: &str = "mesh-client";
    // 用 match 而不是运行时拼接：返回值是 `&'static str`，这里一次分配都不做。
    match EXE_SUFFIX {
        ".exe" => "mesh-client.exe",
        _ => NAME,
    }
}

/// 安装根目录下固定不变的启动器名。
pub fn launcher_name() -> &'static str {
    match EXE_SUFFIX {
        ".exe" => "mesh.exe",
        _ => "mesh",
    }
}

/// 更新包必须带上的内核名。
pub fn core_name() -> &'static str {
    match EXE_SUFFIX {
        ".exe" => "easytier-core.exe",
        _ => "easytier-core",
    }
}

/// `GOOS/GOARCH` 形式的平台标识。
pub fn id() -> &'static str {
    PLATFORM_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_exe_suffix() {
        assert!(client_exe().starts_with("mesh-client"));
        assert!(core_name().starts_with("easytier-core"));
        assert!(launcher_name().starts_with("mesh"));
        #[cfg(windows)]
        {
            assert!(client_exe().ends_with(".exe"));
            assert_eq!(launcher_name(), "mesh.exe");
        }
        #[cfg(not(windows))]
        {
            assert!(!client_exe().ends_with(".exe"));
            assert_eq!(launcher_name(), "mesh");
        }
    }

    #[test]
    fn platform_id_matches_the_goos_goarch_shape() {
        let id = id();
        let (os, arch) = id.split_once('/').unwrap();
        assert_eq!(os, std::env::consts::OS);
        assert!(matches!(arch, "amd64" | "arm64"));
    }
}
