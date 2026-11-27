use crate::args::Source;
use crate::launch;
use crate::log;
use mesh_install::failures::Failures;
use mesh_install::layout;
use mesh_install::lock::{acquire as acquire_lock, Lock};
use mesh_install::manifest::{self, Manifest};
use mesh_install::platform;
use mesh_install::signing;
use mesh_install::version;
use mesh_text::id;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// BIN_OLD、UPDATE_FILE 与 INSTALL_STATE 的形状归 mesh-install::layout 管：客户端也要用
// 同一套判据（下包前看失败计数、退出前留 update.tar.zst），各写一份迟早分叉。
//
// BIN_OLD 是上一个版本解包后的样子，只在一次安装的窗口里存在：装之前把 bin
// 改名到这里，启动确认成功后删掉，失败就改回来。**它存在不等于「上次安装没走完」**：
// 「这次安装走到哪一步了」由 INSTALL_STATE 表达，理由见 `Tx` 的注释。
pub(crate) use mesh_install::layout::{BIN_OLD, BIN_STALE, INSTALL_STATE, UPDATE_FILE};
/// WAIT_CLIENT 是“等旧客户端退出”的上限，与客户端调用时的预期一致。
const WAIT_CLIENT: Duration = Duration::from_secs(60);
/// WAIT_BOOT 是“等新版确认启动”的上限。客户端在引擎与面板起来后立刻写标记，
/// 不等网络，所以正常情况下是秒级。
pub(crate) const WAIT_BOOT: Duration = Duration::from_secs(60);
/// 包本身的字节上限。与控制面发布时的 200MB 对齐：那边收不下更大的，这边也不装。
const PACKAGE_BYTES: u64 = 200 << 20;
/// --url 下载的总时限。与客户端下包那条路的上限同量级，只是各自判一次的代价。
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(300);
/// 启动确认标记的前缀。token 由启动器生成，保证“上一次启动留下的标记”不会被这一次误认。
const BOOT_MARKER: &str = ".boot-ok-";

/// 一次安装对工件的预算：条目数、清单字节、解压后字节、总时限。
///
/// 包与清单都是外部字节，没有上限就等于把安装根的存亡交给它。四个数字集中在这里，
/// 读清单、解包、下载共用一套：分成两套，迟早有一处漏。
#[derive(Clone, Copy)]
struct Budget {
    /// 包里的条目数。正常客户端包十几个文件。
    entries: usize,
    /// 包根清单（version.json）的字节数：正常是一小段 JSON。
    manifest: u64,
    /// 解包后的总字节数。正常包 11–20 MiB。
    unpacked: u64,
    /// 读清单与解包的截止时刻。
    deadline: Instant,
}

impl Budget {
    /// 一个包允许的最大开销。留了两个数量级余量，同时把「把安装根写满」与
    /// 「解一辈子」挡在外面。
    fn package() -> Self {
        Self {
            entries: 1024,
            manifest: 64 << 10,
            unpacked: 256 << 20,
            deadline: Instant::now() + Duration::from_secs(120),
        }
    }
}

/// 安装根的所有权：`install.state`、`bin/`、`bin.old/`、待装包的写都在它之内。
///
/// 拿不到它，这里一行都不写。Windows 上它随一次 `--file` / 启动命令存在；Linux 上
/// 常驻启动器从进程开始握到结束——「谁在装」因此只有一个答案，第二个启动器当场
/// 失败，而不是并行改目录。
///
/// 进程退出（含被杀）时内核会释放文件锁，不会留下需要人工删的东西。
pub(crate) struct Owner {
    root: PathBuf,
    _lock: Lock,
}

/// own 取得安装根的写权。已有写者时返回错误，不排队：
///
/// * 写者可能是另一个启动器（正在装或正在确认），
/// * 也可能是客户端（正在提交更新包或替换启动器）。
///
/// 冲突不靠它预防：客户端在跑时手工安装由 `client_running` 判断并拒绝。
pub(crate) fn own(root: &Path) -> Result<Owner, String> {
    // 文案与客户端的单实例锁分开：占着安装根的可能是另一个启动器，那时「已有客户端
    // 实例在运行」是错的（客户端可能早就退出了）。非争用错误照原样上抛。
    let lock = match acquire_lock(&Lock::owner_path(root)) {
        Ok(lock) => lock,
        Err(mesh_install::Error::AlreadyRunning) => {
            return Err(mesh_text::get(id::LAUNCHER_ERROR_INSTALL_ROOT_BUSY).to_string())
        }
        Err(err) => return Err(err.to_string()),
    };
    Ok(Owner {
        root: root.to_path_buf(),
        _lock: lock,
    })
}

impl Owner {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// 读事务状态。文件不存在是「没有在途安装」；内容不认识是错误——安装根被人
    /// 动过，猜一个状态只会拿错的前提去删文件。
    fn read_tx(&self) -> Result<Option<Tx>, String> {
        read_tx(&self.root)
    }

    fn write_tx(&self, tx: Tx) -> Result<(), String> {
        write_tx(&self.root, tx)
    }

    fn clear_tx(&self) -> Result<(), String> {
        clear_tx(&self.root)
    }

    /// 这一轮的 `bin/` 是不是一次装着、还没确认启动的新版本。
    pub(crate) fn in_flight(&self) -> Result<bool, String> {
        Ok(matches!(self.read_tx()?, Some(Tx::Installed)))
    }

    /// recover 在启动任何东西之前调用：把没走完的安装退回安装前的样子。
    ///
    /// 每一步失败都保留状态文件，下一次启动接着做。
    pub(crate) fn recover(&self) -> Result<(), String> {
        match self.read_tx()? {
            // 没有在途安装：bin/ 就是已确认的那一份，只剩可能没删干净的残留。
            None => {
                self.clear_backup();
                Ok(())
            }
            Some(Tx::Committed) => {
                self.clear_backup();
                self.clear_tx()
            }
            // 装好了、等启动确认：这一轮的启动就是那次确认，bin/ 必须留着。
            Some(Tx::Installed) => Ok(()),
            Some(tx) => {
                log::line(mesh_text::get(id::LAUNCHER_TEXT_RECOVER_STALE));
                self.undo(tx)?;
                self.clear_tx()
            }
        }
    }

    /// abort 放弃一次还没确认启动的安装：按事务状态退回安装前的样子。
    ///
    /// 与 recover 的差别只有一处：`installed` 也退——调用方已经知道这次启动失败了，
    /// 或者正要拿一份新包替换它。没有在途安装时它是空操作，只清掉安装产物残留。
    pub(crate) fn abort(&self) -> Result<(), String> {
        match self.read_tx()? {
            None | Some(Tx::Committed) => {
                self.clear_backup();
                Ok(())
            }
            Some(tx) => {
                self.undo(tx)?;
                self.clear_tx()
            }
        }
    }

    /// commit 在启动确认成功之后调用：先把「已提交」落到盘上，再清理旧版本与安装包。
    ///
    /// 顺序有意义：写下 committed 之后 `bin/` 就不再回滚，后面清理失败只影响残留，
    /// 不会把已经跑起来的新版本重新解释成「没装完」。
    ///
    /// 没有在途安装时（普通启动确认成功）只清安装产物残留：状态文件描述的是**一次
    /// 安装**，不能因为一次普通的启动就凭空出现；待装包更不是残留，它是「请安装」
    /// 的信号，只由消耗它的安装事务删掉。
    pub(crate) fn commit(&self) -> Result<(), String> {
        if self.read_tx()?.is_none() {
            self.clear_backup();
            return Ok(());
        }
        self.write_tx(Tx::Committed)?;
        self.clear_backup();
        self.clear_package();
        self.clear_tx()
    }

    /// install 把包装进 bin/：先把现有 bin 改名到 bin.old，再解包校验。
    ///
    /// 失败时把 bin 退回去——调用方看到的永远是「旧的」或「新的」，不会是一半。
    /// 成功时事务仍未提交：bin.old 留着，等启动确认之后再 [`Owner::commit`]。
    pub(crate) fn install(&self, pkg: &Path, force: bool) -> Result<(), String> {
        self.install_signed_by(pkg, force, signing::RELEASE_PUBLIC_KEY)
    }

    /// 用给定公钥装包。轮换公钥与测试走这一条，签名以外的行为与 [`Owner::install`] 相同。
    pub(crate) fn install_signed_by(
        &self,
        pkg: &Path,
        force: bool,
        public_hex: &str,
    ) -> Result<(), String> {
        // 上一份还没确认启动的安装先退回去：它的备份要拿来当这一次的回滚材料。
        self.abort()?;
        let bin = self.root.join(layout::BIN);
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
            let failed = Failures::load(&self.root);
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
        // 状态先落盘：从这里开始进程被杀，下一次启动知道该退什么。
        self.write_tx(tx)?;
        let outcome = self
            .swap_and_unpack(pkg, &manifest, tx)
            .and_then(|()| self.write_tx(Tx::Installed));
        if let Err(e) = outcome {
            // 装不上就是一次失败：不记下来的话客户端每轮都会重新下一遍、再试一遍。
            record_failure(&self.root, manifest.version());
            return Err(match self.undo(tx).and_then(|()| self.clear_tx()) {
                Ok(()) => e,
                Err(r) => mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_ROLLBACK, &[&e, &r]),
            });
        }
        log::line(mesh_text::get(id::LAUNCHER_TEXT_VERIFIED));
        Ok(())
    }

    /// swap_and_unpack 是一次安装的实际动作：改掉旧 bin、解包、按最终落盘的目录校验。
    fn swap_and_unpack(&self, pkg: &Path, manifest: &Manifest, tx: Tx) -> Result<(), String> {
        let bin = self.root.join(layout::BIN);
        if tx == Tx::Replaced {
            fs::rename(&bin, self.root.join(BIN_OLD))
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_MOVE_OLD_BIN, &[&e]))?;
        }
        fs::create_dir_all(&bin)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CREATE_BIN, &[&e]))?;
        unpack(pkg, &bin, Budget::package())?;
        verify(&bin, manifest)
    }

    /// undo 把 `bin/` 退回安装前的样子，只做这次事务拥有的那部分改动。
    ///
    /// 每一步都可以重复，也不存在「旧版本已经被删掉」的中间态：换回旧版本时先把它
    /// 改名成 `bin/`，再删被丢下的新版本；删除失败就留一份 `bin.stale/` 给下一次。
    fn undo(&self, tx: Tx) -> Result<(), String> {
        let bin = self.root.join(layout::BIN);
        let old = self.root.join(BIN_OLD);
        if tx == Tx::Committed {
            return Ok(());
        }
        let stale = self.root.join(BIN_STALE);
        if !old.exists() {
            // 没有备份时只有两种可能，靠状态区分：还没走到改名那一步（bin/ 是原来
            // 那一份，什么都不用退），或者本身就是首次安装（bin/ 是本次的产物）。
            // 前者只剩 bin.stale/ 的收尾——那是上一次换回时没能删掉的残骸。
            return match tx {
                Tx::Replaced | Tx::Restoring => remove_dir(&stale),
                _ => remove_dir(&bin),
            };
        }
        // 有备份：从这里开始 bin.old/ 是唯一的旧版本。先把「正在换回」记进状态，
        // 之后任何一次恢复都按「换回来」解释——绝不会因为 bin.old/ 已经不在了，
        // 就把刚恢复好的 bin/ 当成本次的产物删掉。
        if tx != Tx::Restoring {
            self.write_tx(Tx::Restoring)?;
        }
        remove_dir(&stale)?;
        if bin.exists() {
            fs::rename(&bin, &stale)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_ROLLBACK, &[&e]))?;
        }
        fs::rename(&old, &bin)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_ROLLBACK, &[&e]))?;
        remove_dir(&stale)
    }

    /// clear_backup 删掉安装产物：上一版本的备份与回滚留下的残骸。
    ///
    /// 只删这两个：它们是安装的产物，没有在途安装时就是残留。安装包不是——那是
    /// 「请安装」的信号，客户端下完就退出，启动器的下一轮才去装它；在这里顺手删掉，
    /// Linux 上的自动更新就永远装不上了。
    ///
    /// 失败只记日志：残留下次启动再试，它是否还在不再影响 `bin/` 的判定。
    fn clear_backup(&self) {
        remove_path(&self.root.join(BIN_OLD));
        remove_path(&self.root.join(BIN_STALE));
    }

    /// 删掉已经处理过的安装包（装上或明确丢弃）。失败只记日志：文件还留着的话，
    /// 下一轮会把它当成一个「不比当前新」的包再丢一次，不会重新装一遍。
    fn clear_package(&self) {
        remove_path(&self.root.join(UPDATE_FILE));
    }

    /// verify_current 在拉起客户端之前校验 `bin/` 与它自带的清单一致。
    pub(crate) fn verify_current(&self) -> Result<(), String> {
        self.verify_current_signed_by(signing::RELEASE_PUBLIC_KEY)
    }

    /// 用给定公钥验当前 `bin/`。轮换公钥与测试走这一条。
    pub(crate) fn verify_current_signed_by(&self, public_hex: &str) -> Result<(), String> {
        let manifest =
            manifest::read(&layout::manifest_path(&self.root)).map_err(|err| err.to_string())?;
        check_manifest(&manifest, public_hex)?;
        verify(&self.root.join(layout::BIN), &manifest)
    }

    /// boot_and_settle 把 `bin/` 里的这一份拉到确定的结局。
    ///
    /// 四个入口（安装后、普通启动、确认被打断后的下一次启动、常驻监督）都走这一条：
    /// 验证 → 拉起 → 等确认 → 提交或停下来回滚。两个平台的差别只有进程怎么托管，
    /// 判据与顺序一模一样。
    ///
    /// 返回的进程句柄由调用方决定拿去等（Linux 常驻监督）还是放手（Windows 装完就退）。
    pub(crate) fn boot_and_settle(&self) -> Result<launch::Client, String> {
        let bin = self.root.join(layout::BIN);
        let version = installed_version(&bin).unwrap_or_default();
        // 1. 验：拉起 bin/ 等于执行里面的代码，验不过就没有「试一次」这回事。
        if let Err(e) = self.verify_current() {
            record_failure(&self.root, &version);
            // 没有在途安装时是空操作：普通启动失败不删任何文件。
            self.abort()?;
            return Err(e);
        }
        // 2. 起
        let exe = client_exe(&self.root)?;
        let token = token();
        let mut client = match launch::start(&exe, &self.root, Some(&token)) {
            Ok(client) => client,
            Err(e) => {
                record_failure(&self.root, &version);
                self.abort()?;
                return Err(e);
            }
        };
        // 3. 等确认
        if let Err(reason) = wait_boot(&mut client, &self.root, &token) {
            log::line(mesh_text::get(id::LAUNCHER_TEXT_BOOT_FAILED_ROLLBACK));
            // 停不下来就不回滚：新版还占着 TUN 与 bin/ 里的文件，这时候换目录只会更糟。
            // 保留 installed，下一次启动接着确认。
            client.stop()?;
            record_failure(&self.root, &version);
            self.abort()?;
            return Err(reason);
        }
        // 4. 提交。确认已经成功，清理失败只影响残留，不该把跑起来的版本退回去。
        log::line(mesh_text::get(id::LAUNCHER_TEXT_BOOT_OK));
        clear_failure(&self.root, &version);
        if let Err(e) = self.commit() {
            log::line(&mesh_text::format(id::LAUNCHER_ERROR_COMMIT_FAILED, &[&e]));
        }
        Ok(client)
    }
}

/// 一次安装的事务状态，记在 `<root>/install.state` 里。
///
/// 没有这个文件就等于「没有在途的安装」：`bin/` 是已经确认能启动的那一份。
///
/// ```text
/// 无 ─开始装→ fresh | replaced ─解包校验通过→ installed ─启动确认→ committed ─清理完→ 无
///                      └────── 回滚开始（有备份）→ restoring ──目录换回──→ 无
/// ```
///
/// 为什么不用「`bin.old` 存在」当判据：那个判据要同时回答两件事——「上次安装没走完」
/// 与「旧版本还没删掉」。启动确认成功之后删备份失败，会让下一次启动把刚跑起来的
/// 新版本当成没装完退回去。状态写进文件，提交与清理就是两件可以各自失败的事。
///
/// `restoring` 专给「换回旧版本」这一段：它先于目录改动落盘，因此「`bin.old/` 不在」
/// 只可能是换回已经做完，而不是「首次安装没有备份」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tx {
    /// 安装前没有 `bin/`：现在的 `bin/` 是本次解出来的，退回时整个删掉。
    Fresh,
    /// 安装前有 `bin/`，它已经改名到 `bin.old`。
    Replaced,
    /// 解包与校验都过了，`bin/` 是完整的新版本，等启动确认。
    Installed,
    /// 正在把 `bin.old/` 换回 `bin/`：旧版本是唯一的启动材料，只能恢复，不能再删。
    Restoring,
    /// 新版本确认启动过了：`bin/` 不再回滚，只剩清理。
    Committed,
}

impl Tx {
    fn text(self) -> &'static str {
        match self {
            Tx::Fresh => "fresh",
            Tx::Replaced => "replaced",
            Tx::Installed => "installed",
            Tx::Restoring => "restoring",
            Tx::Committed => "committed",
        }
    }

    fn parse(text: &str) -> Option<Tx> {
        [
            Tx::Fresh,
            Tx::Replaced,
            Tx::Installed,
            Tx::Restoring,
            Tx::Committed,
        ]
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
    // 整个事务在所有权之内：从恢复、换目录到启动确认，中间不放锁。
    let owner = own(root)?;
    // 上一份没走完的安装先退回去。手动安装与启动路径必须同一个前提：
    // 不能拿唯一的回滚材料去换一份还没验过的新包。
    owner.recover()?;
    clean_stale_markers(root);
    if let Some(pid) = wait_for {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_WAIT_PROCESS, &[&pid]));
        launch::wait_exit(pid, WAIT_CLIENT)?;
    } else if client_running(root) {
        // 没有 --wait-for 却还有客户端在跑：在它脚下换 bin/，它的下一次文件访问就
        // 落到另一个版本上。直接拒绝，让人先退出客户端。
        return Err(mesh_text::get(id::INSTALL_ERROR_ALREADY_RUNNING).to_string());
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
    let downloaded = pkg == root.join(UPDATE_FILE);
    if let Some(want) = want_sha {
        if !sum.eq_ignore_ascii_case(&want) {
            if downloaded {
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

    if let Err(e) = owner.install(&pkg, force) {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_INSTALL_FAILED, &[&e]));
        if downloaded {
            let _ = fs::remove_file(&pkg);
        }
        // 安装失败时 bin/ 已经被换回原来那一套。Windows 上客户端早已退出，必须把它
        // 拉回来——否则节点就此静默离线，直到有人去双击 UAC。
        #[cfg(windows)]
        {
            start_current(&owner).map_err(|reload| {
                mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_RELOAD, &[&e, &reload])
            })?;
            log::line(mesh_text::get(id::LAUNCHER_TEXT_RELOADED));
        }
        return Err(e);
    }

    #[cfg(windows)]
    {
        // 装完接着拉起新版本并等确认：客户端已经退出，没人启动了。
        boot_or_restore(&owner)?;
    }
    #[cfg(unix)]
    log::line(mesh_text::get(id::LAUNCHER_TEXT_INSTALLED_FOR_SYSTEMD));
    Ok(())
}

/// launch_installed 是 Windows 的普通启动入口。
///
/// 没有在途安装时就是「拉起已确认的那一份、放手」；有在途安装时，这一轮启动就是
/// 那次安装的确认——与安装入口同一个出口。
#[cfg(windows)]
pub(crate) fn launch_installed(root: &Path) -> Result<(), String> {
    let owner = own(root)?;
    clean_stale_markers(root);
    owner.recover()?;
    if owner.in_flight()? {
        if client_running(root) {
            // 新版本已经在跑：上一个启动器在等确认时被中断，或确认超时后它自己活了下来。
            // 不再起第二个（它会撞上单实例锁直接退出），把这一份记成已启动——
            // 回滚一个正在运行的版本只会更糟。
            owner.commit()?;
            return Ok(());
        }
        return boot_or_restore(&owner);
    }
    start_current(&owner)
}

/// 拉起新版本并等确认；没确认就回滚，再把旧版本拉回来。
///
/// Windows 的启动器装完就退，没有人替它把节点拉回来：回滚之后必须自己重启旧版本。
#[cfg(windows)]
fn boot_or_restore(owner: &Owner) -> Result<(), String> {
    log::line(mesh_text::get(id::LAUNCHER_TEXT_STARTED_NEW));
    match owner.boot_and_settle() {
        // 确认成功，新版本自己跑；启动器放手退出。
        Ok(client) => {
            drop(client);
            Ok(())
        }
        Err(e) => {
            start_current(owner).map_err(|reload| {
                mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_RELOAD, &[&e, &reload])
            })?;
            log::line(mesh_text::get(id::LAUNCHER_TEXT_RELOADED));
            Err(e)
        }
    }
}

/// 拉起 `bin/` 里已经确认过的那一份，不等确认。
#[cfg(windows)]
fn start_current(owner: &Owner) -> Result<(), String> {
    owner.verify_current()?;
    let exe = client_exe(owner.root())?;
    log::line(mesh_text::get(id::LAUNCHER_TEXT_START_CLIENT));
    launch::start(&exe, owner.root(), None).map(|_| ())
}

/// 有没有客户端正占着这个安装根。
///
/// 判据与客户端自己启动时用的是同一个锁文件：拿得到就是没人在跑，拿不到就是有。
/// 拿到之后立刻放开——这里只是看一眼，不是要占住它。
fn client_running(root: &Path) -> bool {
    match acquire_lock(&Lock::path(root)) {
        Ok(lock) => {
            drop(lock);
            false
        }
        Err(_) => true,
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

fn remove_path(target: &Path) {
    let outcome = if target.is_dir() {
        fs::remove_dir_all(target)
    } else {
        fs::remove_file(target)
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

/// 记一次「这个版本没起来」。版本号读不出来（安装根已经坏了）时不记：
/// 一条空键只会让下一次判断更难读。
fn record_failure(root: &Path, version: &str) {
    if !version.is_empty() {
        Failures::load(root).record(version);
    }
}

/// 某个版本确认启动成功后调用，把这个版本的记录抹掉。
fn clear_failure(root: &Path, version: &str) {
    if !version.is_empty() {
        Failures::load(root).clear(version);
    }
}
/// 读包根的清单，同时把预算核一遍。
///
/// 清单必须在动 `bin/` 之前读出来，预算也只能在这里判：条目过多、或声明的解压后
/// 总量超限的包，不该走到「旧版本已经被挪走」那一步。
fn read_manifest(pkg: &Path) -> Result<Manifest, String> {
    read_manifest_with(pkg, Budget::package())
}

fn read_manifest_with(pkg: &Path, budget: Budget) -> Result<Manifest, String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE, &[&e]))?;
    let mut count = 0usize;
    let mut declared = 0u64;
    let mut seen = BTreeSet::new();
    let mut manifest: Option<Manifest> = None;
    // 一次读完整个包：预算要在动 bin/ 之前判完，条目名也在这一遍定死，
    // 不给「先解一半再发现不合规」留空间。
    for entry in entries {
        let entry =
            entry.map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE_ENTRY, &[&e]))?;
        count += 1;
        if count > budget.entries {
            return Err(package_error(
                pkg,
                &format!("条目超过上限 {}", budget.entries),
            ));
        }
        if Instant::now() > budget.deadline {
            return Err(package_error(pkg, "读清单超过时限"));
        }
        declared = declared.saturating_add(entry.size());
        if declared > budget.unpacked {
            return Err(package_error(
                pkg,
                &format!("声明的解压后字节数超过上限 {}", budget.unpacked),
            ));
        }
        let is_manifest = {
            let path = entry
                .path()
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_ENTRY_NAME, &[&e]))?;
            let Some(name) = path
                .to_str()
                .filter(|name| manifest::is_plain_file_name(name))
                .map(str::to_string)
            else {
                return Err(package_error(pkg, &format!("条目 {path:?} 不是普通文件名")));
            };
            if !seen.insert(name.clone()) {
                return Err(package_error(pkg, &format!("条目 {name} 出现两次")));
            }
            path.as_os_str() == layout::MANIFEST
        };
        if is_manifest && manifest.is_none() {
            manifest = Some(read_manifest_entry(pkg, entry, budget.manifest)?);
        }
    }
    manifest.ok_or_else(|| mesh_text::get(id::LAUNCHER_ERROR_NO_VERSION_JSON).to_string())
}

/// 读清单本身，字节数有上限：清单是一小段 JSON，一个 64 KiB 的 version.json
/// 已经说明这份包不是出包脚本产出的。
fn read_manifest_entry<R: Read>(
    pkg: &Path,
    entry: tar::Entry<'_, R>,
    max: u64,
) -> Result<Manifest, String> {
    let mut text = String::new();
    entry
        .take(max + 1)
        .read_to_string(&mut text)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_VERSION_JSON, &[&e]))?;
    if text.len() as u64 > max {
        return Err(package_error(pkg, &format!("清单超过上限 {max} 字节")));
    }
    Manifest::parse(&text)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_PARSE_VERSION_JSON, &[&e]))
}

/// 解包到 `dest`，按预算逐条写。
///
/// 只接受单层普通文件名：清单的合法域就只允许这个形状，解包这一侧不按另一套规则
/// 收——路径穿越、符号链接、目录都由这一条判据挡掉。
fn unpack(pkg: &Path, dest: &Path, budget: Budget) -> Result<(), String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE, &[&e]))?;
    let mut count = 0usize;
    let mut written = 0u64;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE_ENTRY, &[&e]))?;
        count += 1;
        if count > budget.entries {
            return Err(unpack_error(
                dest,
                &format!("条目超过上限 {}", budget.entries),
            ));
        }
        if Instant::now() > budget.deadline {
            return Err(unpack_error(dest, "解包超过时限"));
        }
        let path = entry
            .path()
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_ENTRY_NAME, &[&e]))?;
        let Some(name) = path
            .to_str()
            .filter(|name| manifest::is_plain_file_name(name))
        else {
            return Err(unpack_error(dest, &format!("条目 {path:?} 不是普通文件名")));
        };
        if !entry.header().entry_type().is_file() {
            return Err(unpack_error(dest, &format!("条目 {name} 不是普通文件")));
        }
        let target = dest.join(name);
        let mut out = File::create(&target).map_err(|e| {
            mesh_text::format(id::LAUNCHER_ERROR_CREATE_FILE, &[&target.display(), &e])
        })?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_PACKAGE, &[&e]))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > budget.unpacked {
                return Err(unpack_error(
                    dest,
                    &format!("解压后超过上限 {} 字节", budget.unpacked),
                ));
            }
            if Instant::now() > budget.deadline {
                return Err(unpack_error(dest, "解包超过时限"));
            }
            out.write_all(&buf[..n]).map_err(|e| {
                mesh_text::format(id::LAUNCHER_ERROR_WRITE_FILE, &[&target.display(), &e])
            })?;
        }
    }
    Ok(())
}

fn package_error(pkg: &Path, reason: &str) -> String {
    mesh_text::format(
        id::LAUNCHER_ERROR_READ_PACKAGE,
        &[&format!("{}（{reason}）", pkg.display())],
    )
}

fn unpack_error(dest: &Path, reason: &str) -> String {
    mesh_text::format(id::LAUNCHER_ERROR_UNPACK, &[&dest.display(), &reason])
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

/// 客户端主程序必须存在：拉起 `bin/` 的那条路走同一个判据。
fn client_exe(root: &Path) -> Result<PathBuf, String> {
    let exe = root.join(layout::BIN).join(launch::CLIENT_EXE);
    if !exe.is_file() {
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_CLIENT_MISSING,
            &[&exe.display()],
        ));
    }
    Ok(exe)
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
fn boot_marker(root: &Path, token: &str) -> PathBuf {
    root.join(format!("{BOOT_MARKER}{token}"))
}

/// 启动确认标记只属于一次尝试：token 每次都不同，旧的永远不会被认领，留着只会
/// 让安装根里堆小文件。任何一次事务入口先清一遍。
pub(crate) fn clean_stale_markers(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(BOOT_MARKER) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// wait_boot 等客户端写启动标记，或者在它提前退出时立刻失败——那是「装上就崩」，
/// 越早知道越好。两个平台同一个判据：进程是不是还活着由句柄回答。
fn wait_boot(client: &mut launch::Client, root: &Path, token: &str) -> Result<(), String> {
    let marker = boot_marker(root, token);
    let deadline = Instant::now() + WAIT_BOOT;
    while Instant::now() < deadline {
        if marker.is_file() {
            let _ = fs::remove_file(&marker);
            return Ok(());
        }
        if let Some(code) = client.exited()? {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_CLIENT_EXITED_IMMEDIATELY,
                &[&code],
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(mesh_text::format(
        id::LAUNCHER_ERROR_BOOT_TIMEOUT,
        &[&WAIT_BOOT.as_secs()],
    ))
}

fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos & 0xffff_ffff)
}

/// 下载先落 `<dest>.part`，成功才一次改名到正式名。
///
/// 正式名本身是「请安装」的信号：写到一半的文件被当成信号，启动器下一轮就会去
/// 解一个半截包。字节数与时限在流里判，不靠 `Content-Length`——那是对方的说法。
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let tmp = part_path(dest);
    let _ = fs::remove_file(&tmp);
    download_into(url, &tmp, Instant::now() + DOWNLOAD_DEADLINE)?;
    fs::rename(&tmp, dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        mesh_text::format(id::LAUNCHER_ERROR_WRITE_FILE, &[&dest.display(), &e])
    })
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// 手动安装的下载。Linux 版走 system curl：静态 musl 下把 openssl 链进二进制
/// 不值得，而 Linux 上 curl 是标配。两条路的预算同一组数字。
#[cfg(windows)]
fn download_into(url: &str, tmp: &Path, deadline: Instant) -> Result<(), String> {
    let response = ureq::get(url)
        .timeout(DOWNLOAD_DEADLINE)
        .call()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_DOWNLOAD, &[&e]))?;
    let mut reader = response.into_reader();
    copy_bounded(&mut reader, tmp, PACKAGE_BYTES, deadline).map(|_| ())
}

#[cfg(unix)]
fn download_into(url: &str, tmp: &Path, _deadline: Instant) -> Result<(), String> {
    let status = std::process::Command::new("curl")
        .args(["-fL", "--connect-timeout", "20", "--retry", "2"])
        .arg("--max-time")
        .arg(DOWNLOAD_DEADLINE.as_secs().to_string())
        .arg("--max-filesize")
        .arg(PACKAGE_BYTES.to_string())
        .arg("-o")
        .arg(tmp)
        .arg(url)
        .status()
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CALL_CURL, &[&e]))?;
    if !status.success() {
        let _ = fs::remove_file(tmp);
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_DOWNLOAD_CURL,
            &[&status],
        ));
    }
    // curl 的 --max-filesize 在部分协议/版本上不生效，尺寸在这里再判一次。
    let size = fs::metadata(tmp)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_WRITE_FILE, &[&tmp.display(), &e]))?
        .len();
    if size > PACKAGE_BYTES {
        let _ = fs::remove_file(tmp);
        return Err(mesh_text::format(
            id::LAUNCHER_ERROR_DOWNLOAD,
            &[&format!("包超过 {PACKAGE_BYTES} 字节")],
        ));
    }
    Ok(())
}

/// 把一个流写进临时文件，超过字节上限或时限就失败，并把半截文件删掉。
#[cfg(any(windows, test))]
fn copy_bounded<R: Read>(
    reader: &mut R,
    tmp: &Path,
    max: u64,
    deadline: Instant,
) -> Result<u64, String> {
    let outcome = copy_bounded_inner(reader, tmp, max, deadline);
    if outcome.is_err() {
        let _ = fs::remove_file(tmp);
    }
    outcome
}

#[cfg(any(windows, test))]
fn copy_bounded_inner<R: Read>(
    reader: &mut R,
    tmp: &Path,
    max: u64,
    deadline: Instant,
) -> Result<u64, String> {
    let mut file = File::create(tmp)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_CREATE_FILE, &[&tmp.display(), &e]))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        if Instant::now() > deadline {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_DOWNLOAD,
                &[&format!("下载超过 {}s", DOWNLOAD_DEADLINE.as_secs())],
            ));
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_DOWNLOAD, &[&e]))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > max {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_DOWNLOAD,
                &[&format!("包超过 {max} 字节")],
            ));
        }
        file.write_all(&buf[..n])
            .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_WRITE_FILE, &[&tmp.display(), &e]))?;
    }
    Ok(written)
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
            owner(&root).verify_current_signed_by(&public).is_ok(),
            "一份完整且签过名的 bin 应当通过"
        );
        // 换了二进制却没换清单：这正是「有人动过安装根」的形态。
        std::fs::write(bin.join(platform::client_exe()), b"evil").unwrap();
        assert!(
            owner(&root).verify_current_signed_by(&public).is_err(),
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
        assert!(owner(&root).verify_current_signed_by(&public).is_ok());

        std::fs::write(bin.join("extra.dll"), b"planted").unwrap();
        assert!(
            owner(&root).verify_current_signed_by(&public).is_err(),
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
        assert!(owner(&root)
            .verify_current_signed_by(&public_hex(&key))
            .is_err());
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
            owner(&root)
                .verify_current_signed_by(&public_hex(&stranger))
                .is_err(),
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

    /// 测试里的「这一个启动器」：取到安装根写权，做一件事，放手。
    fn owner(root: &Path) -> Owner {
        own(root).unwrap()
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

        assert!(owner(&root).install(&pkg, true).is_err(), "缺签名必须拒绝");
        assert!(bin.join("KEEP").is_file(), "验签失败不该动 bin/");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn a_bin_without_a_manifest_is_refused() {
        let root = temp_root("verify-current-missing");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        assert!(owner(&root).verify_current().is_err(), "没有清单就不能启动");
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
            owner(&root)
                .install_signed_by(&pkg, true, &public_hex(&key))
                .is_err(),
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

        owner(&root)
            .install_signed_by(&pkg, true, &public_hex(&key))
            .unwrap();

        assert_eq!(read_tx(&root).unwrap(), Some(Tx::Installed));
        assert!(
            root.join(BIN_OLD).join("KEEP").is_file(),
            "确认之前 bin.old 必须留着，它是唯一的回滚材料"
        );
        assert!(bin.join(platform::client_exe()).is_file());

        owner(&root).commit().unwrap();

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
        owner(&root)
            .install_signed_by(&pkg, true, &public_hex(&key))
            .unwrap();

        owner(&root).abort().unwrap();

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

        assert!(owner(&root)
            .install_signed_by(&pkg, true, &public_hex(&key))
            .is_err());

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

        owner(&root).recover().unwrap();

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

        owner(&root).recover().unwrap();

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

        owner(&root).recover().unwrap();

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

        owner(&root).recover().unwrap();

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

        owner(&root).recover().unwrap();

        assert!(bin.join("NEW").is_file());
        assert!(!root.join(BIN_OLD).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_install_state_refuses_to_guess() {
        let root = temp_root("recover-garbage");
        fs::write(tx_path(&root), b"half-way").unwrap();

        assert!(
            owner(&root).recover().is_err(),
            "认不出的状态不能猜：猜错就会拿错的前提删文件"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 待安装的包是「请安装」的信号，不是安装残留：启动时的自愈不能删掉它，
    /// 否则 Linux 上客户端留下的更新永远装不上。
    #[test]
    fn recover_keeps_a_pending_package() {
        let root = temp_root("recover-keeps-package");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        fs::write(root.join(layout::BIN).join("mesh-client"), b"client").unwrap();
        fs::write(root.join(UPDATE_FILE), b"package").unwrap();

        owner(&root).recover().unwrap();

        assert!(
            root.join(UPDATE_FILE).is_file(),
            "启动自愈删掉了待安装的包：这台机器再也装不上更新"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 装上之后包才算被消费掉。
    #[test]
    fn commit_removes_the_consumed_package() {
        let root = temp_root("commit-drops-package");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        fs::create_dir_all(root.join(BIN_OLD)).unwrap();
        fs::write(root.join(UPDATE_FILE), b"package").unwrap();
        write_tx(&root, Tx::Installed).unwrap();

        owner(&root).commit().unwrap();

        assert!(!root.join(UPDATE_FILE).exists(), "装完的包要删掉");
        assert!(!root.join(BIN_OLD).exists());
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
    }

    /// 普通启动确认成功（没有在途安装）不能凭空写出事务状态，也不能删掉待装包：
    /// 那是客户端交接的「请安装」信号，只由消耗它的安装事务删。
    #[test]
    fn commit_without_an_install_leaves_no_state_behind() {
        let root = temp_root("commit-idle");
        let bin = root.join(layout::BIN);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join(BIN_OLD)).unwrap();
        fs::write(root.join(UPDATE_FILE), b"package").unwrap();

        owner(&root).commit().unwrap();

        assert!(
            read_tx(&root).unwrap().is_none(),
            "没有在途安装时不该写状态：{}",
            tx_path(&root).display()
        );
        assert!(!root.join(BIN_OLD).exists(), "残留的备份照旧清掉");
        assert!(bin.exists(), "普通启动确认不能动 bin/");
        assert!(
            root.join(UPDATE_FILE).is_file(),
            "没有安装事务时删掉了待装包：客户端留下的更新永远不会被装上"
        );
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
    /// F1：回滚已经把旧版本恢复回来，但状态文件还没清掉（清理失败，或进程在清理前
    /// 被杀）。再次回滚不能把刚恢复的那一份当成「首次安装的产物」删掉。
    #[test]
    fn a_restored_version_survives_a_second_rollback() {
        let root = temp_root("rollback-twice");
        let src = temp_root("rollback-twice-src");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let public = public_hex(&key);
        let owner = owner(&root);
        let pkg = valid_package(&src, "2026.11.27-2", &required_entries());
        owner.install_signed_by(&pkg, true, &public).unwrap();
        assert_eq!(owner.read_tx().unwrap(), Some(Tx::Installed));

        // 恢复做完、状态没清：这正是「清理失败或进程在清理前中止」留下的形状。
        owner.undo(Tx::Installed).unwrap();
        assert!(bin.join("KEEP").is_file(), "恢复完成之后 bin/ 必须是旧版本");
        assert!(!bin.join(platform::client_exe()).exists());
        assert_eq!(
            owner.read_tx().unwrap(),
            Some(Tx::Restoring),
            "换回旧版本这一步要留下可重复的判据"
        );

        // 第二次放弃安装：状态是「正在换回」，这一步必须是空操作。
        owner.abort().unwrap();
        assert!(
            bin.join("KEEP").is_file(),
            "第二次回滚删掉了唯一能启动的版本"
        );
        assert_no_install_in_flight(&root);

        // 再装一次、再退回一次，原版本仍然完整。
        let pkg = valid_package(&src, "2026.11.27-3", &required_entries());
        owner.install_signed_by(&pkg, true, &public).unwrap();
        assert!(bin.join(platform::client_exe()).is_file());
        owner.abort().unwrap();
        assert!(bin.join("KEEP").is_file(), "第三次回滚之后原版本仍要完整");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F1：换回旧版本中途断掉留下的形状——旧版本已经回到 bin/、被丢下的新版本还在
    /// bin.stale/、状态是 restoring。下一次恢复只收尾，不能动 bin/。
    #[test]
    fn a_half_finished_restore_only_cleans_up() {
        let root = temp_root("restore-half");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        fs::create_dir_all(root.join(BIN_STALE)).unwrap();
        fs::write(root.join(BIN_STALE).join("NEW"), b"new").unwrap();
        write_tx(&root, Tx::Restoring).unwrap();

        owner(&root).recover().unwrap();

        assert!(bin.join("KEEP").is_file(), "恢复只该收尾，不该动 bin/");
        assert!(!root.join(BIN_STALE).exists(), "被丢下的新版本要删掉");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
    }

    /// F2：`installed` 的验证失败也要有确定结局——退回旧版本，而不是留一个装了一半
    /// 的新版本等下一次启动去猜。
    #[test]
    fn a_new_version_that_fails_verification_is_rolled_back() {
        let root = temp_root("boot-verify");
        let src = temp_root("boot-verify-src");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let owner = owner(&root);
        let pkg = valid_package(&src, "2026.11.27-5", &required_entries());
        owner
            .install_signed_by(&pkg, true, &public_hex(&key))
            .unwrap();
        // 解包之后文件被换掉：这就是「验证失败」的形态。
        fs::write(bin.join(platform::client_exe()), b"tampered").unwrap();

        assert!(owner.boot_and_settle().is_err(), "验证不过就不该拉起");

        assert!(bin.join("KEEP").is_file(), "验证失败要把旧版本退回来");
        assert!(!bin.join(platform::client_exe()).exists());
        assert_no_install_in_flight(&root);
        assert_eq!(Failures::load(&root).count("2026.11.27-5"), 1);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F2：拉起失败（清单与摘要都对，但主程序根本执行不了）同样要退回旧版本。
    #[test]
    fn a_new_version_that_cannot_start_is_rolled_back() {
        let root = temp_root("boot-spawn");
        let src = temp_root("boot-spawn-src");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let owner = owner(&root);
        // 摘要与清单一致，所以验证能过；内容不是可执行文件，所以拉起必然失败。
        let entries: [(&str, &[u8]); 2] = [
            (platform::client_exe(), b"not an executable"),
            (platform::core_name(), b"core"),
        ];
        let pkg = package(
            &src,
            &signed_manifest(&key, "2026.11.27-8", &entries),
            &entries,
        );
        owner
            .install_signed_by(&pkg, true, &public_hex(&key))
            .unwrap();

        assert!(owner.boot_and_settle().is_err(), "执行不了的主程序必须报错");

        assert!(bin.join("KEEP").is_file(), "拉起失败要把旧版本退回来");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F4：安装根只有一个写者。第二个启动器拿不到所有权，也就改不了事务状态。
    #[test]
    fn a_second_launcher_cannot_touch_the_install_root() {
        let root = temp_root("ownership");
        let src = temp_root("ownership-src");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        let pkg = broken_package(&src);
        write_tx(&root, Tx::Replaced).unwrap();

        let first = owner(&root);
        assert!(own(&root).is_err(), "安装根已经有一个写者，第二个必须失败");
        assert!(
            run(&root, Source::File(pkg), None, None, true).is_err(),
            "第二个启动器不该装得进去"
        );
        assert_eq!(
            first.read_tx().unwrap(),
            Some(Tx::Replaced),
            "事务状态被另一个启动器改了"
        );

        drop(first);
        assert!(own(&root).is_ok(), "第一个写者退出后应当能让位");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：条目数超限的包在动 bin/ 之前就被拒绝。
    #[test]
    fn a_package_with_too_many_entries_is_refused() {
        let root = temp_root("budget-entries");
        let src = temp_root("budget-entries-src");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        // 1025 个条目：多一个就超限。
        let names: Vec<String> = (0..1025).map(|i| format!("f{i}")).collect();
        let entries: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), b"x".as_slice()))
            .collect();
        let manifest = signed_manifest(&key, "2026.11.27-6", &required_entries());
        let pkg = package(&src, &manifest, &entries);

        assert!(read_manifest(&pkg).is_err(), "条目数超限必须拒绝");
        assert!(owner(&root)
            .install_signed_by(&pkg, true, &public_hex(&key))
            .is_err());
        assert!(bin.join("KEEP").is_file(), "超限的包不该动 bin/");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：清单字节数有上限。一份能在合法域内解析、但大到不像出包脚本产出的清单
    /// 同样拒绝。
    #[test]
    fn a_package_with_an_oversized_manifest_is_refused() {
        let root = temp_root("budget-manifest");
        let src = temp_root("budget-manifest-src");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let mut manifest = signed_manifest(&key, "2026.11.27-7", &required_entries());
        manifest.push_str(&" ".repeat(70 * 1024));
        let pkg = package(&src, &manifest, &required_entries());

        assert!(owner(&root)
            .install_signed_by(&pkg, true, &public_hex(&key))
            .is_err());
        assert!(bin.join("KEEP").is_file(), "超限的清单不该动 bin/");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：声明的解压后总量超限，在读清单这一步就拒绝——旧版本还没被挪走。
    #[test]
    fn a_package_that_declares_too_much_is_refused() {
        let src = temp_root("budget-declared-src");
        let pkg = valid_package(&src, "2026.11.27-9", &required_entries());
        let budget = Budget {
            unpacked: 8,
            ..Budget::package()
        };
        assert!(
            read_manifest_with(&pkg, budget).is_err(),
            "声明的解压后总量超限必须拒绝"
        );
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：解包时的实际字节数同样有上限，不是只看清单上的声明。
    #[test]
    fn unpacking_stops_at_the_byte_budget() {
        let src = temp_root("budget-unpack-src");
        let dest = temp_root("budget-unpack-dest");
        let pkg = valid_package(&src, "2026.11.27-10", &required_entries());
        let budget = Budget {
            unpacked: 8,
            ..Budget::package()
        };
        assert!(unpack(&pkg, &dest, budget).is_err(), "解压输出超限必须失败");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
    }

    /// F7：超时的包不再往下读。
    #[test]
    fn an_expired_budget_stops_the_scan() {
        let src = temp_root("budget-deadline-src");
        let pkg = valid_package(&src, "2026.11.27-11", &required_entries());
        let budget = Budget {
            deadline: Instant::now() - Duration::from_secs(1),
            ..Budget::package()
        };
        assert!(
            read_manifest_with(&pkg, budget).is_err(),
            "过期的预算必须停下"
        );
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：下载先落 `.part`。超预算时半截文件不留下，正式名（「请安装」的信号）
    /// 也不会出现。
    #[test]
    fn a_download_over_its_budget_leaves_no_part_file() {
        let dir = temp_root("download-budget");
        let tmp = part_path(&dir.join(UPDATE_FILE));
        let mut body = std::io::Cursor::new(vec![0u8; 64]);
        assert!(
            copy_bounded(&mut body, &tmp, 16, Instant::now() + Duration::from_secs(5)).is_err()
        );
        assert!(!tmp.exists(), "超预算的下载留下了半截文件");
        assert!(!dir.join(UPDATE_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// F7：下载超时同样不留下半截文件。
    #[test]
    fn a_download_that_misses_its_deadline_leaves_no_part_file() {
        let dir = temp_root("download-deadline");
        let tmp = part_path(&dir.join(UPDATE_FILE));
        let mut body = std::io::Cursor::new(vec![0u8; 64]);
        let deadline = Instant::now() - Duration::from_secs(1);
        assert!(copy_bounded(&mut body, &tmp, 1024, deadline).is_err());
        assert!(!tmp.exists(), "超时的下载留下了半截文件");
        let _ = fs::remove_dir_all(&dir);
    }

    /// F7：包里的条目名必须是单层普通文件名。路径穿越由这一条判据在动 bin/ 之前挡下。
    #[test]
    fn a_package_with_a_traversing_entry_is_refused() {
        let root = temp_root("budget-traversal");
        let src = temp_root("budget-traversal-src");
        let bin = root.join(layout::BIN);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("KEEP"), b"keep").unwrap();
        let key = test_key();
        let entries = required_entries();
        let path = src.join("pkg.tar.zst");
        {
            let file = File::create(&path).unwrap();
            let encoder = zstd::Encoder::new(file, 3).unwrap();
            let mut builder = tar::Builder::new(encoder);
            append(
                &mut builder,
                "version.json",
                signed_manifest(&key, "2026.11.27-12", &entries).as_bytes(),
            );
            // 子目录条目：tar 允许，但清单的合法域只允许单层普通文件名。
            append(&mut builder, "sub/evil", b"evil");
            let encoder = builder.into_inner().unwrap();
            encoder.finish().unwrap();
        }

        assert!(owner(&root)
            .install_signed_by(&path, true, &public_hex(&key))
            .is_err());
        assert!(bin.join("KEEP").is_file(), "非法条目的包不该动 bin/");
        assert!(!bin.join("sub").exists(), "非法条目不该落到 bin/ 里");
        assert_no_install_in_flight(&root);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }
}
