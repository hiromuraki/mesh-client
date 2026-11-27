use crate::install::{self, Owner};
use crate::log;
use mesh_install::layout;
use mesh_install::version;
use mesh_text::id;
use std::fs;
use std::path::Path;

/// run 是 Linux 的常驻启动器：拉起客户端、等启动确认、守着它，客户端要更新时
/// 由它来装。
///
/// 与 Windows 的差别只有一处：Windows 的启动器把客户端拉起来、确认启动后就退出，
/// 客户端从此独立；Linux 的启动器必须一直活着，因为它是 systemd 的 ExecStart，
/// 它一退 systemd 就认为服务结束，会在安装途中再拉起一个来抢同一个虚拟 IP。
///
/// 安装根的写权只覆盖「改安装根」的那一段：自愈、装包、拉起并确认。等客户端的那
/// 整段时间放锁——客户端提交更新包与替换启动器时也要拿同一把锁，常驻启动器若一直
/// 握着，Linux 上的自动更新就永远提交不了。冲突不靠这把锁预防：客户端在跑时手工
/// `mesh --file` 会被 `client_running` 判据拒掉（除非带 `--wait-for`）。
pub fn run(root: &Path) -> Result<i32, String> {
    let loop_body = |root: &Path| -> Result<Option<i32>, String> {
        // 每一轮都先自愈：上次没走完的安装在这里退回去，提交之后没删干净的
        // 残留也在这里清掉。放在循环里而不是循环前，装完一轮之后的残留同样管得到。
        let (mut client, version) = {
            let owner = install::own(root)?;
            install::clean_stale_markers(root);
            owner.recover()?;
            // 有待装的包就装（update.tar.zst 存在就是信号）。
            apply_pending(&owner);

            let bin = root.join(layout::BIN);
            let version = install::installed_version(&bin).ok_or_else(|| {
                mesh_text::format(
                    id::LAUNCHER_ERROR_NO_VERSION_JSON_SUPERVISOR,
                    &[&bin.display()],
                )
            })?;
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_STARTING, &[&version]));

            // 这一轮启动是不是在确认一次安装：失败时只有这种情况才有东西可退。
            let booting_new = owner.in_flight()?;
            let client = match owner.boot_and_settle() {
                Ok(client) => client,
                Err(reason) => {
                    log::line(&mesh_text::format(
                        id::LAUNCHER_TEXT_START_FAILED,
                        &[&version, &reason],
                    ));
                    // 装上的新版本没起来：已经回滚，接着起旧的那一份。
                    if booting_new {
                        return Ok(None);
                    }
                    // 普通启动失败保留已安装的文件，按实际错误退出。
                    return Err(reason);
                }
            };
            (client, version)
        };
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_BOOT_CONFIRMED,
            &[&version],
        ));

        let code = client.wait()?;
        Ok(Some(code))
    };

    loop {
        match loop_body(root)? {
            Some(0) if root.join(install::UPDATE_FILE).is_file() => {
                log::line(mesh_text::get(id::LAUNCHER_TEXT_CLIENT_LEFT_UPDATE));
            }
            Some(0) => return Ok(0),
            Some(code) => return Ok(code),
            None => continue,
        }
    }
}

/// apply_pending 处理安装根下的 update.tar.zst。
///
/// 任何一步不成立都只是“丢掉这个包并继续启动当前版本”：文件是唯一信号，
/// 丢了就丢了，客户端下一轮心跳会重新下。
fn apply_pending(owner: &Owner) {
    let root = owner.root();
    let pkg = root.join(install::UPDATE_FILE);
    if !pkg.is_file() {
        return;
    }

    let current = install::installed_version(&root.join(layout::BIN));
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
            remove_package(&pkg);
            return;
        }
        Err(e) => {
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_UPDATE_UNUSABLE, &[&e]));
            remove_package(&pkg);
            return;
        }
    }

    if let Err(e) = owner.install(&pkg, false) {
        // 失败已经在 install 里记进 Failures，客户端下轮会看到。
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_INSTALL_FAILED_KEEP_RUNNING,
            &[&e],
        ));
    }
    // 不管装没装上，这个包都不再有用：失败时 bin 已经回到旧的那一套。
    remove_package(&pkg);
}

/// remove_package 删掉不再有用的安装包。删不掉只记日志：文件还在时下一轮会把它
/// 当成一个“不比当前新”的包再丢一次，不会重新装一遍。
fn remove_package(pkg: &Path) {
    if let Err(e) = fs::remove_file(pkg) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::line(&mesh_text::format(
                id::LAUNCHER_TEXT_CLEANUP_FAILED,
                &[&pkg.display(), &e],
            ));
        }
    }
}
