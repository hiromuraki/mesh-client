//! 版本号与 Windows 资源。
//!
//! 客户端报给控制面的版本始终来自 bin/version.json（运行时读），构建号只进
//! Windows 的 VERSIONINFO 与 --help 的最后一行：两处都必须是这次构建的事实，
//! 不能默认一个 0.0.0。
//!
//! 资源只在 release 里编：把 requireAdministrator 清单塞进测试二进制会让它在
//! SxS 里起不来（启动器的 build.rs 有同样的说明）。debug 构建因此不需要 UAC，
//! 正好可以用来在临时目录里验证安装路径。

fn main() {
    println!("cargo:rerun-if-changed=build/windows/client.exe.manifest");
    println!("cargo:rerun-if-changed=assets/tray.ico");
    println!("cargo:rerun-if-changed=../launcher/assets/icon.ico");
    println!("cargo:rerun-if-env-changed=MESH_VERSION");
    let version = std::env::var("MESH_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".to_string())
        });
    println!("cargo:rustc-env=MESH_CLIENT_BUILD_VERSION={version}");
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
            && std::env::var("PROFILE").as_deref() == Ok("release")
        {
            compile_resources(&version);
        }
    }
}

/// VERSIONINFO 的 FileVersion 只接受四段数字；把 2026.11.26-21 压成
/// 2026.11.26.21，多段截断、缺段补零。
#[cfg(windows)]
fn numeric_version(raw: &str) -> String {
    let mut parts: Vec<String> = raw
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(4)
        .map(|part| part.trim_start_matches('0').to_string())
        .map(|part| {
            if part.is_empty() {
                "0".to_string()
            } else {
                part
            }
        })
        .collect();
    while parts.len() < 4 {
        parts.push("0".to_string());
    }
    parts.join(".")
}

#[cfg(windows)]
fn compile_resources(version: &str) {
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("../launcher/assets/icon.ico");
    resource.set_manifest_file("build/windows/client.exe.manifest");
    resource.set("FileDescription", "Mesh Client");
    resource.set("ProductName", "Mesh");
    resource.set("OriginalFilename", "mesh-client.exe");
    resource.set("ProductVersion", version);
    resource.set("FileVersion", &numeric_version(version));
    if let Err(err) = resource.compile() {
        panic!("编译 Windows 资源失败: {err}");
    }
}
