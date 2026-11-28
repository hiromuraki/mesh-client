use crate::args::Source;
use crate::launch;
use crate::log;
use mesh_install::artifact;
use mesh_install::failures::Failures;
use mesh_install::layout;
use mesh_install::lock::{acquire as acquire_lock, Lock};
use mesh_install::manifest::{self, Manifest};
use mesh_install::platform;
use mesh_install::signing;
use mesh_install::version as version_order;
use mesh_text::id;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// 槽目录、`bin/current` 与待装包的形状归 mesh-install::layout 管：客户端也要用同一套
// 判据（提交当前槽、读当前版本），各写一份迟早分叉。
pub(crate) use mesh_install::layout::UPDATE_FILE;

/// WAIT_CLIENT 是“等旧客户端退出”的上限，与客户端调用时的预期一致。
const WAIT_CLIENT: Duration = Duration::from_secs(60);
/// WAIT_BOOT 是“等新版确认启动”的上限。客户端在引擎与面板就位时提交 `bin/current`，
/// 不等网络，所以正常情况下是秒级。
pub(crate) const WAIT_BOOT: Duration = Duration::from_secs(60);
/// 等启动确认时的轮询间隔。
const POLL: Duration = Duration::from_millis(200);
/// 包本身的字节上限。与控制面发布时的 200MB 对齐：那边收不下更大的，这边也不装。
const PACKAGE_BYTES: u64 = 200 << 20;
/// --url 下载的总时限。与客户端下包那条路的上限同量级，只是各自判一次的代价。
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(300);

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
    /// 读清单与解包的**同一个**截止时刻。契约里的 120 s 是这两步合起来的预算：
    /// 各自新建一个等于把上限翻倍，重复扫描的代价也就被藏在两次时限之间。
    deadline: Instant,
}

impl Budget {
    /// 一个包允许的最大开销。留了两个数量级余量，同时把「把安装根写满」与
    /// 「解一辈子」挡在外面。
    ///
    /// 清单字节数与 [`manifest::MANIFEST_BYTES`] 是同一个数字：包里的清单与已安装
    /// 槽里的清单没有理由用两套上限。
    fn package() -> Self {
        Self {
            entries: 1024,
            manifest: manifest::MANIFEST_BYTES,
            unpacked: 256 << 20,
            deadline: Instant::now() + Duration::from_secs(120),
        }
    }
}

/// 解压器的窗口预算：zstd 的窗口最大 2^26 = 64 MiB。
///
/// 与 [`Budget::unpacked`] 是两个预算：那个管写出多少字节，这个管解压器自己要占
/// 多少内存。窗口大小由包在帧头里声明，不设上限就等于把启动器的内存交给包——
/// 一个 20 MiB 的包可以声明 4 GiB 的窗口。
const UNPACK_WINDOW_LOG: u32 = 26;

/// 安装根的所有权：槽目录、`bin/current` 的修复、待装包的写都在它之内。
///
/// 拿不到它，这里一行都不写。Windows 上它随一次 `--file` / 启动命令存在；Linux 上
/// 常驻启动器每轮从选槽握到确认，确认之后放手——客户端提交更新包也要拿同一把锁。
///
/// 进程退出（含被杀）时内核会释放文件锁，不会留下需要人工删的东西。
pub(crate) struct Owner {
    /// 安装根：槽目录、`bin/current` 与 `install.lock`。
    root: PathBuf,
    /// 数据根：待装包、失败计数与 `launcher.json`。
    data: PathBuf,
    /// 验清单签名用的公钥。生产代码用编进二进制的发布公钥；轮换与测试换这一项。
    public: String,
    _lock: Lock,
}

/// 选中的槽：名字与清单里的版本号。
#[derive(Debug)]
pub(crate) struct Selected {
    pub(crate) name: &'static str,
    pub(crate) version: String,
}

/// 候选槽试运行失败时的三种形状。调用方不能把它们收成一个 String：
/// 「没创建出来」「确认失败且已停止」按版本计次、可以回退；「停不下来」的进程
/// 事实还在，既不能计次，也不能在它脚下把旧槽拉起来。
#[derive(Debug)]
pub(crate) enum BootFailure {
    /// 候选槽的客户端进程没能创建。
    Create(String),
    /// 拉起成功但没等到 `bin/current`，且已经确认候选进程退出。
    Confirm(String),
    /// 没等到确认，且没能确认候选进程退出：它可能还活着。
    NotStopped(String),
}

impl BootFailure {
    /// 给日志与调用方看的原文。三种形状的文案在产生处就已经写清原因。
    pub(crate) fn reason(&self) -> &str {
        match self {
            BootFailure::Create(reason)
            | BootFailure::Confirm(reason)
            | BootFailure::NotStopped(reason) => reason,
        }
    }
}

/// own 取得安装根的写权。已有写者时返回错误，不排队：
///
/// * 写者可能是另一个启动器（正在装或正在确认），
/// * 也可能是客户端（正在提交更新包或替换启动器）。
///
/// 冲突不靠它预防：客户端在跑时手工安装由 `client_running` 判断并拒绝。
pub(crate) fn own(root: &Path, data: &Path) -> Result<Owner, String> {
    own_with_key(root, data, signing::RELEASE_PUBLIC_KEY)
}

/// 用给定公钥验清单的 [`own`]。轮换公钥与测试走这一条，所有权语义完全相同。
fn own_with_key(root: &Path, data: &Path, public_hex: &str) -> Result<Owner, String> {
    // 文案与客户端的单实例锁分开：占着安装根的可能是另一个启动器，那时「已有客户端
    // 实例在运行」是错的（客户端可能早就退出了）。非争用错误照原样上抛。
    let lock = match acquire_lock(&Lock::owner_path(data)) {
        Ok(lock) => lock,
        Err(mesh_install::Error::AlreadyRunning) => {
            return Err(mesh_text::get(id::LAUNCHER_ERROR_INSTALL_ROOT_BUSY).to_string())
        }
        Err(err) => return Err(err.to_string()),
    };
    Ok(Owner {
        root: root.to_path_buf(),
        data: data.to_path_buf(),
        public: public_hex.to_string(),
        _lock: lock,
    })
}

impl Owner {
    /// prepare 是每个入口在拉起任何进程之前做的第一件事：清理另一个槽、修复
    /// `bin/current`，再按选槽顺序挑一个能启动的槽。
    ///
    /// 返回 `None` 表示两个槽都无效：首次安装或需要人工。这不是错误——装包路径会
    /// 把候选槽定为 `alice`，普通启动路径据此报「没有可启动版本」。
    pub(crate) fn prepare(&self) -> Result<Option<Selected>, String> {
        let current = layout::read_current(&self.root);
        let alice = self.valid_slot(layout::SLOT_ALICE);
        let bob = self.valid_slot(layout::SLOT_BOB);

        // 选槽：`current` 有效就用它；否则在有效槽里取目录名字典序最大的（bob 优先）。
        // 只看名字，不看目录时间，也不看清单里的版本号。
        let chosen = match current {
            Some(slot) if slot_version(&alice, &bob, slot).is_some() => Some(slot),
            _ if bob.is_some() => Some(layout::SLOT_BOB),
            _ if alice.is_some() => Some(layout::SLOT_ALICE),
            _ => None,
        };
        let Some(name) = chosen else {
            // 两个槽都无效：`current` 无论指向哪个槽都不可达，留着它只会在下一次
            // 安装里与候选槽撞名——首次安装固定进 `alice`，那个遗留值会被当成
            // 「这一次的启动确认」。删掉它等于回到契约里的「还没有提交过」。
            if current.is_some() {
                remove_path(&layout::current_path(&self.root));
            }
            return Ok(None);
        };
        let version = slot_version(&alice, &bob, name)
            .unwrap_or_default()
            .to_string();

        // 修复：`current` 缺失、值非法或指向的槽无效时补写一次。
        //
        // 写不进去只记一行、不让这一次流程失败：普通启动不该要求安装根可写，
        // 而未提权时它确实不可写。确认判据不受影响——客户端启动成功后会自己提交
        // `current`（它带 requireAdministrator），那次写入本来就要发生。
        if current != Some(name) {
            log::line(&mesh_text::format(
                id::LAUNCHER_TEXT_REPAIR_CURRENT,
                &[&name],
            ));
            if let Err(err) = layout::write_current(&self.root, name) {
                log::line(&mesh_text::format(
                    id::LAUNCHER_ERROR_CURRENT_WRITE,
                    &[&layout::current_path(&self.root).display(), &err],
                ));
            }
        }
        Ok(Some(Selected { name, version }))
    }

    /// 某个槽的有效版本号；槽无效（没有清单、验签不过、文件集合不符）就是 `None`。
    fn valid_slot(&self, slot: &str) -> Option<String> {
        verify_slot(&layout::slot_dir(&self.root, slot), &self.public)
            .ok()
            .map(|manifest| manifest.version().to_string())
    }

    /// install 把包装进候选槽：先清空它，解包，再按最终落盘的内容校验。
    ///
    /// 校验不过（解包出来的清单缺失、验签不过、文件集合不符）说明包本身不可用：
    /// 丢弃候选槽、不记失败次数——记了只会让一个坏包把版本号钉死。拉起与启动确认
    /// 是下一步，失败才按版本计次。
    fn install(
        &self,
        pkg: &Path,
        target: &'static str,
        current_version: Option<&str>,
        force: bool,
    ) -> Result<Selected, String> {
        // 所有进入安装的包走同一道字节闸：Linux 的 update.tar.zst 不经过 run()，
        // 只在那里判等于给这条路留了后门。
        check_package_bytes(pkg)?;
        // 不变量：开始试运行之前 current 不得已经等于本次候选槽。
        //
        // 启动确认的唯一判据是「current 变成正在跑的那个槽」。若它本来就是这个槽，
        // 拉起之后的第一眼就会看见它，一次根本没提交过的安装会被当成确认成功，
        // 随后删掉旧槽。这个形状只能来自遗留指针（两个槽都无效时的 current 已被
        // prepare 清掉），在这里明确拒绝，不去猜。
        if layout::read_current(&self.root) == Some(target) {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_CANDIDATE_IS_CURRENT,
                &[&target],
            ));
        }
        // 读清单与解包共用一个预算：同一个截止时刻，同一组上限。
        let budget = Budget::package();
        let manifest = read_manifest_with(pkg, budget)?;
        // 签名与本机适用性放在动候选槽之前：坏包不该先清掉唯一的启动材料。
        check_manifest(&manifest, &self.public)?;
        let version = manifest.version().to_string();

        // 降级防护：服务端把发布记录指回旧包时，客户端会当成“有新版本”下载下来。
        // 2026-11-26 两个种子就是这么被降级到 -3 的。 --force 留给手动修机器。
        if !force {
            if let Some(old) = current_version {
                if !version_order::is_newer(&version, old) {
                    return Err(mesh_text::format(
                        id::LAUNCHER_ERROR_NOT_NEWER,
                        &[&version, &old],
                    ));
                }
            }
            let failed = Failures::load(&self.data);
            if failed.exhausted(&version) {
                return Err(mesh_text::format(
                    id::LAUNCHER_ERROR_FAILURES_EXHAUSTED,
                    &[&version, &failed.count(&version)],
                ));
            }
        }
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_INSTALLING,
            &[&version, &manifest.files().len()],
        ));

        let dir = layout::slot_dir(&self.root, target);
        remove_dir(&dir)?;
        fs::create_dir_all(&dir).map_err(|e| {
            mesh_text::format(id::LAUNCHER_ERROR_CREATE_SLOT, &[&dir.display(), &e])
        })?;
        if let Err(e) = unpack(pkg, &dir, budget) {
            return Err(self.discard_candidate(target, e));
        }
        if let Err(e) = verify_slot(&dir, &self.public) {
            return Err(self.discard_candidate(target, e));
        }
        log::line(mesh_text::get(id::LAUNCHER_TEXT_VERIFIED));
        Ok(Selected {
            name: target,
            version,
        })
    }

    /// 候选槽没有产出有效清单：丢弃它的内容，不计失败次数。调用方负责删待装包。
    fn discard_candidate(&self, target: &str, reason: String) -> String {
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_BAD_PACKAGE,
            &[&target, &reason],
        ));
        remove_path(&layout::slot_dir(&self.root, target));
        reason
    }

    /// apply_pending 消费待装包 `<root>/update.tar.zst` 与它的摘要。
    ///
    /// 返回 `Some` 表示包装进了候选槽（下一次拉起它）；`None` 表示包被丢弃——不比
    /// 当前新、失败次数已满、摘要不符、或包本身不可用。无论哪种结果，包与摘要都
    /// 不再有用。
    ///
    /// 只有 Linux 的常驻启动器消费 `update.tar.zst`：Windows 的交接走 `--file`，
    /// 包是命令参数，摘要也在命令行上。
    #[cfg(unix)]
    pub(crate) fn apply_pending(&self, selected: Option<&Selected>) -> Option<Selected> {
        let pkg = self.data.join(UPDATE_FILE);
        if !pkg.is_file() {
            return None;
        }
        let digest = self.data.join(layout::UPDATE_DIGEST);
        let target = candidate(selected);
        let current_version = selected.map(|slot| slot.version.as_str());
        // 摘要就是客户端验过发布签名的那一份：对不上说明盘上的字节已经不是被授权
        // 的那一份，连候选槽都不该动。
        let outcome = match pending_digest(&digest) {
            Ok(None) => self.install(&pkg, target, current_version, false),
            Ok(Some(want)) => match sha256_file(&pkg) {
                Ok(got) if got.eq_ignore_ascii_case(&want) => {
                    self.install(&pkg, target, current_version, false)
                }
                Ok(got) => Err(mesh_text::format(
                    id::LAUNCHER_ERROR_DIGEST_MISMATCH,
                    &[&got, &want],
                )),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        // 安装事务是唯一可以删待装包的：普通启动不碰它，否则 Linux 上的更新永远装不上。
        // 摘要与包同生命周期：读到就一起删，别让下一份包继承上一份的摘要。
        remove_path(&pkg);
        remove_path(&digest);
        match outcome {
            Ok(installed) => Some(installed),
            Err(e) => {
                log::line(&mesh_text::format(id::LAUNCHER_TEXT_UPDATE_UNUSABLE, &[&e]));
                None
            }
        }
    }

    /// 拉起当前槽；普通启动的出口。
    ///
    /// 不做启动确认：`current` 本来就指着这个槽，确认恒为真，等下去只会多一条
    /// 永远走不进的分支。进程是死是活由调用方守着——Linux 的常驻启动器等它退出，
    /// Windows 的启动器拉起来就放手。
    pub(crate) fn boot_current(&self, selected: &Selected) -> Result<launch::Client, String> {
        self.start(selected.name)
    }

    /// 拉起刚装好的候选槽并等它提交 `bin/current`；安装路径的出口。
    ///
    /// 失败时停掉候选槽、按版本记一次失败：`current` 不动、候选槽留着。
    /// 停不掉就不计数——它可能还活着，「起不来」还没被证明。调用方必须按
    /// BootFailure 的三种形状分别收尾，不能把它们当成同一种失败。
    pub(crate) fn boot_candidate(
        &self,
        installed: &Selected,
    ) -> Result<launch::Client, BootFailure> {
        let mut client = match self.start(installed.name) {
            Ok(client) => client,
            Err(reason) => {
                // 进程都没创建出来：这一次必然失败，按版本计次，免得同一版本被
                // 反复拉起、反复在同一个地方失败。
                record_failure(&self.data, &installed.version);
                return Err(BootFailure::Create(reason));
            }
        };
        match self.confirm(installed.name, &mut client) {
            Ok(()) => {
                clear_failure(&self.data, &installed.version);
                self.drop_old_slot(installed.name);
                Ok(client)
            }
            Err(reason) => {
                log::line(&mesh_text::format(
                    id::LAUNCHER_TEXT_BOOT_FAILED,
                    &[&installed.name],
                ));
                Err(self.candidate_failed(installed, reason, client.stop()))
            }
        }
    }

    /// 提交之后旧槽才是可丢的：此刻 `current` 已经指向新槽，旧槽既不是在跑的那一份，
    /// 也不是回滚材料。
    ///
    /// 只能在这里删：启动器无法区分一个有效槽是「上一次提交留下的旧版本」，还是
    /// 「这一次刚装好、还没确认的候选槽」——按「另一个槽有效就删」去做，会把刚失败
    /// 的候选槽一起删掉，那次安装从此无法重试。
    fn drop_old_slot(&self, keep: &str) {
        let old = other_slot(keep);
        if layout::slot_dir(&self.root, old).exists() {
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_CLEAN_SLOT, &[&old]));
            remove_path(&layout::slot_dir(&self.root, old));
        }
    }

    /// 确认失败之后的收尾：先停掉候选槽，再按「停没停掉」分成两个结局。
    ///
    /// 停掉了才是「这一版起不来」，按版本记一次失败。停不掉则进程事实还在，
    /// 计数与回退都会把它当成「没在跑」——那会把一个还占着 TUN 与实例锁的进程
    /// 留在后面每一次安装的脚下。
    fn candidate_failed(
        &self,
        installed: &Selected,
        reason: String,
        stopped: Result<(), String>,
    ) -> BootFailure {
        match stopped {
            Ok(()) => {
                record_failure(&self.data, &installed.version);
                BootFailure::Confirm(reason)
            }
            Err(e) => {
                log::line(&mesh_text::format(id::LAUNCHER_TEXT_STOP_FAILED, &[&e]));
                BootFailure::NotStopped(mesh_text::format(
                    id::LAUNCHER_ERROR_CANDIDATE_NOT_STOPPED,
                    &[&installed.name, &e],
                ))
            }
        }
    }

    /// 等 `bin/current` 变成这个槽。客户端提交之后启动才算确认。
    fn confirm(&self, slot: &str, client: &mut launch::Client) -> Result<(), String> {
        wait_current(&self.root, slot, Instant::now() + WAIT_BOOT, || {
            client.exited()
        })?;
        log::line(&mesh_text::format(
            id::LAUNCHER_TEXT_BOOT_CONFIRMED,
            &[&slot],
        ));
        Ok(())
    }

    /// 拉起一个槽里的客户端。清单与文件已经在选槽或装包时验过，这里只确认主程序
    /// 真的落在那个目录里。
    fn start(&self, slot: &str) -> Result<launch::Client, String> {
        let exe = layout::slot_dir(&self.root, slot).join(launch::CLIENT_EXE);
        if !exe.is_file() {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_CLIENT_MISSING,
                &[&exe.display()],
            ));
        }
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_START_CLIENT, &[&slot]));
        launch::start(&exe, &self.root)
    }
}

/// 两个槽互为候选：非当前槽就是候选槽，不需要任何记录。
fn other_slot(slot: &str) -> &'static str {
    if slot == layout::SLOT_ALICE {
        layout::SLOT_BOB
    } else {
        layout::SLOT_ALICE
    }
}

/// 装包的目标槽：有可判定的当前槽就装另一个，判不出来（首次安装）固定 `alice`。
fn candidate(selected: Option<&Selected>) -> &'static str {
    selected.map_or(layout::SLOT_ALICE, |slot| other_slot(slot.name))
}

fn slot_version<'a>(
    alice: &'a Option<String>,
    bob: &'a Option<String>,
    slot: &str,
) -> Option<&'a str> {
    match slot {
        layout::SLOT_ALICE => alice.as_deref(),
        layout::SLOT_BOB => bob.as_deref(),
        _ => None,
    }
}

/// 没有可启动版本时的错误。首次安装也走这里：装包路径不会用到它。
pub(crate) fn no_slot_error() -> String {
    mesh_text::get(id::LAUNCHER_ERROR_NO_SLOT).to_string()
}

/// run 是手动安装入口（--file / --url）。
///
/// 顺序是契约里的那条：等旧客户端让位 → 清理与选槽 → 装进候选槽 → 拉起并等它提交。
/// 成功与失败都由它自己收口：客户端已经退出，没有人替它把节点拉回来。
pub fn run(
    root: &Path,
    data: &Path,
    source: Source,
    want_sha: Option<String>,
    wait_for: Option<u32>,
    force: bool,
) -> Result<(), String> {
    // 整个事务在所有权之内：从等旧客户端、清理槽到启动确认，中间不放锁。
    let owner = own(root, data)?;
    if let Some(pid) = wait_for {
        log::line(&mesh_text::format(id::LAUNCHER_TEXT_WAIT_PROCESS, &[&pid]));
        launch::wait_exit(pid, WAIT_CLIENT)?;
    } else if client_running(data) {
        // 没有 --wait-for 却还有客户端在跑：在它脚下换槽目录，它的下一次文件访问就
        // 落到另一个版本上。直接拒绝，让人先退出客户端。
        return Err(mesh_text::get(id::INSTALL_ERROR_ALREADY_RUNNING).to_string());
    }
    ensure_install_root_writable(root)?;
    let selected = owner.prepare()?;
    let target = candidate(selected.as_ref());

    let pkg = match source {
        Source::File(p) => resolve_package(p, data),
        Source::Url(url) => {
            let dest = data.join(UPDATE_FILE);
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_DOWNLOAD, &[&url]));
            download(&url, &dest)?;
            dest
        }
    };

    // 摘要之前先按文件大小拒绝：--file 收的是用户给的文件，整包求摘要本身就是
    // 一次不受长度限制的读。这个闸与 install() 里那道是同一个函数、同一组上限。
    check_package_bytes(&pkg)?;
    // 这一条只防“下载完成到安装之间”包被换掉，与解包结果无关。
    let sum = sha256_file(&pkg)?;
    // 只有自己下载到 <root>/update.tar.zst 的那份才删：--file 给的是用户的文件，
    // 同样的路径成功安装时反而保留，两种结果不该由失败与否决定。
    let downloaded = pkg == data.join(UPDATE_FILE);
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

    let installed = match owner.install(
        &pkg,
        target,
        selected.as_ref().map(|s| s.version.as_str()),
        force,
    ) {
        Ok(installed) => installed,
        Err(e) => {
            log::line(&mesh_text::format(id::LAUNCHER_TEXT_INSTALL_FAILED, &[&e]));
            if downloaded {
                let _ = fs::remove_file(&pkg);
            }
            return Err(restore_after_failure(&owner, selected.as_ref(), e));
        }
    };

    // 装完接着拉起新版本并等确认：客户端已经退出，没人启动了。
    let booted = owner.boot_candidate(&installed);
    if downloaded {
        let _ = fs::remove_file(&pkg);
    }
    match booted {
        Ok(client) => {
            #[cfg(unix)]
            {
                // Linux 的客户端由常驻启动器托管：这一次拉起只为确认它能跑，确认之后
                // 收手，交给 systemd 的 ExecStart 正式接管——否则系统里会多一个不受
                // 监督的客户端，下一次服务启动还会撞上它的 instance.lock。
                //
                // 停不掉就是失败：临时客户端还握着 instance.lock，systemd 接管的那一份
                // 起不来，而安装已经报「完成」。这里不宣完成。
                let mut client = client;
                client
                    .stop()
                    .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT, &[&e]))?;
                log::line(mesh_text::get(id::LAUNCHER_TEXT_INSTALLED_FOR_SYSTEMD));
            }
            #[cfg(windows)]
            {
                drop(client);
                log::line(mesh_text::get(id::LAUNCHER_TEXT_INSTALLED));
            }
            Ok(())
        }
        // 停不下来：候选进程可能还活着。此刻把当前槽拉起来只会与它抢 instance.lock，
        // 所以不回退、不宣告完成，把失败原样交出去。
        Err(BootFailure::NotStopped(reason)) => Err(reason),
        Err(failure) => Err(restore_after_failure(
            &owner,
            selected.as_ref(),
            failure.reason().to_string(),
        )),
    }
}

/// 安装或启动确认失败：把当前槽拉回来。客户端已经退出，没有人替它起。
///
/// 只有 Windows 走这一条：启动器拉起后不保留进程，失败后没有人会再启动旧版本。
/// Linux 的客户端由常驻启动器托管，这一次会话不留下不受监督的进程。
#[cfg(windows)]
fn restore_after_failure(owner: &Owner, selected: Option<&Selected>, reason: String) -> String {
    let Some(current) = selected else {
        return reason;
    };
    match owner.boot_current(current) {
        Ok(client) => {
            drop(client);
            log::line(mesh_text::get(id::LAUNCHER_TEXT_RELOADED));
            reason
        }
        Err(reload) => {
            mesh_text::format(id::LAUNCHER_ERROR_INSTALL_AND_RELOAD, &[&reason, &reload])
        }
    }
}

/// Linux 上没有「把当前槽拉回来」这一步：下一次启动由常驻启动器负责。
#[cfg(unix)]
fn restore_after_failure(_owner: &Owner, _selected: Option<&Selected>, reason: String) -> String {
    reason
}

/// launch_installed 是 Windows 的普通启动入口：清理、选槽、拉起、放手。
#[cfg(windows)]
pub(crate) fn launch_installed(root: &Path) -> Result<(), String> {
    let owner = own(root, root)?;
    let selected = owner.prepare()?.ok_or_else(no_slot_error)?;
    log::line(&mesh_text::format(
        id::LAUNCHER_TEXT_STARTING,
        &[&selected.version],
    ));
    let client = owner.boot_current(&selected)?;
    drop(client);
    Ok(())
}

/// 装包前先确认安装根可写；写不了就是没提权，直接给一句能照做的话。
///
/// 普通启动不需要这一步：它不写安装根。只有装包会创建槽目录、改 `bin/current`、
/// 删旧槽，那时安装根必须可写。
fn ensure_install_root_writable(root: &Path) -> Result<(), String> {
    let probe = root.join(".write-probe");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(mesh_text::get(id::LAUNCHER_ERROR_NEED_ADMIN).to_string())
        }
        Err(err) => Err(mesh_text::format(
            id::LAUNCHER_ERROR_CURRENT_WRITE,
            &[&root.display(), &err],
        )),
    }
}

/// 交接包的实际位置。
///
/// 交接时客户端给的是它自己写下的路径。旧客户端写在安装根，而启动器一进来就把状态
/// 搬去了数据根——包跟着一起搬走，命令行上的路径于是指空。这里按同一个文件名在数据根
/// 再找一次：字节没变，摘要校验照做，只是路径换了地方。
fn resolve_package(path: PathBuf, data: &Path) -> PathBuf {
    if path.is_file() {
        return path;
    }
    let moved = data.join(UPDATE_FILE);
    if moved.is_file() {
        return moved;
    }
    path
}

/// 有没有客户端正占着这个安装根。
///
/// 判据与客户端自己启动时用的是同一个锁文件（`instance.lock`，在数据根下）：
/// 拿得到就是没人在跑，拿不到就是有。
/// 拿到之后立刻放开——这里只是看一眼，不是要占住它。
fn client_running(data: &Path) -> bool {
    match acquire_lock(&Lock::path(data)) {
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

/// 进入安装的包的字节闸：超过 [`PACKAGE_BYTES`] 的包在求摘要、读清单之前就拒绝。
///
/// 两条入口（`--file` / `--url` 的 `run()`，Linux 常驻启动器的 `update.tar.zst`）
/// 走同一个函数：一个包因为来路不同而免检，等于没有上限。
fn check_package_bytes(pkg: &Path) -> Result<(), String> {
    let size = fs::metadata(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?
        .len();
    if size > PACKAGE_BYTES {
        return Err(package_error(
            pkg,
            &format!("包有 {size} 字节，超过上限 {PACKAGE_BYTES} 字节"),
        ));
    }
    Ok(())
}

/// 读包根的清单，同时把预算核一遍。
///
/// 清单必须在动候选槽之前读出来，预算也只能在这里判：条目过多、或声明的解压后
/// 总量超限的包，不该走到「候选槽已经被清空」那一步。
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
    // 一次读完整个包：预算要在动候选槽之前判完，条目名也在这一遍定死，
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
    unpack_with(pkg, dest, budget, UNPACK_WINDOW_LOG)
}

/// 同 [`unpack`]，但解压器的窗口上限由调用方给：测试用它证明这一道预算真的生效。
fn unpack_with(pkg: &Path, dest: &Path, budget: Budget, window_log: u32) -> Result<(), String> {
    let file = File::open(pkg)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_OPEN_PACKAGE, &[&pkg.display(), &e]))?;
    let mut decoder = zstd::Decoder::new(file).map_err(|e| {
        mesh_text::format(id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE, &[&pkg.display(), &e])
    })?;
    // 窗口大小写在包自己的帧头里：不设上限，一个 20 MiB 的包就能让解压器申请
    // 几 GiB。这一道与「解压后总字节」分开，管的正是解压器要占多少内存。
    decoder.window_log_max(window_log).map_err(|e| {
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
        // 包里每一项都是要执行的东西（清单除外，设了也无害）。文件是逐字节写出来的，
        // 权限得自己给：少了可执行位，拉起就是 EACCES，而清单与文件集合全都是对的，
        // 排查时最容易看错方向。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).map_err(|e| {
                mesh_text::format(id::LAUNCHER_ERROR_CREATE_FILE, &[&target.display(), &e])
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
/// 装包、选槽、手动装后的拉起都走这一条：三处各自组合检查项，迟早有一处漏。
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

/// 待装包的整包摘要：文件不在就是「没有摘要的手动路径」，只有包内清单可验。
///
/// 内容是客户端验过发布签名的那一份摘要。文件在但读不出来是明确的失败——写坏的
/// 交接不该悄悄退化成只验清单；空文件同理，比对时必然不符。
#[cfg(unix)]
fn pending_digest(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(mesh_text::format(
            id::LAUNCHER_ERROR_READ_FILE,
            &[&path.display(), &e],
        )),
    }
}

/// 一个槽是否有效：清单验签通过，且目录与清单是**同一个集合**。
///
/// 只核对清单列出的文件不够：签名授权的是一个文件集合，不是一份白名单。目录里
/// 多出来的文件同样会被加载（Windows 的 DLL 搜索、Linux 的 `LD_LIBRARY_PATH` 相邻
/// 目录），所以多一个也是「安装根被人动过」。`version.json` 是唯一例外——它自己
/// 写不进自己的摘要，靠 `signature` 保护。
fn verify_slot(dir: &Path, public_hex: &str) -> Result<Manifest, String> {
    let manifest = manifest::read(&dir.join(layout::MANIFEST)).map_err(|err| err.to_string())?;
    check_manifest(&manifest, public_hex)?;
    verify(dir, &manifest)?;
    Ok(manifest)
}

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
    // 同样会被加载，多一个也算「安装根被人动过」，少一个就是装残了。
    // `version.json` 是唯一例外：它写不进自己的摘要，但类型要跟其余文件一样。
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

/// 包、或槽里某个文件的完整摘要。读不动时带上路径与系统错误：两条调用路径都要
/// 说清是哪个文件、为什么读不了。
fn sha256_file(path: &Path) -> Result<String, String> {
    artifact::digest(path)
        .map_err(|e| mesh_text::format(id::LAUNCHER_ERROR_READ_FILE, &[&path.display(), &e]))
}

/// wait_current 等客户端提交这个槽。进程提前退出是「装上就崩」，越早知道越好；
/// 两个平台同一个判据：它是不是还活着由 `exited` 回答（调用方传进程句柄）。
///
/// 前提：拉起之前 `bin/current` 不得已经等于这个槽。安装路径在清空候选槽之前
/// 判过这一条（`Owner::install`），所以「看见 current 等于该槽」才等价于
/// 「这一次启动提交了它」——否则一个遗留指针会让确认在第一眼就为真。
///
/// `exited` 是唯一的可测试缝隙：确认逻辑只有「看到 current、看到进程退出、超时」
/// 三种结局，不必为了测它造一个会写文件的客户端。
fn wait_current(
    root: &Path,
    slot: &str,
    deadline: Instant,
    mut exited: impl FnMut() -> Result<Option<i32>, String>,
) -> Result<(), String> {
    loop {
        if layout::read_current(root).is_some_and(|name| name == slot) {
            return Ok(());
        }
        if let Some(code) = exited()? {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_CLIENT_EXITED_IMMEDIATELY,
                &[&code],
            ));
        }
        if Instant::now() >= deadline {
            return Err(mesh_text::format(
                id::LAUNCHER_ERROR_BOOT_TIMEOUT,
                &[&WAIT_BOOT.as_secs()],
            ));
        }
        std::thread::sleep(POLL);
    }
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
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// 小写十六进制。造摘要与签名只发生在测试里，生产代码不再需要它。
    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

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

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[5u8; 32])
    }

    fn public_hex(key: &SigningKey) -> String {
        hex(&key.verifying_key().to_bytes())
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex(&hasher.finalize())
    }

    /// 造一份签名正确的清单：摘要按文件内容算，签的是清单摘要。
    fn signed_manifest(key: &SigningKey, version: &str, entries: &[(&str, &[u8])]) -> String {
        signed_manifest_for(platform::id(), key, version, entries)
    }

    /// 同 [`signed_manifest`]，但清单声明的平台由调用方给：用来单独改这一个条件。
    fn signed_manifest_for(
        manifest_platform: &str,
        key: &SigningKey,
        version: &str,
        entries: &[(&str, &[u8])],
    ) -> String {
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

    fn required_entries() -> [(&'static str, &'static [u8]); 2] {
        [
            (platform::client_exe(), b"client"),
            (platform::core_name(), b"core"),
        ]
    }

    /// 造一个合法包：清单按内容算摘要、用测试密钥签名。
    fn valid_package(dir: &Path, version: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let key = test_key();
        package(dir, &signed_manifest(&key, version, entries), entries)
    }

    /// 在 root 里造一个有效槽：目录、文件、与它们一致的签名清单。
    fn write_slot(
        root: &Path,
        slot: &str,
        key: &SigningKey,
        version: &str,
        entries: &[(&str, &[u8])],
    ) -> PathBuf {
        let dir = layout::slot_dir(root, slot);
        fs::create_dir_all(&dir).unwrap();
        for (name, body) in entries {
            fs::write(dir.join(name), body).unwrap();
        }
        fs::write(
            dir.join(layout::MANIFEST),
            signed_manifest(key, version, entries),
        )
        .unwrap();
        dir
    }

    /// 测试里的「这一个启动器」：取到安装根写权，用测试公钥验清单。
    fn owner(root: &Path) -> Owner {
        own_with_key(root, root, &public_hex(&test_key())).unwrap()
    }

    fn set_current(root: &Path, slot: &str) {
        layout::write_current(root, slot).unwrap();
    }

    fn count_failures(root: &Path, version: &str) -> u32 {
        Failures::load(root).count(version)
    }

    /// 一个包在解包后与自己的清单不符：装得上但校验不过，正是「坏包」的形状。
    fn mismatched_package(dir: &Path, version: &str) -> PathBuf {
        let key = test_key();
        let declared = required_entries();
        let actual: [(&str, &[u8]); 2] = [
            (platform::client_exe(), b"tampered client"),
            (platform::core_name(), b"core"),
        ];
        package(dir, &signed_manifest(&key, version, &declared), &actual)
    }

    #[test]
    fn a_tampered_slot_is_refused_before_launch() {
        let root = temp_root("slot-tampered");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        // 换了二进制却没换清单：这正是「有人动过安装根」的形态。
        fs::write(
            layout::slot_dir(&root, layout::SLOT_ALICE).join(platform::client_exe()),
            b"evil",
        )
        .unwrap();
        assert!(
            owner(&root).prepare().unwrap().is_none(),
            "换掉槽里的文件就必须拒绝启动"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单之外的文件同样要被拒：签名授权的是一个文件集合，不是一份白名单。
    #[test]
    fn a_file_outside_the_manifest_is_refused() {
        let root = temp_root("slot-extra");
        let key = test_key();
        let dir = write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        fs::write(dir.join("extra.dll"), b"planted").unwrap();
        assert!(owner(&root).prepare().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单合法但不含内核：装上去也起不来，所以选槽阶段就判它不可用。
    #[test]
    fn a_manifest_without_the_core_is_refused() {
        let root = temp_root("slot-no-core");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &[(platform::client_exe(), b"client")],
        );
        assert!(owner(&root).prepare().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单与槽完全一致，但签名不是这个公钥签的：文件摘要那一关会全过，
    /// 只有验签能拦住——这正是加签名的理由。
    #[test]
    fn a_manifest_signed_by_another_key_is_refused() {
        let root = temp_root("slot-other-key");
        let stranger = SigningKey::from_bytes(&[9u8; 32]);
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &stranger,
            "2026.11.27-1",
            &required_entries(),
        );
        assert!(
            owner(&root).prepare().unwrap().is_none(),
            "别的密钥签出来的清单不能过"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 清单文件本身也在集合里：它免摘要，但类型仍要是普通文件。
    #[test]
    fn a_manifest_that_is_a_directory_is_refused() {
        let root = temp_root("slot-manifest-dir");
        let dir = layout::slot_dir(&root, layout::SLOT_ALICE);
        fs::create_dir_all(dir.join(layout::MANIFEST)).unwrap();
        for (name, body) in required_entries() {
            fs::write(dir.join(name), body).unwrap();
        }
        assert!(owner(&root).prepare().unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn selection_uses_a_valid_current_slot() {
        let root = temp_root("select-current");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        assert_eq!(selected.name, layout::SLOT_ALICE);
        assert_eq!(selected.version, "2026.11.27-1");
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        let _ = fs::remove_dir_all(&root);
    }

    /// `current` 缺失时按名字的字典序选：`bob` 优先于 `alice`，并补写 `current`。
    #[test]
    fn selection_prefers_bob_when_current_is_missing() {
        let root = temp_root("select-bob");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        write_slot(
            &root,
            layout::SLOT_BOB,
            &key,
            "2026.11.27-2",
            &required_entries(),
        );
        let selected = owner(&root).prepare().unwrap().expect("两个槽都有效");
        assert_eq!(selected.name, layout::SLOT_BOB);
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_BOB));
        // `current` 原本缺失，清理不动任何槽：另一个槽是唯一的回退材料。
        assert!(layout::slot_dir(&root, layout::SLOT_ALICE).is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    /// `current` 指向的槽无效：取另一个有效槽，并把 `current` 改写成它。
    #[test]
    fn selection_falls_back_to_the_other_slot() {
        let root = temp_root("select-fallback");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        fs::create_dir_all(layout::slot_dir(&root, layout::SLOT_BOB)).unwrap();
        fs::write(
            layout::slot_dir(&root, layout::SLOT_BOB).join("HALF"),
            b"half",
        )
        .unwrap();
        set_current(&root, layout::SLOT_BOB);
        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        assert_eq!(selected.name, layout::SLOT_ALICE);
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        // 无效的槽不是「另一个有效槽」，清理不删它。
        assert!(layout::slot_dir(&root, layout::SLOT_BOB).is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    /// 值非法的 `current` 与缺失同义：按字典序选，再修复。
    #[test]
    fn a_garbage_current_selects_by_name_and_is_repaired() {
        let root = temp_root("select-garbage");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        write_slot(
            &root,
            layout::SLOT_BOB,
            &key,
            "2026.11.27-2",
            &required_entries(),
        );
        fs::write(layout::current_path(&root), "carol\n").unwrap();
        let selected = owner(&root).prepare().unwrap().expect("bob 有效");
        assert_eq!(selected.name, layout::SLOT_BOB);
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_BOB));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_bootable_slot_when_both_are_invalid() {
        let root = temp_root("select-none");
        assert!(owner(&root).prepare().unwrap().is_none());
        assert!(
            !layout::current_path(&root).exists(),
            "没有可启动版本时不该写 current"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 选槽不删任何槽：启动器分不清「上一次提交留下的旧槽」与「这一次刚装好、
    /// 还没确认的候选槽」，按「另一个槽有效就删」去收拾，会把失败的那次安装从盘上抹掉
    /// （真机上就是这么把候选槽删掉、然后永远重试旧版本的）。
    #[test]
    fn selection_never_removes_a_slot() {
        let root = temp_root("select-keeps");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        write_slot(
            &root,
            layout::SLOT_BOB,
            &key,
            "2026.11.27-2",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        assert_eq!(selected.name, layout::SLOT_ALICE);
        assert!(
            layout::slot_dir(&root, layout::SLOT_BOB).is_dir(),
            "选槽不能碰另一个槽"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 清理只在确认成功之后发生：删的是已经不该再用的旧槽。
    #[test]
    fn cleanup_drops_the_old_slot_after_a_confirmed_install() {
        let root = temp_root("cleanup-after-confirm");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        write_slot(
            &root,
            layout::SLOT_BOB,
            &key,
            "2026.11.27-2",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_BOB);
        owner(&root).drop_old_slot(layout::SLOT_BOB);
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        // 旧槽本来就不在时是无副作用的 no-op。
        owner(&root).drop_old_slot(layout::SLOT_BOB);
        assert!(layout::slot_dir(&root, layout::SLOT_BOB).is_dir());
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn a_candidate_install_leaves_the_current_slot_alone() {
        let root = temp_root("install-candidate");
        let src = temp_root("install-candidate-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let pkg = valid_package(&src, "2026.11.27-2", &required_entries());

        let installed = owner(&root)
            .install(&pkg, layout::SLOT_BOB, Some("2026.11.27-1"), false)
            .unwrap();

        assert_eq!(installed.name, layout::SLOT_BOB);
        assert_eq!(installed.version, "2026.11.27-2");
        assert!(layout::slot_dir(&root, layout::SLOT_BOB).is_dir());
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        assert!(layout::slot_dir(&root, layout::SLOT_ALICE)
            .join(platform::client_exe())
            .is_file());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 客户端提交之后，下一次启动把新槽当作当前槽；旧槽留到确认那一刻才删。
    #[test]
    fn a_committed_candidate_becomes_current_on_the_next_start() {
        let root = temp_root("commit-next-start");
        let src = temp_root("commit-next-start-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let pkg = valid_package(&src, "2026.11.27-2", &required_entries());
        let installed = owner(&root)
            .install(&pkg, layout::SLOT_BOB, Some("2026.11.27-1"), false)
            .unwrap();
        // 这是客户端在启动成功时做的唯一一次写：父目录是 bob。
        set_current(&root, installed.name);

        let selected = owner(&root).prepare().unwrap().expect("bob 有效");
        assert_eq!(selected.name, layout::SLOT_BOB);
        assert_eq!(selected.version, "2026.11.27-2");
        assert!(
            layout::slot_dir(&root, layout::SLOT_ALICE).is_dir(),
            "选槽只挑当前槽，删旧槽是确认那一步的事"
        );
        owner(&root).drop_old_slot(layout::SLOT_BOB);
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn a_first_install_goes_to_alice() {
        let root = temp_root("install-first");
        let src = temp_root("install-first-src");
        let installed = owner(&root)
            .install(
                &valid_package(&src, "2026.11.27-1", &required_entries()),
                candidate(None),
                None,
                false,
            )
            .unwrap();
        assert_eq!(installed.name, layout::SLOT_ALICE);
        assert!(layout::slot_dir(&root, layout::SLOT_ALICE)
            .join(layout::MANIFEST)
            .is_file());
        assert!(!layout::current_path(&root).exists(), "启动确认之前不提交");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn a_package_that_is_not_newer_is_refused_before_touching_the_candidate() {
        let root = temp_root("install-not-newer");
        let src = temp_root("install-not-newer-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-2",
            &required_entries(),
        );
        let pkg = valid_package(&src, "2026.11.27-1", &required_entries());

        let err = owner(&root)
            .install(&pkg, layout::SLOT_BOB, Some("2026.11.27-2"), false)
            .unwrap_err();
        assert!(err.contains("不比"), "错误里要说明为什么拒绝：{err}");
        assert!(!layout::slot_dir(&root, layout::SLOT_BOB).exists());
        assert!(layout::slot_dir(&root, layout::SLOT_ALICE).is_dir());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 到上限的版本在解包之前就被跳过，候选槽不动。
    #[test]
    fn a_version_that_already_failed_is_skipped_before_unpacking() {
        let root = temp_root("install-exhausted");
        let src = temp_root("install-exhausted-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        let mut failures = Failures::load(&root);
        failures.record("2026.11.27-2");
        failures.record("2026.11.27-2");
        let pkg = valid_package(&src, "2026.11.27-2", &required_entries());

        let err = owner(&root)
            .install(&pkg, layout::SLOT_BOB, Some("2026.11.27-1"), false)
            .unwrap_err();
        assert!(err.contains("失败"), "错误里要说明跳过原因：{err}");
        assert!(!layout::slot_dir(&root, layout::SLOT_BOB).exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    #[test]
    fn a_package_without_a_signature_is_refused_before_touching_the_candidate() {
        let root = temp_root("install-unsigned");
        let src = temp_root("install-unsigned-src");
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
            owner(&root)
                .install(&pkg, layout::SLOT_ALICE, None, true)
                .is_err(),
            "缺签名必须拒绝"
        );
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 交接给的摘要不符：拒绝发生在清空候选槽之前，用户给的包不删。
    #[test]
    fn a_manual_install_with_a_mismatched_digest_is_refused() {
        let root = temp_root("install-manual-digest");
        let src = temp_root("install-manual-digest-src");
        let pkg = valid_package(&src, "2026.11.27-1", &required_entries());

        let err = run(
            &root,
            &root,
            Source::File(pkg.clone()),
            Some("0".repeat(64)),
            None,
            true,
        )
        .unwrap_err();

        assert!(err.contains("摘要"), "拒绝原因要指向摘要：{err}");
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        assert!(pkg.is_file(), "用户给的包不能被删");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 清单是别的平台的：签名完全正确，只有 slot/platform 这一项不合本机。
    #[test]
    fn a_package_for_another_platform_is_refused_before_touching_the_candidate() {
        let root = temp_root("install-other-platform");
        let src = temp_root("install-other-platform-src");
        let key = test_key();
        let entries = required_entries();
        let other = if platform::id() == "linux/amd64" {
            "windows/amd64"
        } else {
            "linux/amd64"
        };
        let manifest = signed_manifest_for(other, &key, "2026.11.27-1", &entries);
        let pkg = package(&src, &manifest, &entries);

        assert!(owner(&root)
            .install(&pkg, layout::SLOT_ALICE, None, true)
            .is_err());
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 坏包：解包出来与清单不符。丢弃候选槽、不记失败次数。
    #[test]
    fn a_bad_package_is_discarded_without_counting_a_failure() {
        let root = temp_root("install-bad-discard");
        let src = temp_root("install-bad-discard-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        let pkg = mismatched_package(&src, "2026.11.27-2");
        assert!(owner(&root)
            .install(&pkg, layout::SLOT_BOB, Some("2026.11.27-1"), false)
            .is_err());
        assert!(!layout::slot_dir(&root, layout::SLOT_BOB).exists());
        assert_eq!(count_failures(&root, "2026.11.27-2"), 0);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 解包出来的每一项都要可执行：少了可执行位，拉起就是 EACCES，而清单与文件集合
    /// 全都是"对"的，排查时最容易看错方向。
    #[cfg(unix)]
    #[test]
    fn unpacked_files_are_executable() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("unpack-mode");
        let src = temp_root("unpack-mode-src");
        let pkg = valid_package(&src, "2026.11.27-1", &required_entries());
        owner(&root)
            .install(&pkg, layout::SLOT_ALICE, None, true)
            .unwrap();
        let exe = layout::slot_dir(&root, layout::SLOT_ALICE).join(platform::client_exe());
        let mode = fs::metadata(&exe).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "解包出来的客户端必须可执行");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 坏包：解包出来与清单不符。丢弃候选槽、删掉待装包、不记失败次数。
    #[cfg(unix)]
    #[test]
    fn a_bad_package_is_discarded_and_the_pending_package_deleted() {
        let root = temp_root("install-bad-package");
        let src = temp_root("install-bad-package-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let pkg = mismatched_package(&src, "2026.11.27-2");
        let pending = root.join(UPDATE_FILE);
        fs::copy(&pkg, &pending).unwrap();

        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        assert!(
            owner(&root).apply_pending(Some(&selected)).is_none(),
            "坏包装不进候选槽"
        );
        assert!(
            !layout::slot_dir(&root, layout::SLOT_BOB).exists(),
            "坏包不能留下半包"
        );
        assert!(!pending.exists(), "消耗它的安装事务要删掉待装包");
        assert_eq!(
            count_failures(&root, "2026.11.27-2"),
            0,
            "候选槽没有有效清单时不按版本计次"
        );
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 安装根只读时，普通启动照样选出槽：`install.lock` 在数据根，`bin/current`
    /// 修不了只记一行，不该把启动打死。
    #[cfg(unix)]
    #[test]
    fn a_read_only_install_root_still_selects_a_slot() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("readonly-root");
        let data = temp_root("readonly-data");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
        let owner = own_with_key(&root, &data, &public_hex(&test_key())).unwrap();
        let selected = owner.prepare().unwrap().expect("alice 有效");
        assert_eq!(selected.name, layout::SLOT_ALICE);
        assert_eq!(layout::read_current(&root), None, "写不进去就保持原样");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&data);
    }

    /// 装包路径要求安装根可写：写不了时说「请以管理员身份运行」，不给一句「拒绝访问」。
    #[cfg(unix)]
    #[test]
    fn installing_into_a_read_only_root_asks_for_admin() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("readonly-install");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
        let err = ensure_install_root_writable(&root).unwrap_err();
        assert!(err.contains("管理员"), "提示要能照做：{err}");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// 交接包被迁移搬到数据根之后，命令行上的旧路径要能落到新位置。
    #[test]
    fn the_handoff_package_is_found_after_the_state_moved() {
        let root = temp_root("handoff-moved");
        let data = temp_root("handoff-moved-data");
        fs::write(data.join(UPDATE_FILE), b"pkg").unwrap();
        let given = root.join(UPDATE_FILE);
        assert_eq!(
            resolve_package(given.clone(), &data),
            data.join(UPDATE_FILE)
        );
        // 给了不存在的路径、数据根也没有：原样返回，让调用方报「文件不存在」。
        let missing_data = temp_root("handoff-missing");
        assert_eq!(resolve_package(given.clone(), &missing_data), given);
        // 路径本身还在（新客户端写在数据根）：直接用。
        let direct = data.join(UPDATE_FILE);
        assert_eq!(resolve_package(direct.clone(), &data), direct);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&data);
        let _ = fs::remove_dir_all(&missing_data);
    }

    /// 启动确认失败：候选槽保留、current 不动、按版本记一次失败。
    #[test]
    fn a_boot_failure_keeps_the_candidate_and_counts_by_version() {
        let root = temp_root("boot-failure");
        let src = temp_root("boot-failure-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let owner = owner(&root);
        let installed = owner
            .install(
                &valid_package(&src, "2026.11.27-2", &required_entries()),
                layout::SLOT_BOB,
                Some("2026.11.27-1"),
                false,
            )
            .unwrap();

        let failure = owner.candidate_failed(&installed, "没确认".to_string(), Ok(()));

        assert!(matches!(failure, BootFailure::Confirm(_)), "{failure:?}");
        assert!(
            layout::slot_dir(&root, layout::SLOT_BOB).is_dir(),
            "失败路径不删候选槽"
        );
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        assert_eq!(count_failures(&root, "2026.11.27-2"), 1);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 停不掉候选槽时不计数：它可能还活着，「起不来」还没被证明；结局必须是
    /// 「停止未确认」，调用方据此不回退。
    #[test]
    fn a_candidate_that_cannot_be_stopped_is_not_counted() {
        let root = temp_root("boot-stop-failed");
        let installed = Selected {
            name: layout::SLOT_BOB,
            version: "2026.11.27-2".to_string(),
        };
        let failure = owner(&root).candidate_failed(
            &installed,
            "没确认".to_string(),
            Err("still running".to_string()),
        );
        assert!(matches!(failure, BootFailure::NotStopped(_)), "{failure:?}");
        assert_eq!(count_failures(&root, "2026.11.27-2"), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_current_confirms_when_current_becomes_the_slot() {
        let root = temp_root("wait-current");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        let mut polls = 0;
        let outcome = wait_current(
            &root,
            layout::SLOT_BOB,
            Instant::now() + Duration::from_secs(5),
            || {
                polls += 1;
                if polls == 2 {
                    set_current(&root, layout::SLOT_BOB);
                }
                Ok(None)
            },
        );
        assert!(outcome.is_ok());
        assert_eq!(polls, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_current_fails_when_the_client_exits() {
        let root = temp_root("wait-exit");
        let outcome = wait_current(
            &root,
            layout::SLOT_ALICE,
            Instant::now() + Duration::from_secs(5),
            || Ok(Some(3)),
        );
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().contains('3'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_current_fails_at_the_deadline() {
        let root = temp_root("wait-deadline");
        let outcome = wait_current(
            &root,
            layout::SLOT_ALICE,
            Instant::now() - Duration::from_secs(1),
            || Ok(None),
        );
        assert!(outcome.is_err());
        let _ = fs::remove_dir_all(&root);
    }

    /// 安装失败不能删用户给的文件：--file 收的是用户自己的包。
    #[test]
    fn a_failed_manual_install_keeps_the_package_the_user_gave() {
        let root = temp_root("keep-package");
        let src = temp_root("keep-package-src");
        let pkg = package(
            &src,
            r#"{"version":"2099.01.01-1","files":{"mesh-client.exe":"00"}}"#,
            &[("mesh-client.exe", b"not the bytes the manifest claims")],
        );

        assert!(
            run(&root, &root, Source::File(pkg.clone()), None, None, true).is_err(),
            "坏包应当安装失败"
        );
        assert!(pkg.is_file(), "用户给的包被删掉了: {}", pkg.display());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 安装根只有一个写者。
    #[test]
    fn a_second_launcher_cannot_touch_the_install_root() {
        let root = temp_root("ownership");
        let src = temp_root("ownership-src");
        let first = owner(&root);
        assert!(
            own(&root, &root).is_err(),
            "安装根已经有一个写者，第二个必须失败"
        );
        assert!(run(
            &root,
            &root,
            Source::File(package(
                &src,
                r#"{"version":"2099.01.01-1","files":{}}"#,
                &[]
            )),
            None,
            None,
            true
        )
        .is_err());
        drop(first);
        assert!(own(&root, &root).is_ok(), "第一个写者退出后应当能让位");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：条目数超限的包在动候选槽之前就被拒绝。
    #[test]
    fn a_package_with_too_many_entries_is_refused() {
        let root = temp_root("budget-entries");
        let src = temp_root("budget-entries-src");
        let key = test_key();
        // 1025 个条目：多一个就超限。
        let names: Vec<String> = (0..1025).map(|i| format!("f{i}")).collect();
        let entries: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), b"x".as_slice()))
            .collect();
        let manifest = signed_manifest(&key, "2026.11.27-6", &required_entries());
        let pkg = package(&src, &manifest, &entries);

        assert!(
            read_manifest_with(&pkg, Budget::package()).is_err(),
            "条目数超限必须拒绝"
        );
        assert!(owner(&root)
            .install(&pkg, layout::SLOT_ALICE, None, true)
            .is_err());
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：清单字节数有上限。
    #[test]
    fn a_package_with_an_oversized_manifest_is_refused() {
        let root = temp_root("budget-manifest");
        let src = temp_root("budget-manifest-src");
        let key = test_key();
        let mut manifest = signed_manifest(&key, "2026.11.27-7", &required_entries());
        manifest.push_str(&" ".repeat(70 * 1024));
        let pkg = package(&src, &manifest, &required_entries());

        assert!(owner(&root)
            .install(&pkg, layout::SLOT_ALICE, None, true)
            .is_err());
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// F7：声明的解压后总量超限，在读清单这一步就拒绝。
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

    /// F7：下载先落 `.part`。超预算时半截文件不留下，正式名也不会出现。
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

    /// F7：包里的条目名必须是单层普通文件名。路径穿越由这一条判据挡下。
    #[test]
    fn a_package_with_a_traversing_entry_is_refused() {
        let root = temp_root("budget-traversal");
        let src = temp_root("budget-traversal-src");
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
            .install(&path, layout::SLOT_ALICE, None, true)
            .is_err());
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE)
            .join("sub")
            .exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 两个槽都无效时，指向其中的 current 会被下一次安装当成自己的候选槽：
    /// 首次安装固定进 alice，那个遗留值就成了「已经提交过」的假证据。
    #[test]
    fn an_unbootable_install_drops_a_stale_current() {
        let root = temp_root("stale-current");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        fs::write(layout::current_path(&root), "alice\n").unwrap();

        assert!(owner(&root).prepare().unwrap().is_none());
        assert!(
            !layout::current_path(&root).exists(),
            "两个槽都无效时不能留下会与候选槽撞名的旧指针"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 同一件事的端到端形状：清掉旧指针之后，装进 alice 不再被当成「已确认」，
    /// 安装本身也不写 current。
    #[test]
    fn a_stale_current_does_not_confirm_the_next_install() {
        let root = temp_root("stale-current-install");
        let src = temp_root("stale-current-install-src");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        fs::write(layout::current_path(&root), "alice\n").unwrap();

        assert!(owner(&root).prepare().unwrap().is_none());
        let owner = owner(&root);
        let installed = owner
            .install(
                &valid_package(&src, "2026.11.27-5", &required_entries()),
                layout::SLOT_ALICE,
                None,
                true,
            )
            .unwrap();

        assert_eq!(installed.name, layout::SLOT_ALICE);
        assert_eq!(
            layout::read_current(&root),
            None,
            "安装不写 current：提交只发生在客户端启动成功之后"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 不变量：开始试运行之前 current 不得已经等于本次候选槽。这里直接造出那个
    /// 形状（current 指着一个半包），安装必须在清空候选槽之前拒绝。
    #[test]
    fn an_install_refuses_a_candidate_that_current_already_names() {
        let root = temp_root("candidate-is-current");
        let src = temp_root("candidate-is-current-src");
        fs::create_dir_all(root.join(layout::BIN)).unwrap();
        fs::write(layout::current_path(&root), "alice\n").unwrap();

        let err = owner(&root)
            .install(
                &valid_package(&src, "2026.11.27-4", &required_entries()),
                layout::SLOT_ALICE,
                None,
                true,
            )
            .unwrap_err();

        assert!(err.contains("bin/current"), "{err}");
        assert!(
            !layout::slot_dir(&root, layout::SLOT_ALICE).exists(),
            "拒绝要发生在清空候选槽之前"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 修不好指针就不开始安装：确认判据依赖「拉起之前 current 不等于候选槽」，
    /// 修不了它还把候选槽清空，等于把这次安装交给一个无法归属的确认。
    #[test]
    fn an_install_fails_when_bin_current_cannot_be_repaired() {
        let root = temp_root("repair-fails");
        let src = temp_root("repair-fails-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        // bin/current 是个目录：读不出来（当作缺失），改写必然失败。
        fs::create_dir_all(layout::current_path(&root)).unwrap();

        let err = run(
            &root,
            &root,
            Source::File(valid_package(&src, "2026.11.27-2", &required_entries())),
            None,
            None,
            true,
        )
        .unwrap_err();

        assert!(!err.is_empty());
        assert!(
            layout::current_path(&root).is_dir(),
            "不能把指针写成别的东西"
        );
        assert!(
            layout::slot_dir(&root, layout::SLOT_ALICE)
                .join(platform::client_exe())
                .is_file(),
            "修复失败必须保留有效槽"
        );
        assert!(
            !layout::slot_dir(&root, layout::SLOT_BOB).exists(),
            "修复失败不得开始安装"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// --file 给的包在求摘要之前就按文件大小拒绝：整包求摘要本身就是一次
    /// 不受长度限制的读。
    #[test]
    fn a_package_over_the_byte_budget_is_refused_before_its_digest() {
        let root = temp_root("budget-package-bytes");
        let src = temp_root("budget-package-bytes-src");
        let pkg = src.join("big.tar.zst");
        let file = File::create(&pkg).unwrap();
        file.set_len(PACKAGE_BYTES + 1).unwrap();
        drop(file);

        let err = run(&root, &root, Source::File(pkg.clone()), None, None, false).unwrap_err();

        assert!(err.contains("超过上限"), "拒绝原因要指向大小：{err}");
        assert!(!layout::slot_dir(&root, layout::SLOT_ALICE).exists());
        assert!(pkg.is_file(), "用户给的包不能被删");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 解压器的窗口预算单独生效：同一份包，窗口降到 1 KiB 就解不开。
    #[test]
    fn the_decoder_refuses_a_frame_window_over_its_budget() {
        let src = temp_root("window-src");
        let ok = temp_root("window-ok");
        let narrow = temp_root("window-narrow");
        let pkg = valid_package(&src, "2026.11.27-8", &required_entries());

        assert!(unpack_with(&pkg, &ok, Budget::package(), UNPACK_WINDOW_LOG).is_ok());
        assert!(
            unpack_with(&pkg, &narrow, Budget::package(), 10).is_err(),
            "窗口超过预算的包必须解不开"
        );
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&ok);
        let _ = fs::remove_dir_all(&narrow);
    }

    /// 进程创建失败也算这一版失败：否则同一版本会被反复拉起、反复在这里失败。
    #[test]
    fn a_candidate_that_cannot_be_started_counts_a_failure_and_keeps_current() {
        let root = temp_root("boot-create-failed");
        let src = temp_root("boot-create-failed-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let owner = owner(&root);
        // 包里的 mesh-client 只是一段文本：拉起必然失败。
        let installed = owner
            .install(
                &valid_package(&src, "2026.11.27-2", &required_entries()),
                layout::SLOT_BOB,
                Some("2026.11.27-1"),
                false,
            )
            .unwrap();

        let failure = match owner.boot_candidate(&installed) {
            Ok(_) => panic!("候选槽起不来时不该返回成功"),
            Err(failure) => failure,
        };

        assert!(matches!(failure, BootFailure::Create(_)), "{failure:?}");
        assert_eq!(count_failures(&root, "2026.11.27-2"), 1);
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        assert!(layout::slot_dir(&root, layout::SLOT_BOB).is_dir());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 交接把摘要写成 `<root>/update.tar.zst.sha256`：对得上就装，读完连摘要一起删。
    #[cfg(unix)]
    #[test]
    fn a_pending_package_with_a_matching_digest_is_installed_and_clears_it() {
        let root = temp_root("pending-digest");
        let src = temp_root("pending-digest-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let pending = root.join(UPDATE_FILE);
        fs::copy(
            valid_package(&src, "2026.11.27-2", &required_entries()),
            &pending,
        )
        .unwrap();
        let digest = root.join(layout::UPDATE_DIGEST);
        fs::write(&digest, format!("{}\n", sha256_file(&pending).unwrap())).unwrap();

        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        let installed = owner(&root)
            .apply_pending(Some(&selected))
            .expect("摘要相符就该装");

        assert_eq!(installed.name, layout::SLOT_BOB);
        assert_eq!(installed.version, "2026.11.27-2");
        assert!(!pending.exists());
        assert!(!digest.exists(), "摘要与包同生命周期");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 摘要不符：盘上的字节已经不是客户端验过的那一份，包与摘要一起丢，候选槽不动。
    #[cfg(unix)]
    #[test]
    fn a_pending_package_with_a_mismatched_digest_is_discarded_before_the_candidate() {
        let root = temp_root("pending-digest-mismatch");
        let src = temp_root("pending-digest-mismatch-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let pending = root.join(UPDATE_FILE);
        fs::copy(
            valid_package(&src, "2026.11.27-2", &required_entries()),
            &pending,
        )
        .unwrap();
        // 与包一起走过交接的摘要对不上：包换过，或者摘要不是这一份的。
        let digest = root.join(layout::UPDATE_DIGEST);
        fs::write(&digest, format!("{}\n", "0".repeat(64))).unwrap();

        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        assert!(
            owner(&root).apply_pending(Some(&selected)).is_none(),
            "摘要不符的包装不上"
        );

        assert!(
            !layout::slot_dir(&root, layout::SLOT_BOB).exists(),
            "拒绝要发生在动候选槽之前"
        );
        assert!(!pending.exists(), "包不能被下一次安装捡走");
        assert!(!digest.exists(), "摘要跟着包一起丢");
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 没有摘要文件就是手动路径：只有包内清单可验，包照样装得上。
    #[cfg(unix)]
    #[test]
    fn a_pending_package_without_a_digest_is_the_manual_path() {
        let root = temp_root("pending-no-digest");
        let src = temp_root("pending-no-digest-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let pending = root.join(UPDATE_FILE);
        fs::copy(
            valid_package(&src, "2026.11.27-2", &required_entries()),
            &pending,
        )
        .unwrap();

        let selected = owner(&root).prepare().unwrap().expect("alice 有效");
        let installed = owner(&root)
            .apply_pending(Some(&selected))
            .expect("没有摘要就是手动路径");

        assert_eq!(installed.name, layout::SLOT_BOB);
        assert!(!pending.exists());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }

    /// 客户端起来了却没提交 current：确认失败、候选槽保留、按版本计次。
    /// 用测试二进制本身当客户端：它不认识 --root，起来就退出。
    #[test]
    fn a_candidate_that_exits_without_confirming_is_counted_and_keeps_current() {
        let root = temp_root("boot-exit-failed");
        let src = temp_root("boot-exit-failed-src");
        let key = test_key();
        write_slot(
            &root,
            layout::SLOT_ALICE,
            &key,
            "2026.11.27-1",
            &required_entries(),
        );
        set_current(&root, layout::SLOT_ALICE);
        let exits_at_once = fs::read(std::env::current_exe().unwrap()).unwrap();
        let entries: [(&str, &[u8]); 2] = [
            (platform::client_exe(), exits_at_once.as_slice()),
            (platform::core_name(), b"core"),
        ];
        let owner = owner(&root);
        let installed = owner
            .install(
                &valid_package(&src, "2026.11.27-3", &entries),
                layout::SLOT_BOB,
                Some("2026.11.27-1"),
                false,
            )
            .unwrap();

        let failure = match owner.boot_candidate(&installed) {
            Ok(_) => panic!("候选槽起不来时不该返回成功"),
            Err(failure) => failure,
        };

        assert!(matches!(failure, BootFailure::Confirm(_)), "{failure:?}");
        assert_eq!(count_failures(&root, "2026.11.27-3"), 1);
        assert_eq!(layout::read_current(&root), Some(layout::SLOT_ALICE));
        assert!(layout::slot_dir(&root, layout::SLOT_BOB).is_dir());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&src);
    }
}
