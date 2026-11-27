use crate::args::Source;
use crate::launch;
use crate::log;
use mesh_install::failures::Failures;
use mesh_install::layout;
use mesh_install::manifest::Manifest;
use mesh_install::platform;
use mesh_install::signing;
use mesh_install::version;
use mesh_text::id;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// BIN_OLD、UPDATE_FILE 与 INSTALL_STATE 的形状归 mesh-install::layout 管：客户端也要用
// 同一套判据（下包前看失败计数、退出前留 update.tar.zst），各写一份迟早分叉。
//
// BIN_OLD 是上一个版本解包后的样子，只在一次安装的窗口里存在：装之前把 bin
// 改名到这里，启动确认成功后删掉，失败就改回来。**它存在不等于「上次安装没走完」**：
// 「这次安装走到哪一步了」由 INSTALL_STATE 表达，理由见 `Tx` 的注释。
pub(crate) use mesh_install::layout::{BIN_OLD, INSTALL_STATE, UPDATE_FILE};
/// WAIT_CLIENT 是“等旧客户端退出”的上限，与客户端调用时的预期一致。
const WAIT_CLIENT: Duration = Duration::from_secs(60);
/// WAIT_BOOT 是“等新版确认启动”的上限。客户端在引擎与面板起来后立刻写标记，
/// 不等网络，所以正常情况下是秒级。
pub(crate) const WAIT_BOOT: Duration = Duration::from_secs(60);

/// run 是手动安装入口（--file / --url）。
///
/// Windows 上装完接着拉起新版本（客户端已经退出，没人启动了）；Linux 上装完就
/// 返回，由 systemd 拉起常驻启动器，否则手动装的这一份会和它抢同一个虚拟 IP。
pub fn run(
    root: &Path,
    source: Source,
    want_sha: Option<String>,
    wait_for: Option<u32>,
    force: bool,
) -> Result<(), String> {
    // 上次安装没走完（bin.old 还在）就先回滚。手动安装与启动路径必须同一个前提：
    // 不能拿唯一的回滚材料去换一份还没验过的新包。
    recover(root)?;
    if let Some(pid) = wait_for {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_WAIT_PROCESS, &[&pid]));
        launch::wait_exit(pid, WAIT_CLIENT)?;
    }

    let pkg = match source {
        Source::File(p) => p,
        Source::Url(url) => {
            let dest = root.join(UPDATE_FILE);
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_DOWNLOAD, &[&url]));
            download(&url, &dest)?;
            dest
        }
    };

    // 这一条只防“下载完成到安装之间”包被换掉，与解包结果无关。
    let sum = sha256_file(&pkg)?;
    // 只有自己下载到 <root>/update.tar.zst 的那份才删：--file 给的是用户的文件，
    // 同样的路径成功安装时反而保留，两种结果不该由失败与否决定。
    let owned = pkg == root.join(UPDATE_FILE);
    if let Some(want) = want_sha {
        if !sum.eq_ignore_ascii_case(&want) {
            if owned {
                let _ = fs::remove_file(&pkg);
            }
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_DIGEST_MISMATCH,
                &[&sum, &want],
            ));
        }
    }
    log::line(&mesh_text::format(
        id::LAUNCHER_TEXT_PACKAGE_DIGEST,
        &[&sum],
    ));

    if let Err(e) = install_from_file(root, &pkg, force) {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_INSTALL_FAILED, &[&e]));
        if owned {
            let _ = fs::remove_file(&pkg);
        }
        // 安装失败时 bin/ 已经被换回原来那一套。Windows 上客户端早已退出，
        // 必须把它拉回来——否则节点就此静默离线，直到有人去双击 UAC。
        #[cfg(windows)]
        {
            reload_current(root).map_err(|reload| {
                mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_RELOAD, &[&e, &reload])
            })?;
            log::line(mesh_text::get(id::LAUNCHER_TEXT_RELOADED));
            return Ok(());
        }
        #[cfg(unix)]
        return Err(e);
    }

    #[cfg(windows)]
    {
        launch_and_check(root)?;
    }
    #[cfg(unix)]
    {
        log::line(mesh_text::get(id::LAUNCHER_TEXT_INSTALLED_FOR_SYSTEMD));
    }
    Ok(())
}

/// 一次安装的事务状态，记在 `<root>/install.state` 里。
///
/// 没有这个文件就等于「没有在途的安装」：`bin/` 是已经确认能启动的那一份。
///
/// ```text
/// 无 ──开始装──▶ fresh | replaced ──解包校验通过──▶ installed ──启动确认──▶ committed ──清理完──▶ 无
/// ```
///
/// 为什么不用「`bin.old` 存在」当判据：那个判据要同时回答两件事——「上次安装没走完」
/// 与「旧版本还没删掉」。startup 确认成功之后删备份失败，会让下一次启动把刚跑起来
/// 的新版本当成没装完退回去。状态写进文件，提交与清理就是两件可以各自失败的事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tx {
    /// 安装前没有 `bin/`：现在的 `bin/` 是本次解出来的，退回时整个删掉。
    Fresh,
    /// 安装前有 `bin/`，它已经改名到 `bin.old`。
    Replaced,
    /// 解包与校验都过了，`bin/` 是完整的新版本，等启动确认。
    Installed,
    /// 新版本确认启动过了：`bin/` 不再回滚，只剩清理。
    Committed,
}

impl Tx {
    fn text(self) -> &'static str {
        match self {
            Tx::Fresh => "fresh",
            Tx::Replaced => "replaced",
            Tx::Installed => "installed",
            Tx::Committed => "committed",
        }
    }

    fn parse(text: &str) -> Option<Tx> {
        [Tx::Fresh, Tx::Replaced, Tx::Installed, Tx::Committed]
            .into_iter()
            .find(|tx| tx.text() == text)
    }
}

fn tx_path(root: &Path) -> PathBuf {
    root.join(INSTALL_STATE)
}

/// 读事务状态。文件不存在是「没有在途安装」；内容不认识是错误——安装根被人动过，
/// 猜一个状态只会拿错的前提去删文件。
fn read_tx(root: &Path) -> Result<Option<Tx>, String> {
    let path = tx_path(root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_INSTALL_STATE_IO,
                &[&path.display(), &e],
            ))
        }
    };
    let text = text.trim();
    Tx::parse(text).map(Some).ok_or_else(|| {
        mesh_text::format(
            id::LAUNCHER_ERROR_INSTALL_STATE_INVALID,
            &[&path.display(), &text],
        )
    })
}

fn write_tx(root: &Path, tx: Tx) -> Result<(), String> {
    let path = tx_path(root);
    mesh_install::atomic_replace(&path, tx.text().as_bytes(), 0o644)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_INSTALL_STATE_IO, &[&path.display(), &e]))
}

fn clear_tx(root: &Path) -> Result<(), String> {
    let path = tx_path(root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(mesh_text::format(
            id::LAUNCHER_ERROR_INSTALL_STATE_IO,
            &[&path.display(), &e],
        )),
    }
}

/// in_flight 说明 `bin/` 是不是一次装着、还没确认启动的新版本。
#[cfg(unix)]
pub(crate) fn in_flight(root: &Path) -> Result<bool, String> {
    Ok(matches!(read_tx(root)?, Some(Tx::Installed)))
}

/// recover 在启动任何东西之前调用：把没走完的安装退回安装前的样子。
///
/// 每一步失败都保留状态文件，下一次启动接着做。
pub(crate) fn recover(root: &Path) -> Result<(), String> {
    match read_tx(root)? {
        // 没有在途安装：bin/ 就是已确认的那一份，只剩可能没删干净的备份。
        None => {
            clear_backup(root);
            Ok(())
        }
        Some(Tx::Committed) => {
            clear_backup(root);
            clear_tx(root)
        }
        // 装好了、等启动确认：这一轮的启动就是那次确认，bin/ 必须留着。
        Some(Tx::Installed) => Ok(()),
        Some(tx) => {
            log::line(mesh_text::get(id::LAUNCHER_TEXT_RECOVER_STALE));
            undo(root, tx)?;
            clear_tx(root)
        }
    }
}

/// abort 放弃一次还没确认启动的安装：按事务状态退回安装前的样子。
///
/// 与 recover 的差别只有一处：`installed` 也退——调用方已经知道这次启动失败了，
/// 或者正要拿一份新包替换它。
pub(crate) fn abort(root: &Path) -> Result<(), String> {
    match read_tx(root)? {
        None | Some(Tx::Committed) => Ok(()),
        Some(tx) => {
            undo(root, tx)?;
            clear_tx(root)
        }
    }
}

/// undo 把 `bin/` 退回安装前的样子，只做这次事务拥有的那部分改动。
///
/// 幂等：已经退过的那一步（`bin/` 删掉了、改名做完了）再来一次是空操作，
/// 所以「退到一半失败、下次启动接着退」不会把旧版本删掉。
fn undo(root: &Path, tx: Tx) -> Result<(), String> {
    let bin = root.join("bin");
    let old = root.join(BIN_OLD);
    match tx {
        // 已经确认启动：bin/ 不回滚。
        Tx::Committed => Ok(()),
        // 还没走到改名那一步：bin/ 还是原来那一份，什么都不用退。
        Tx::Replaced if !old.exists() => Ok(()),
        // 有备份就换回来；首次安装没有备份，删掉本次的产物就是「没装过」。
        _ if old.exists() => {
            remove_dir(&bin)?;
            fs::rename(&old, &bin)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_ROLLBACK, &[&e]))
        }
        _ => remove_dir(&bin),
    }
}

fn remove_dir(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(mesh_text::format(
            id::LAUNCHER_ERROR_REMOVE_DIR,
            &[&path.display(), &e],
        )),
    }
}

/// clear_backup 删掉上一版本与安装包。
///
/// 失败只记日志：这两样都是残留，下次启动会再试一次；它们是否还在不再影响
/// `bin/` 的判定——那由事务状态决定。
fn clear_backup(root: &Path) {
    for target in [root.join(BIN_OLD), root.join(UPDATE_FILE)] {
        let outcome = if target.is_dir() {
            fs::remove_dir_all(&target)
        } else {
            fs::remove_file(&target)
        };
        if let Err(e) = outcome {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::line(&mesh_text::format(
                    id::LAUNCHER_TEXT_CLEANUP_FAILED,
                    &[&target.display(), &e],
                ));
            }
        }
    }
}

/// install_from_file 把包装进 bin/：先把现有 bin 改名到 bin.old，再解包校验。
///
/// 失败时把 bin 退回去——调用方看到的永远是「旧的」或「新的」，不会是一半。
/// 成功时事务仍未提交：bin.old 留着，等启动确认之后再 [`commit`]。
pub(crate) fn install_from_file(root: &Path, pkg: &Path, force: bool) -> Result<(), String> {
    install_signed_by(root, pkg, force, signing::RELEASE_PUBLIC_KEY)
}

/// 用给定公钥装包。轮换公钥与测试走这一条，签名以外的行为与 [`install_from_file`] 相同。
pub(crate) fn install_signed_by(
    root: &Path,
    pkg: &Path,
    force: bool,
    public_hex: &str,
) -> Result<(), String> {
    // 上一份还没确认启动的安装先退回去：它的备份要拿来当这一次的回滚材料。
    abort(root)?;
    let bin = root.join(layout::BIN);
    let old_version = installed_version(&bin);

    let manifest = read_manifest(pkg)?;
    // 签名与本机适用性放在动 bin/ 之前：坏包不该换掉唯一的回滚材料，也不该走到
    // 「装完才发现起不来」那一步。降级检查与失败计数都在后面，因为它们只读、
    // 给出的错误更具体。
    check_manifest(&manifest, public_hex)?;
    log::line(&mesh_text::format(
        id::LAUNCHER_TEXT_INSTALLING,
        &[&manifest.version(), &manifest.files().len()],
    ));

    // 降级防护：服务端把发布记录指回旧包时，客户端会当成“有新版本”下载下来。
    // 2026-11-26 两个种子就是这么被降级到 -3 的。 --force 留给手动修机器。
    if !force {
        if let Some(old) = old_version.as_deref() {
            if !version::is_newer(manifest.version(), old) {
                return Err(mesh_text::format(
                    id::LAUNCHER_ERROR_NOT_NEWER,
                    &[&manifest.version(), &old],
                ));
            }
        }
        let failed = Failures::load(root);
        if failed.exhausted(manifest.version()) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_FAILURES_EXHAUSTED,
                &[&manifest.version(), &failed.count(manifest.version())],
            ));
        }
    }

    let tx = if bin.exists() {
        Tx::Replaced
    } else {
        Tx::Fresh
    };
    // 状态先落盘：从这里开始断电或被杀，下一次启动知道该退什么。
    write_tx(root, tx)?;
    let outcome =
        swap_and_unpack(root, pkg, &manifest, tx).and_then(|()| write_tx(root, Tx::Installed));
    if let Err(e) = outcome {
        // 装不上就是一次失败：不记下来的话客户端每轮都会重新下一遍、再试一遍。
        Failures::load(root).record(manifest.version());
        return Err(match undo(root, tx).and_then(|()| clear_tx(root)) {
            Ok(()) => e,
            Err(r) => mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_ROLLBACK, &[&e, &r]),
        });
    }
    log::line(mesh_text::get(id::LAUNCHER_TEXT_VERIFIED));
    Ok(())
}

/// swap_and_unpack 是一次安装的实际动作：改掉旧 bin、解包、按最终落盘的目录校验。
fn swap_and_unpack(root: &Path, pkg: &Path, manifest: &Manifest, tx: Tx) -> Result<(), String> {
    let bin = root.join(layout::BIN);
    if tx == Tx::Replaced {
        fs::rename(&bin, root.join(BIN_OLD))
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_MOVE_OLD_BIN, &[&e]))?;
    }
    fs::create_dir_all(&bin)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CREATE_BIN, &[&e]))?;
    unpack(pkg, &bin)?;
    verify(&bin, manifest)
}

/// commit 在启动确认成功之后调用：先把「已提交」落到盘上，再清理旧版本与安装包。
///
/// 顺序有意义：写下 committed 之后 `bin/` 就不再回滚，后面清理失败只影响残留，
/// 不会把已经跑起来的新版本重新解释成「没装完」。
///
/// 没有在途安装时（普通启动确认成功）不写状态，只清理：状态文件描述的是**一次安装**，
/// 不能因为一次普通的启动就凭空出现。
pub(crate) fn commit(root: &Path) -> Result<(), String> {
    if read_tx(root)?.is_none() {
        clear_backup(root);
        return Ok(());
    }
    write_tx(root, Tx::Committed)?;
    clear_backup(root);
    clear_tx(root)
}

/// launch_and_check 拉起 bin/ 里的版本并等它确认启动，失败就回到 bin.old。
/// 只有 Windows 用：Linux 的启动器是常驻的，等确认与回退都在 supervisor 里。
#[cfg(windows)]
fn launch_and_check(root: &Path) -> Result<(), String> {
    let bin = root.join(layout::BIN);
    let version = installed_version(&bin).unwrap_or_default();
    // 拉起之前按最终落盘的目录验一遍：与日常启动走同一道闸。
    verify_current(root)?;
    let token = token();
    launch::start(&bin.join(launch::CLIENT_EXE), root, Some(&token))?;
    log::line(mesh_text::get(id::LAUNCHER_TEXT_STARTED_NEW));

    if wait_boot_ok(root, &token, WAIT_BOOT) {
        log::line(mesh_text::get(id::LAUNCHER_TEXT_BOOT_OK));
        Failures::load(root).clear(&version);
        return commit(root)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_COMMIT_FAILED, &[&e]));
    }

    log::line(mesh_text::get(id::LAUNCHER_TEXT_BOOT_FAILED_ROLLBACK));
    Failures::load(root).record(&version);
    abort(root)?;
    reload_current(root).map_err(|r| mesh_text::format(id::LAUNCHER_ERROR_RELOAD_OLD, &[&r]))
}

/// reload_current 拉起 bin/ 里现有的版本（不等启动确认）。
///
/// 恢复分支里的东西同样是「马上就要执行的代码」，所以和正常入口走同一道检查：
/// 回滚材料本身就坏了就停下来等人修，而不是照旧拉起。
#[cfg(windows)]
fn reload_current(root: &Path) -> Result<(), String> {
    verify_current(root)?;
    let exe = root.join("bin").join(launch::CLIENT_EXE);
    if !exe.is_file() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_EXE_MISSING,
            &[&exe.display()],
        ));
    }
    launch::start(&exe, root, None)?;
    Ok(())
}

/// installed_version 读当前 bin/version.json 的版本号；还没有安装过就是 None。
///
/// 这里只取版本号、不校验字符集：值不合法时按“没有已安装版本”处理，
/// 与原来 serde 直接解析的行为一致（字符集校验在 layout::read_version 里，
/// 那是给“当前版本”这个判据用的）。
pub(crate) fn installed_version(bin: &Path) -> Option<String> {
    let text = fs::read_to_string(bin.join(layout::MANIFEST)).ok()?;
    Manifest::parse(&text)
        .ok()
        .map(|manifest| manifest.version().to_string())
}

/// package_version 只读包里的版本号，给常驻启动器做“这包还该不该装”的预检查用。
#[cfg(unix)]
pub(crate) fn package_version(pkg: &Path) -> Result<String, String> {
    Ok(read_manifest(pkg)?.version().to_string())
}

fn read_manifest(pkg: &Path) -> Result<Manifest, String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE, &[&e]))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE_ENTRY, &[&e]))?;
        let path = entry
            .path()
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_ENTRY_NAME, &[&e]))?;
        if path.as_os_str() == "version.json" {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_VERSION_JSON, &[&e]))?;
            return Manifest::parse(&text)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_PARSE_VERSION_JSON, &[&e]));
        }
    }
    Err(mesh_text::get(id::LAUNCHER_ERROR_NO_VERSION_JSON).to_string())
}

fn unpack(pkg: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_UNPACK, &[&dest.display(), &e]))
}

/// verify_current 在拉起客户端之前校验 `bin/` 与它自带的清单一致。
///
/// 两道闸，顺序有意义：先验清单的签名（谁能改 `bin/` 里的文件，却改不出发布私钥
/// 的签名），再逐文件比对摘要（解包只写了一半、磁盘坏了一块、有人换了二进制却
/// 没换清单，都在这里被拦住）。11 MiB 的包实测约 30 ms，与「加载一个 DLL」同一个量级。
pub fn verify_current(root: &Path) -> Result<(), String> {
    verify_current_signed_by(root, signing::RELEASE_PUBLIC_KEY)
}

/// 用给定公钥验当前 `bin/`。轮换公钥与测试走这一条。
pub fn verify_current_signed_by(root: &Path, public_hex: &str) -> Result<(), String> {
    let manifest = mesh_install::manifest::read(&layout::manifest_path(root))
        .map_err(|err| err.to_string())?;
    check_manifest(&manifest, public_hex)?;
    verify(&root.join(layout::BIN), &manifest)
}

/// 清单既要是发布方签的，也要是给这台机器的。
///
/// 签名证明内容出自发布私钥，不证明它是**这一份**：别的平台、别的槽位的清单同样
/// 能验签通过，所以槽位与平台由验签方对着自己的环境判断，清单自己说了不算。
/// 装包、日常启动、手动装后的拉起都走这一条：三处各自组合检查项，迟早有一处漏。
fn check_manifest(manifest: &Manifest, public_hex: &str) -> Result<(), String> {
    if !manifest.verify_with(public_hex) {
        return Err(mesh_text::get(id::LAUNCHER_ERROR_MANIFEST_SIGNATURE_INVALID).to_string());
    }
    if manifest.slot() != signing::SLOT_CLIENT || manifest.platform() != platform::id() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_MANIFEST_IDENTITY,
            &[
                &manifest.slot(),
                &manifest.platform(),
                &signing::SLOT_CLIENT,
                &platform::id(),
            ],
        ));
    }
    Ok(())
}

/// verify 要求 `bin/` 与清单是**同一个集合**：少了、多了、对不上都判包不可用。
///
/// 只核对清单列出的文件不够：签名授权的是一个文件集合，不是一份白名单。目录里
/// 多出来的文件同样会被加载（Windows 的 DLL 搜索、Linux 的 `LD_LIBRARY_PATH` 相邻
/// 目录），所以多一个也是「安装根被人动过」。`version.json` 是唯一例外——它自己
/// 写不进自己的摘要，靠 `signature` 保护。
fn verify(bin: &Path, manifest: &Manifest) -> Result<(), String> {
    // 主程序与内核必须出现在清单里：一份「格式合法、内容齐全」却不含这两者的
    // 清单，装上去的结果就是起不来。
    for required in [platform::client_exe(), platform::core_name()] {
        if !manifest.files().contains_key(required) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_MISSING_FILE,
                &[&required],
            ));
        }
    }

    // 先取实际目录：文件名 → 是不是普通文件。遍历里的错误不能吞掉——读不动的
    // 目录不等于「与清单一致」。
    let mut present: BTreeMap<String, bool> = BTreeMap::new();
    let entries = fs::read_dir(bin)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_DIR, &[&bin.display(), &e]))?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            mesh_text::format(id::LAUNCHER_ERROR_READ_DIR_ENTRY, &[&bin.display(), &e])
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry
            .file_type()
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_ENTRY_TYPE, &[&name, &e]))?;
        present.insert(name, kind.is_file());
    }

    // 集合必须精确相等：签名授权的是一个文件集合，不是一份白名单。清单之外的文件
    // 同样会被加载（Windows 的 DLL 搜索、Linux 的相邻目录），多一个也算「安装根被
    // 人动过」，少一个就是装残了。`version.json` 是唯一例外——它写不进自己的摘要，
    // 靠 `signature` 保护，但类型要跟其余文件一样是普通文件。
    for (name, regular) in &present {
        if name == layout::MANIFEST {
            if !regular {
                return Err(mesh_text::format(
                    id::LAUNCHER_ERROR_UNEXPECTED_FILE,
                    &[&name],
                ));
            }
            continue;
        }
        if !regular || !manifest.files().contains_key(name) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_UNEXPECTED_FILE,
                &[&name],
            ));
        }
    }

    // 集合对上了才读摘要：读的是清单允许、且确实落在目录里的那几个文件。
    for (name, want) in manifest.files() {
        if !present.get(name).copied().unwrap_or(false) {
            return Err(mesh_text::format(id::LAUNCHER_ERROR_MISSING_FILE, &[&name]));
        }
        let got = sha256_file(&bin.join(name))?;
        if !got.eq_ignore_ascii_case(want) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_FILE_DIGEST_MISMATCH,
                &[&name],
            ));
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&path.display(), &e]))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_FILE, &[&path.display(), &e]))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// boot_marker 是客户端确认启动后写的文件。token 由启动器生成，保证“上一次启动
/// 留下的标记”不会被这一次误认。
pub(crate) fn boot_marker(root: &Path, token: &str) -> PathBuf {
    root.join(format!(".boot-ok-{token}"))
}

#[cfg(windows)]
fn wait_boot_ok(root: &Path, token: &str, timeout: Duration) -> bool {
    let marker = boot_marker(root, token);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if marker.is_file() {
            let _ = fs::remove_file(&marker);
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

pub(crate) fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos & 0xffff_ffff)
}

/// download 只在 --url 时用；Linux 版走 system curl：静态 musl 下把 openssl
/// 链进二进制不值得，而 Linux 上 curl 是标配。
#[cfg(windows)]
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_DOWNLOAD, &[&e]))?;
    let mut reader = response.into_reader();
    let mut file = File::create(dest)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CREATE_FILE, &[&dest.display(), &e]))?;
    std::io::copy(&mut reader, &mut file)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_WRITE_FILE, &[&dest.display(), &e]))?;
    Ok(())
}

#[cfg(unix)]
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let status = std::process::Command::new("curl")
        .args(["-fL", "--connect-timeout", "20", "--retry", "2"])
        .arg("-o")
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CALL_CURL, &[&e]))?;
    if !status.success() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_DOWNLOAD_CURL,
            &[&status],
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mesh-launcher-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个 tar.zst 包：根上是清单，`entries` 是包里的文件。
    fn package(dir: &Path, manifest: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("pkg.tar.zst");
        let file = fs::File::create(&path).unwrap();
        let encoder = zstd::Encoder::new(file, 3).unwrap();
        let mut builder = tar::Builder::new(encoder);
        append(&mut builder, "version.json", manifest.as_bytes());
        for (name, body) in entries {
            append(&mut builder, name, body);
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        path
    }

    fn append<W: std::io::Write>(builder: &mut tar::Builder<W>, name: &str, body: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, body).unwrap();
    }

    /// 清单声明的摘要与实际内容不符：解包成功、校验失败。
    fn broken_package(dir: &Path) -> PathBuf {
        package(
            dir,
            r#"{"version":"2099.01.01-1","files":{"mesh-client.exe":"00"}}"#,
            &[("mesh-client.exe", b"not the bytes the manifest claims")],
        )
    }

    fn test_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[5u8; 32])
    }

    fn public_hex(key: &ed25519_dalek::SigningKey) -> String {
        hex(&key.verifying_key().to_bytes())
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex(&hasher.finalize())
    }

    /// 造一份签名正确的清单：摘要按文件内容算，签的是清单摘要。
    fn signed_manifest(
        key: &ed25519_dalek::SigningKey,
        version: &str,
        entries: &[(&str, &[u8])],
    ) -> String {
        signed_manifest_for(platform::id(), key, version, entries)
    }

    /// 同 [`signed_manifest`]，但清单声明的平台由调用方给：用来单独改这一个条件。
    fn signed_manifest_for(
        manifest_platform: &str,
        key: &ed25519_dalek::SigningKey,
        version: &str,
        entries: &[(&str, &[u8])],
    ) -> String {
        use ed25519_dalek::Signer;
        let files: BTreeMap<String, String> = entries
            .iter()
            .map(|(name, body)| (name.to_string(), sha256_bytes(body)))
            .collect();
        let message = signing::manifest_string(
            signing::SLOT_CLIENT,
            manifest_platform,
            version,
            &signing::files_digest(&files),
        );
        let signature = hex(&key.sign(message.as_bytes()).to_bytes());
        let pairs = files
            .iter()
            .map(|(name, sha)| format!("{name:?}:{sha:?}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"version":"{version}","slot":"client","platform":"{manifest_platform}","files":{{{pairs}}},"signature":"{signature}"}}"#
        )
    }

    /// 安装失败不能删用户给的文件：--file 收的是用户自己的包，客户端交接时那个
    /// 路径（<root>/update.tar.zst）才是下载下来的、可以删的那一份。
    #[test]
    fn a_tampered_bin_is_refused_before_launch() {
        let root = temp_root("verify-current");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        let entries: [(&str, &[u8]); 2] = [
            (platform::client_exe(), b"client"),
            (platform::core_name(), b"core"),
        ];
        for (name, body) in entries {
            std::fs::write(bin.join(name), body).unwrap();
        }
        let key = test_key();
        std::fs::write(
            layout::manifest_path(&root),
            signed_manifest(&key, "2026.11.27-1", &entries),
        )
        .unwrap();
        let public = public_hex(&key);

        assert!(
            verify_current_signed_by(&root, &public).is_ok(),
            "一份完整且签过名的 bin 应当通过"
        );
        // 换了二进制却没换清单：这正是「有人动过安装根」的形态。
        std::fs::write(bin.join(platform::client_exe()), b"evil").unwrap();
        assert!(
            verify_current_signed_by(&root, &public).is_err(),
            "换掉 bin 里的文件就必须拒绝启动"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单之外的文件同样要被拒：签名授权的是一个文件集合，不是一份白名单。
    #[test]
    fn a_file_outside_the_manifest_is_refused() {
        let root = temp_root("verify-extra");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        let entries: [(&str, &[u8]); 2] = [
            (platform::client_exe(), b"client"),
            (platform::core_name(), b"core"),
        ];
        for (name, body) in entries {
            std::fs::write(bin.join(name), body).unwrap();
        }
        let key = test_key();
        std::fs::write(
            layout::manifest_path(&root),
            signed_manifest(&key, "2026.11.27-1", &entries),
        )
        .unwrap();
        let public = public_hex(&key);
        assert!(verify_current_signed_by(&root, &public).is_ok());

        std::fs::write(bin.join("extra.dll"), b"planted").unwrap();
        assert!(
            verify_current_signed_by(&root, &public).is_err(),
            "清单之外的文件必须被拒绝"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单合法但不含内核：装上去也起不来，所以验签阶段就判它不可用。
    #[test]
    fn a_manifest_without_the_core_is_refused() {
        let root = temp_root("verify-no-core");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(platform::client_exe()), b"client").unwrap();
        let key = test_key();
        std::fs::write(
            layout::manifest_path(&root),
            signed_manifest(&key, "2026.11.27-1", &[(platform::client_exe(), b"client")]),
        )
        .unwrap();
        assert!(verify_current_signed_by(&root, &public_hex(&key)).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单与 bin/ 完全一致，但签名不是这个公钥签的：文件摘要那一关会全过，
    /// 只有验签能拦住——这正是加签名的理由。
    #[test]
    fn a_manifest_signed_by_another_key_is_refused() {
        let root = temp_root("verify-other-key");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mesh-client.exe"), b"client").unwrap();
        let key = test_key();
        std::fs::write(
            layout::manifest_path(&root),
            signed_manifest(&key, "2026.11.27-1", &[("mesh-client.exe", b"client")]),
        )
        .unwrap();

        let stranger = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        assert!(
            verify_current_signed_by(&root, &public_hex(&stranger)).is_err(),
            "别的密钥签出来的清单不能过"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 造一个合法包：清单按内容算摘要、用测试密钥签名。
    fn valid_package(dir: &Path, version: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let key = test_key();
        package(dir, &signed_manifest(&key, version, entries), entries)
    }

    fn required_entries() -> [(&'static str, &'static [u8]); 2] {
        [
            (platform::client_exe(), b"client"),
            (platform::core_name(), b"core"),
        ]
    }

    fn assert_no_install_in_flight(root: &Path) {
        assert!(
            read_tx(root).unwrap().is_none(),
            "不该留下事务状态：{}",
            tx_path(root).display()
        );
        assert!(!root.join(BIN_OLD).exists(), "不该留下 bin.old");
    }

    /// 只有「缺签名」这一个条件不同，其余字段全部合法：断言才能说明是签名拦下的。
    #[test]
    fn a_package_without_a_signature_is_refused_before_touching_bin() {
        let root = temp_root("unsigned-pkg");
        let src = temp_root("unsigned-pkg-src");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("KEEP"), b"keep").unwrap();
        let entries = required_entries();
        let manifest = format!(
            r#"{{"version":"2026.11.27-1","slot":"client","platform":"{}","files":{{"{}":"{}","{}":"{}"}}}}"#,
            platform::id(),
            entries[0].0,
            sha256_bytes(entries[0].1),
            entries[1].0,
            sha256_bytes(entries[1].1)
        );
        let pkg = package(&src, &manifest, &entries);

        assert!(
            install_from_file(&root, &pkg, true).is_err(),
            "缺签名必须拒绝"
        );
        assert!(bin.join("KEEP").is_file(), "验签失败不该动 bin/");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn a_bin_without_a_manifest_is_refused() {
        let root = temp_root("verify-current-missing");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        assert!(verify_current(&root).is_err(), "没有清单就不能启动");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_manual_install_keeps_the_package_the_user_gave() {
        let root = temp_root("keep-package");
        let src = temp_root("keep-package-src");
        let pkg = broken_package(&src);

        assert!(
            run(&root, Source::File(pkg.clone()), None, None, true).is_err(),
            "坏包应当安装失败"
        );
        assert!(pkg.is_file(), "用户给的包被删掉了: {}", pkg.display());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 手动安装前先把没确认的安装退回去：那份 bin.old 是回滚材料，
    /// 不能拿一份坏包把它顶掉。
    #[test]
    fn a_manual_install_rolls_back_the_unconfirmed_install_first() {
        let root = temp_root("stale-bin-old");
        let src = temp_root("stale-bin-old-src");
        fs::create_dir_all(root.join(BIN_OLD)).unwrap();
        fs::write(root.join(BIN_OLD).join("GOOD"), b"good").unwrap();
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        fs::write(root.join(layout::BIN).join("BROKEN"), b"broken").unwrap();
        write_tx(&root, Tx::Installed).unwrap();
        let pkg = broken_package(&src);

        let _ = run(&root, Source::File(pkg), None, None, true);

        assert!(
            root.join(layout::BIN).join("GOOD").is_file(),
            "没确认的安装没有先退回：回滚材料被坏包顶掉了"
        );
        assert!(!root.join(BIN_OLD).exists(), "退回之后不该再有 bin.old");
        assert_no_install_in_flight(&root);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 清单是别的平台的：签名完全正确，只有 slot/platform 这一项不合本机。
    #[test]
    fn a_package_for_another_platform_is_refused_before_touching_bin() {
        let root = temp_root("other-platform");
        let src = temp_root("other-platform-src");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let entries = required_entries();
        let other = if platform::id() == "linux/amd64" {
            "windows/amd64"
        } else {
            "linux/amd64"
        };
        let manifest = signed_manifest_for(other, &key, "2026.11.27-1", &entries);
        let pkg = package(&src, &manifest, &entries);

        assert!(
            install_signed_by(&root, &pkg, true, &public_hex(&key)).is_err(),
            "别的平台的包必须拒绝"
        );
        assert!(bin.join("KEEP").is_file(), "本机不适用时不该动 bin/");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 装完不等于装上：事务停在 installed，回滚材料（bin.old）还在，等启动确认。
    #[test]
    fn an_install_is_not_confirmed_until_commit() {
        let root = temp_root("tx-confirm");
        let src = temp_root("tx-confirm-src");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let entries = required_entries();
        let pkg = valid_package(&src, "2026.11.27-2", &entries);

        install_signed_by(&root, &pkg, true, &public_hex(&key)).unwrap();

        assert_eq!(read_tx(&root).unwrap(), Some(Tx::Installed));
        assert!(
            root.join(BIN_OLD).join("KEEP").is_file(),
            "确认之前 bin.old 必须留着，它是唯一的回滚材料"
        );
        assert!(bin.join(platform::client_exe()).is_file());

        commit(&root).unwrap();

        assert_no_install_in_flight(&root);
        assert!(
            bin.join(platform::client_exe()).is_file(),
            "提交之后新版本还在"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 放弃一次没确认的安装：回到安装前那一份，状态与备份都不留。
    #[test]
    fn aborting_an_unconfirmed_install_restores_the_backup() {
        let root = temp_root("tx-abort");
        let src = temp_root("tx-abort-src");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let entries = required_entries();
        let pkg = valid_package(&src, "2026.11.27-4", &entries);
        install_signed_by(&root, &pkg, true, &public_hex(&key)).unwrap();

        abort(&root).unwrap();

        assert!(bin.join("KEEP").is_file(), "放弃安装必须退回原来那一份");
        assert!(
            !bin.join(platform::client_exe()).exists(),
            "新版本的文件不该留下"
        );
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 解包结果与清单不符：退回原样、记一次失败，状态与备份都不留。
    #[test]
    fn a_failed_install_restores_the_previous_bin() {
        let root = temp_root("tx-failed");
        let src = temp_root("tx-failed-src");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let declared: [(&str, &[u8]); 2] = [
            (platform::client_exe(), b"declared client"),
            (platform::core_name(), b"core"),
        ];
        let actual: [(&str, &[u8]); 2] = [
            (platform::client_exe(), b"tampered client"),
            (platform::core_name(), b"core"),
        ];
        let pkg = package(
            &src,
            &signed_manifest(&key, "2026.11.27-9", &declared),
            &actual,
        );

        assert!(install_signed_by(&root, &pkg, true, &public_hex(&key)).is_err());

        assert!(
            bin.join("KEEP").is_file(),
            "失败的安装必须把原来的 bin 退回来"
        );
        assert_no_install_in_flight(&root);
        assert_eq!(Failures::load(&root).count("2026.11.27-9"), 1);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 没有在途安装时 `bin/` 是已确认能启动的那一份：普通启动失败不能删它。
    #[test]
    fn recover_leaves_an_installed_version_alone() {
        let root = temp_root("recover-idle");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mesh-client.exe"), b"client").unwrap();

        recover(&root).unwrap();

        assert!(
            bin.join("mesh-client.exe").is_file(),
            "没有在途安装时不该动 bin/"
        );
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_rolls_back_an_install_that_never_confirmed() {
        let root = temp_root("recover-replaced");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("HALF"), b"half").unwrap();
        fs::create_dir_all(root.join(BIN_OLD)).unwrap();
        fs::write(root.join(BIN_OLD).join("GOOD"), b"good").unwrap();
        write_tx(&root, Tx::Replaced).unwrap();

        recover(&root).unwrap();

        assert!(bin.join("GOOD").is_file());
        assert!(!bin.join("HALF").exists());
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_cleans_a_first_install_that_never_confirmed() {
        let root = temp_root("recover-fresh");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("HALF"), b"half").unwrap();
        write_tx(&root, Tx::Fresh).unwrap();

        recover(&root).unwrap();

        assert!(!bin.exists(), "首次安装失败要把本次的产物删掉");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
    }

    /// 提交之后只剩清理：再起一次不能把已经跑起来的新版本退回去。
    #[test]
    fn recover_keeps_the_new_version_once_it_is_committed() {
        let root = temp_root("recover-committed");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("NEW"), b"new").unwrap();
        std::fs::create_dir_all(root.join(BIN_OLD)).unwrap();
        fs::write(root.join(BIN_OLD).join("OLD"), b"old").unwrap();
        write_tx(&root, Tx::Committed).unwrap();

        recover(&root).unwrap();

        assert!(bin.join("NEW").is_file(), "已提交的版本不能回滚");
        assert!(!root.join(BIN_OLD).exists(), "清理残留的备份");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
    }

    /// 没有事务状态时 bin.old 只是上次没删干净的残留。
    #[test]
    fn a_stray_backup_is_discarded_when_no_install_is_in_flight() {
        let root = temp_root("recover-stray");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("NEW"), b"new").unwrap();
        std::fs::create_dir_all(root.join(BIN_OLD)).unwrap();

        recover(&root).unwrap();

        assert!(bin.join("NEW").is_file());
        assert!(!root.join(BIN_OLD).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_install_state_refuses_to_guess() {
        let root = temp_root("recover-garbage");
        fs::write(tx_path(&root), b"half-way").unwrap();

        assert!(
            recover(&root).is_err(),
            "认不出的状态不能猜：猜错就会拿错的前提删文件"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 普通启动确认成功（没有在途安装）不能凭空写出事务状态。
    #[test]
    fn commit_without_an_install_leaves_no_state_behind() {
        let root = temp_root("commit-idle");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join(BIN_OLD)).unwrap();

        commit(&root).unwrap();

        assert!(
            read_tx(&root).unwrap().is_none(),
            "没有在途安装时不该写状态：{}",
            tx_path(&root).display()
        );
        assert!(!root.join(BIN_OLD).exists(), "残留的备份照旧清掉");
        assert!(bin.exists(), "普通启动确认不能动 bin/");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 清单文件本身也在集合里：它免摘要，但类型仍要是普通文件。
    #[test]
    fn a_manifest_that_is_a_directory_is_refused() {
        let root = temp_root("verify-manifest-dir");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(bin.join(layout::MANIFEST)).unwrap();
        let entries = required_entries();
        for (name, body) in entries {
            fs::write(bin.join(name), body).unwrap();
        }
        let key = test_key();
        let manifest = Manifest::parse(&signed_manifest(&key, "2026.11.27-1", &entries)).unwrap();

        assert!(
            verify(&bin, &manifest).is_err(),
            "version.json 是目录时必须拒绝启动"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
