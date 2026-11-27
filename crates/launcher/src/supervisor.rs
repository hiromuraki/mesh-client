use crate::install;
use crate::launch;
use crate::log;
use mesh_install::failures::Failures;
use mesh_install::version;
use mesh_text::id;
use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

/// run 是 Linux 的常驻启动器：拉起客户端、等启动确认、守着它，客户端要更新时
/// 由它来装。
///
/// 与 Windows 的差别只有一处：Windows 的启动器把客户端拉起来、确认启动后就退出，
/// 客户端从此独立；Linux 的启动器必须一直活着，因为它是 systemd 的 ExecStart，
/// 它一退 systemd 就认为服务结束，会在安装途中再拉起一个来抢同一个虚拟 IP。
pub fn run(root: &Path) -> Result<i32, String> {
    clean_stale_markers(root);
    // 启动前先自愈：上次没走完的安装会留下 bin.old。
    install::recover(root)?;
    let bin = root.join("bin");

    loop {
        // 有待装的包就装（update.tar.zst 存在就是信号）。
        apply_pending(root);
        // 拉开客户端之前确认 `bin/` 与清单一致：启动器是这台机器上以 root 身份、
        // 在客户端之前运行的唯一东西，它不查就没有人查了。
        install::verify_current(root)?;

        let version = install::installed_version(&bin).ok_or_else(|| {
            mesh_text::format(
                id::LAUNCHER_ERROR_NO_VERSION_JSON_SUPERVISOR,
                &[&bin.display()],
            )
        })?;
        let exe = bin.join(launch::CLIENT_EXE);
        if !exe.is_file() {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_EXE_MISSING,
                &[&exe.display()],
            ));
        }

        let token = install::token();
        let mut child = launch::start(&exe, root, Some(&token))?
            .ok_or_else(|| mesh_text::get(id::LAUNCHER_ERROR_NO_CHILD).to_string())?;
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_STARTING, &[&version]));

        if let Err(reason) = wait_boot(&mut child, root, &token) {
            log::line(&mesh_text::format(
                id::LAUNCHER_TEXT_START_FAILED,
                &[&version, &reason],
            ));
            stop_child(&mut child);
            Failures::load(root).record(&version);
            // 回到 bin.old：这一版起不来就当没装过。
            install::restore(root);
            continue;
        }

        Failures::load(root).clear(&version);
        // 验过了，旧的那一份和更新包都可以扔了。
        install::commit(root);
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_BOOT_CONFIRMED,
            &[&version],
        ));

        let status = child
            .wait()
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT, &[&e]))?;
        let code = status.code().unwrap_or(1);
        // 以 0 退出不等于“服务该停了”：它可能刚下好一个更新包。
        if code != 0 {
            return Ok(code);
        }
        if !root.join(install::UPDATE_FILE).is_file() {
            return Ok(0);
        }
        log::line(mesh_text::get(id::LAUNCHER_TEXT_CLIENT_LEFT_UPDATE));
    }
}

/// apply_pending 处理安装根下的 update.tar.zst。
///
/// 任何一步不成立都只是“丢掉这个包并继续启动当前版本”：文件是唯一信号，
/// 丢了就丢了，客户端下一轮心跳会重新下。
fn apply_pending(root: &Path) {
    let pkg = root.join(install::UPDATE_FILE);
    if !pkg.is_file() {
        return;
    }

    let current = install::installed_version(&root.join("bin"));
    match install::package_version(&pkg) {
        Ok(v)
            if current
                .as_deref()
                .map(|c| version::is_newer(&v, c))
                .unwrap_or(true) =>
        {
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_INSTALL_UPDATE, &[&v]));
        }
        Ok(v) => {
            // 残留的包（上次没清掉、或装完没来得及删）不该再拉起一次安装。
            log::line(&mesh_text::format(
                id::LAUNCHER_TEXT_DISCARD_NOT_NEWER,
                &[&v],
            ));
            let _ = fs::remove_file(&pkg);
            return;
        }
        Err(e) => {
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_UPDATE_UNUSABLE, &[&e]));
            let _ = fs::remove_file(&pkg);
            return;
        }
    }

    if let Err(e) = install::install_from_file(root, &pkg, false) {
        // 失败已经在 install_from_file 里记进 Failures，客户端下轮会看到。
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_INSTALL_FAILED_KEEP_RUNNING,
            &[&e],
        ));
    }
    // 不管装没装上，这个包都不再有用：失败时 bin 已经回到旧的那一套。
    let _ = fs::remove_file(&pkg);
}

/// wait_boot 等客户端写启动标记。子进程先退出就不必等满 60 秒了——
/// 那是“装上就崩”，越早知道越好。
fn wait_boot(child: &mut Child, root: &Path, token: &str) -> Result<(), String> {
    let marker = install::boot_marker(root, token);
    let deadline = Instant::now() + install::WAIT_BOOT;
    while Instant::now() < deadline {
        if marker.is_file() {
            let _ = fs::remove_file(&marker);
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(mesh_text::format(
                    id::LAUNCHER_ERROR_CLIENT_EXITED_IMMEDIATELY,
                    &[&status],
                ));
            }
            Ok(None) => {}
            Err(e) => {
                return Err(mesh_text::format(id::LAUNCHER_ERROR_WAIT_CLIENT, &[&e]));
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(mesh_text::format(
        id::LAUNCHER_ERROR_BOOT_TIMEOUT,
        &[&install::WAIT_BOOT.as_secs()],
    ))
}

/// stop_child 先礼后兵。必须是 SIGTERM：客户端把 core 放进了独立会话，只有它自己
/// 的 Shutdown 才会停 core；SIGKILL 留下的孤儿 core 会和回退后新起的那个抢同一个
/// TUN 和虚拟 IP，等于没回退。
fn stop_child(child: &mut Child) {
    let pid = child.id();
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return,
        }
    }
    log::line(mesh_text::get(id::LAUNCHER_TEXT_KILL_CLIENT));
    let _ = child.kill();
}

/// clean_stale_markers 删掉上一次启动留下的确认标记。token 每次都不一样，
/// 残留的标记永远不会被认领，留着只会让安装目录里堆满小文件。
fn clean_stale_markers(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".boot-ok-") {
            let _ = fs::remove_file(entry.path());
        }
    }
}
