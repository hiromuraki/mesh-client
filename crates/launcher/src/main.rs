#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod args;
mod install;
mod launch;
mod log;
#[cfg(unix)]
mod supervisor;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 启动器是命令行程序：没有 shell 窗口、有图标。成功不说话，失败写日志和 stderr。
fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // 先解析再决定安装根：--root 可以覆盖它，--help 不需要根。
    let opts = match args::parse(&argv) {
        Ok(opts) => opts,
        Err(args::ParseError::Help) => {
            eprintln!("{}", args::usage());
            return ExitCode::SUCCESS;
        }
        Err(args::ParseError::Invalid(msg)) => {
            eprintln!("mesh: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let root = match opts.root {
        Some(path) => path,
        None => match root_dir() {
            Ok(root) => root,
            Err(e) => {
                eprintln!("mesh: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    log::init(&root);
    // 一进来就记下自己是哪一版：两条命令都写，手工 scp、`--file`、systemd 重启
    // 都覆盖得到，客户端下一轮心跳就能拿它判「这份启动器是不是更新的」。
    record_version(&root);

    let outcome: Result<i32, String> = match opts.command {
        args::Command::Launch => launch_installed(&root),
        args::Command::Install {
            source,
            sha256,
            wait_for,
            force,
        } => install::run(&root, source, sha256, wait_for, force).map(|_| 0),
    };

    match outcome {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            log::line(&mesh_text::format(
                mesh_text::id::LAUNCHER_TEXT_FAILED,
                &[&e],
            ));
            eprintln!("mesh: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 编进二进制的启动器版本号（`build/launcher.ps1` 通过 `MESH_VERSION` 交给 cargo）。
const BUILD_VERSION: &str = env!("MESH_LAUNCHER_BUILD_VERSION");

/// record_version 把「盘上这份启动器是哪一版」写进安装根的 launcher.json。
///
/// 写不进去只记日志：它只影响下一次替换的判重，不该拦住启动。
fn record_version(root: &Path) {
    // 开发构建里编进来的是 crate 版本（`0.1.0`），不是发布版本号。把它写进
    // launcher.json 会让客户端把之后每一份正式启动器都判成「不比它新」，从此再也
    // 换不动启动器——所以认不出的版本号干脆不写，客户端按「没有记录」放行。
    if !mesh_install::version::valid(BUILD_VERSION) {
        log::line(&mesh_text::format(
            mesh_text::id::LAUNCHER_TEXT_BUILD_VERSION_INVALID,
            &[&BUILD_VERSION],
        ));
        return;
    }
    if let Err(e) = mesh_install::layout::write_launcher_version(root, BUILD_VERSION) {
        log::line(&mesh_text::format(
            mesh_text::id::LAUNCHER_TEXT_VERSION_RECORD_FAILED,
            &[&mesh_install::layout::LAUNCHER_VERSION, &e],
        ));
    }
}

/// root_dir 就是启动器所在目录：bin/、packages/、config/、log/ 都在它下面。
fn root_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| mesh_text::format(mesh_text::id::LAUNCHER_ERROR_EXE_PATH, &[&e]))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| mesh_text::get(mesh_text::id::LAUNCHER_ERROR_INSTALL_DIR).to_string())
}

/// launch_installed 在 Windows 上拉起客户端就放手：客户端是独立进程，
/// 启动器确认它起来之后退出。
#[cfg(windows)]
fn launch_installed(root: &Path) -> Result<i32, String> {
    // 上次安装没走完会留下 bin.old，先回到那一套再启动。
    install::recover(root)?;
    // 拉起客户端就等于执行 `bin/` 里的代码，先确认它和清单对得上。
    install::verify_current(root)?;
    let exe = root.join("bin").join(launch::CLIENT_EXE);
    if !exe.is_file() {
        return Err(mesh_text::format(
            mesh_text::id::LAUNCHER_ERROR_CLIENT_MISSING,
            &[&exe.display()],
        ));
    }
    log::line(mesh_text::get(mesh_text::id::LAUNCHER_TEXT_START_CLIENT));
    launch::start(&exe, root, None)?;
    Ok(0)
}

/// Linux 上启动器是常驻的：它守着客户端，客户端退出后把退出码透传给 systemd。
#[cfg(unix)]
fn launch_installed(root: &Path) -> Result<i32, String> {
    supervisor::run(root)
}
