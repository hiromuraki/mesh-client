use crate::install;
use crate::launch;
use crate::log;
use mesh_install::layout;
use mesh_text::id;
use std::path::Path;

/// run 是 Linux 的常驻启动器：拉起客户端、等启动确认、守着它；客户端要更新时
/// 由它来装。
///
/// 与 Windows 的差别只有进程怎么托管：Windows 的启动器把客户端拉起来、确认启动后
/// 就退出，客户端从此独立；Linux 的启动器必须一直活着，因为它是 systemd 的
/// ExecStart，它一退 systemd 就认为服务结束。
///
/// 安装根的写权只覆盖「改安装根」的那一段：清理、迁移、修复 current、装包、拉起并
/// 确认。等客户端的那整段时间放锁——客户端提交更新包与替换启动器时也要拿同一把锁，
/// 常驻启动器若一直握着，Linux 上的自动更新就永远提交不了。冲突不靠这把锁预防：
/// 客户端在跑时手工 `mesh --file` 会被 `client_running` 判据拒掉（除非带
/// `--wait-for`）。
pub fn run(root: &Path) -> Result<i32, String> {
    loop {
        match round(root)? {
            Round::Running { mut client } => match client.wait()? {
                // 客户端正常退出并留下了「请安装」信号：回到循环开头装它。
                0 if root.join(layout::UPDATE_FILE).is_file() => {
                    log::line(mesh_text::get(id::LAUNCHER_TEXT_CLIENT_LEFT_UPDATE));
                }
                code => return Ok(code),
            },
            // 新版本没确认启动：current 没动，回到循环开头按选槽判据起当前槽。
            Round::Retry => {}
        }
    }
}

/// 一轮的结局：要么客户端已经起来并确认（交给调用方守着），要么这一轮该重来。
enum Round {
    Running { client: launch::Client },
    Retry,
}

fn round(root: &Path) -> Result<Round, String> {
    // 整轮在所有权之内：选槽、装包、拉起并确认；确认之后放手再等客户端。
    let owner = install::own(root)?;
    let selected = owner.prepare()?;
    // 待装包是唯一的安装信号：文件名固定，内容已经由客户端核过摘要。
    let installed = if root.join(layout::UPDATE_FILE).is_file() {
        owner.apply_pending(selected.as_ref())
    } else {
        None
    };

    let client = match installed {
        // 刚装进候选槽：拉起它，等它提交 current。
        Some(installed) => {
            log::line(&mesh_text::format(
                id::LAUNCHER_TEXT_STARTING,
                &[&installed.version],
            ));
            match owner.boot_candidate(&installed) {
                Ok(client) => client,
                Err(e) => {
                    log::line(&mesh_text::format(
                        id::LAUNCHER_TEXT_START_FAILED,
                        &[&installed.version, &e],
                    ));
                    // 还有一个有效的当前槽：下一轮回到选槽，起旧的那一份。
                    if selected.is_some() {
                        return Ok(Round::Retry);
                    }
                    return Err(e);
                }
            }
        }
        None => {
            let Some(current) = selected else {
                return Err(install::no_slot_error());
            };
            log::line(&mesh_text::format(
                id::LAUNCHER_TEXT_STARTING,
                &[&current.version],
            ));
            match owner.boot_current(&current) {
                Ok(client) => client,
                Err(e) => {
                    // 普通启动失败不删任何文件，按实际错误退出，交给 systemd 的重启策略。
                    log::line(&mesh_text::format(
                        id::LAUNCHER_TEXT_START_FAILED,
                        &[&current.version, &e],
                    ));
                    return Err(e);
                }
            }
        }
    };
    Ok(Round::Running { client })
}
