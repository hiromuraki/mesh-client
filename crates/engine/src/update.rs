//! 客户端自更新：一份已验签的 offer、一个已校验的工件、一个安装意图。
//!
//! 三个问题各有唯一表示，不再用 `Ready` 一词同时表示它们：
//!
//! - 手上是哪一份发布：`offer`（版本 + 摘要 + 策略）；
//! - 本地工件准备好没有：`artifact`（没有 / 已校验暂存 / 已提交待交接 / 失败）；
//! - 谁授权安装：`intent`（无 / 手动 / 自动）。意图只活到 offer 变化为止，
//!   因此它天然只指向当前这一份，不需要再存一份 offer 身份副本。
//!
//! 固定路径 `<root>/update.tar.zst` 仍是「请安装」的唯一信号，但只有真正要交接时
//! 才从暂存件改名过去；拿不到安装根写权只延期提交，已校验的暂存件不重下。
//!
//! 「工件已就绪」与「安装事务已接收」是两件事：改名成固定名只说明前者，拉起启动器
//! 成功才说明后者。交接没被接收时工件与授权都留着，下一轮心跳只重试交接——已校验的
//! 字节不重下，自动尝试标记也不被消耗。

use crate::log::Log;
use crate::run::Engine;
use crate::Cmd;
use mesh_install::{artifact, layout, version};
use mesh_remote as remote;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// 更新包上限，和启动器里的 200MB 对齐。
pub(crate) const MAX_UPDATE_BYTES: u64 = 200 << 20;
/// 下载更新包的总超时（Go 是 20 分钟）。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// 下载启动器的总超时。
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 下载失败后自动重试的次数上限。用完就停下来等人处理，别把日志和发布文件服务
/// 一起刷满。
pub(crate) const MAX_DOWNLOAD_ATTEMPTS: u32 = 5;
/// 启动器替换用的临时目录（Go 也叫 `update/`）。
const UPDATE_DIR: &str = "update";

/// 界面上的更新状态。这是 [`UpdateState`] 的投影，不是另存的一份进度：
/// 没有 offer 即 `Idle`，在下载即 `Downloading`，工件失败即 `Failed`，其余 `Ready`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    #[default]
    Idle,
    Downloading,
    Ready,
    Failed,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "",
            Phase::Downloading => "downloading",
            Phase::Ready => "ready",
            Phase::Failed => "error",
        }
    }
}

/// 一份已经验过签的发布。
#[derive(Debug, Clone)]
pub(crate) struct Offer {
    pub version: String,
    pub url: String,
    pub notes: String,
    pub sha256: String,
    /// 当前策略是否允许自动安装：服务端开关与本机否决合起来的结果。
    pub auto: bool,
    /// 这份 offer 已经自动尝试过：每份只自动试一次。
    pub auto_tried: bool,
}

/// 本地工件的准备状态。
#[derive(Debug, Clone, Default)]
pub(crate) enum Artifact {
    /// 还没有可用的本地包。
    #[default]
    None,
    /// 已下载并校验，暂存在 `.part`：等安装根写权或等授权。
    Staged(PathBuf),
    /// 已提交成固定名 `update.tar.zst`：安装信号已就位，等交接被接收。
    ///
    /// `error` 是最近一次交接没被接收的原因，空串表示这一轮还没试过。工件本身保持
    /// 可用：下一轮心跳只重试交接，不重下。
    Committed { error: String },
    /// 本份 offer 最近一次下载失败。失败次数不在这里：它属于整份 offer
    /// （[`UpdateState::attempts`]），要活过「起任务 → 失败 → 重试」整段。
    Failed { error: String, retry_at: Instant },
}

/// 谁授权安装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Intent {
    Manual,
    Auto,
}

/// 自更新的全部状态：四个正交的部分，没有一个是另一个的别名。
#[derive(Default)]
pub(crate) struct UpdateState {
    /// 手上这份已验签的发布；`None` 即没有。
    pub offer: Option<Offer>,
    /// 正在跑的下载任务。任何时刻最多一个：两个线程写同一个 `.part` 只会互相截断。
    pub task: Option<DownloadTask>,
    pub artifact: Artifact,
    /// 本份 offer 已经连续失败了几次下载。
    ///
    /// 它必须活过「起任务 → 失败 → 重试」整段：起任务时清掉它，连续失败就永远只剩
    /// 第 1 次，既到不了上限，退避也回不到更大的档。只有手动重试与换 offer 才清零。
    pub attempts: u32,
    /// 当前 offer 的安装授权；offer 变化时随之清空。
    pub intent: Option<Intent>,
}

impl UpdateState {
    /// 界面上的进度。
    pub(crate) fn phase(&self) -> Phase {
        match (&self.offer, &self.artifact) {
            (None, _) => Phase::Idle,
            (Some(_), _) if self.task.is_some() => Phase::Downloading,
            (Some(_), Artifact::Failed { .. }) => Phase::Failed,
            // 包已就绪、安装事务没接收：失败要给人看，工件仍可重试。
            (Some(_), Artifact::Committed { error }) if !error.is_empty() => Phase::Failed,
            (Some(_), _) => Phase::Ready,
        }
    }

    /// 界面上要显示的错误行。
    pub(crate) fn error(&self) -> &str {
        match &self.artifact {
            Artifact::Failed { error, .. } | Artifact::Committed { error } => error,
            _ => "",
        }
    }

    /// 撤回当前 offer：意图、失败次数与工件一起作废。在跑的任务留着——线程取消
    /// 不了，它的完成消息回来时身份已经对不上，只会清掉自己的临时文件。
    fn withdraw(&mut self, root: &Path) {
        self.offer = None;
        self.intent = None;
        self.attempts = 0;
        self.discard_artifact(root);
    }

    /// 丢掉手上的工件：只删引擎自己创建、安装事务还没接手的那一份。
    fn discard_artifact(&mut self, root: &Path) {
        match std::mem::take(&mut self.artifact) {
            Artifact::Staged(part) => {
                let _ = std::fs::remove_file(part);
            }
            // 交接没被接收时，固定路径上的包与它的摘要都是引擎自己放的：撤回要把
            // 它们一起撤掉，否则下一轮安装事务会装一个已经不再被授权的版本。
            Artifact::Committed { .. } => {
                let _ = std::fs::remove_file(root.join(layout::UPDATE_FILE));
                let _ = std::fs::remove_file(root.join(layout::UPDATE_DIGEST));
            }
            Artifact::None | Artifact::Failed { .. } => {}
        }
        self.artifact = Artifact::None;
    }
}

/// 一次下载任务的标识。
///
/// `gen` 是代次，不是 offer 身份：同一份 offer 重下、或者下载中换了 offer，都会
/// 起一个不同的任务。完成消息带着它回来，引擎据此判断「这条结果还算数吗」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadTask {
    pub gen: u64,
    pub version: String,
    pub sha256: String,
}

/// 启动器槽位的本地状态。
///
/// 与 [`UpdateState`] 分开：那份状态会随客户端包 offer 整体重建，而启动器的
/// 已知摘要、结论与在途任务要活到进程结束。任务本身就是单飞判据，不再另设
/// 原子占位——同一时刻只可能有一个任务在跑。
#[derive(Default)]
pub(crate) struct LauncherState {
    /// 安装根下这份启动器的完整摘要。
    pub installed_sum: String,
    /// 盘上这份启动器自报的版本号；读不到就是 None，那时不判降级。
    pub installed_version: Option<String>,
    /// 本进程对哪一份 offer 已经有结论（换掉了，或判定不该换）：同一份不再重试。
    pub tried: Option<String>,
    /// 正在跑的替换任务。
    pub task: Option<DownloadTask>,
}

impl LauncherState {
    /// 启动时读一次盘上的事实：安装根里那份启动器的摘要，与它自报的版本号。
    ///
    /// 只在这一个地方读：之后由替换结果更新（[`Engine::on_launcher_result`]）。
    /// 记下的是「盘上那份」，不是正在跑的那一份——两者只在没有替换过时相同。
    pub(crate) fn read(root: &Path) -> LauncherState {
        LauncherState {
            installed_sum: artifact::digest_file(
                &root.join(mesh_install::platform::launcher_name()),
            )
            .unwrap_or_default(),
            installed_version: layout::read_launcher_version(root),
            ..LauncherState::default()
        }
    }
}

/// 一次启动器替换的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherResult {
    /// 盘上那份已经换成下发的这一份。
    Replaced,
    /// 安装根被占着：不是结论，下一轮心跳再来。
    Deferred,
    /// 有结论的失败：同一份不再重试。
    Failed,
}

impl Engine {
    /// 记下服务端报告的发布。安装动作交给启动器。
    ///
    /// 服务端不再提供（`None`）就是撤回：显式迁移状态，不让旧任务的结果续命。
    pub(crate) fn offer_update(&mut self, info: Option<&remote::UpdateInfo>) {
        let Some(info) = info else {
            self.clear_stale_update();
            return;
        };
        let auto = info.auto_update && !self.config.auto_update_disabled;
        // 版本号会被拼进安装目录里的路径，先按 layout 的规则挡掉非法值。
        if !version::valid(&info.version) {
            let shown = format!("{:?}", info.version);
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_IGNORE_INVALID_VERSION, &[&shown]),
            );
            return;
        }
        // 降级防护：发布记录被指回旧包时不要跟着往回装。
        if !self.config.self_version.is_empty()
            && !version::is_newer(&info.version, &self.config.self_version)
        {
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_IGNORE_NOT_NEWER,
                    &[&info.version, &self.config.self_version],
                ),
            );
            return;
        }
        let failures = mesh_install::failures::Failures::load(&self.config.data);
        if failures.exhausted(&info.version) {
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_SKIP_FAILED_VERSION,
                    &[&info.version, &failures.count(&info.version)],
                ),
            );
            return;
        }
        // 契约里包的地址一律是绝对地址；不是就记一句，不去猜。
        if !(info.url.starts_with("http://") || info.url.starts_with("https://")) {
            let shown = format!("{:?}", info.url);
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_UPDATE_URL_NOT_ABSOLUTE, &[&shown]),
            );
            return;
        }
        // 摘要必填：没有摘要就证明不了下载到的包就是发布的那一份，这种 offer 不接。
        if !artifact::is_full_digest(&info.sha256) {
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_IGNORE_NO_DIGEST, &[&info.version]),
            );
            return;
        }
        // 摘要只证明内容没被换过，签名才证明它出自发布方。
        if !remote::signing::verify_release_with(
            &self.release_key,
            remote::signing::SLOT_CLIENT,
            mesh_install::platform::id(),
            &info.version,
            &info.sha256,
            &info.signature,
        ) {
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_UPDATE_SIGNATURE_INVALID,
                    &[&info.version],
                ),
            );
            return;
        }
        // 判重只有这里：服务端每轮心跳都带完整的一份，「这份我已经验过」由版本与
        // 摘要决定——与下载进度无关，下载中、失败、已暂存都还是同一份，不重复下载。
        let same = self.update.offer.as_ref().is_some_and(|offer| {
            offer.version == info.version && offer.sha256.eq_ignore_ascii_case(&info.sha256)
        });
        if same {
            // 同一份工件就是同一份 offer：策略、地址与说明都按最新下发值刷新——
            // 发布方搬迁了地址，客户端还按旧地址重试就永远下不到。已暂存件与在跑的
            // 任务不受影响：它们按版本与摘要认这一份，与地址无关。
            if let Some(offer) = self.update.offer.as_mut() {
                offer.url = info.url.clone();
                offer.notes = info.notes.clone();
                offer.auto = auto;
            }
            return;
        }
        self.start_offer(info, auto);
    }

    /// 换上一份新的 offer。撤回、同版本重发与换版本走的是同一条状态迁移。
    fn start_offer(&mut self, info: &remote::UpdateInfo, auto: bool) {
        // 上一份 offer 的工件不再被授权：暂存件与未接收的固定包一起作废。
        // 在跑的下载任务留在原地：`.part` 只有一个，再起一个只会互相截断。
        let data = self.config.data.clone();
        self.update.discard_artifact(&data);
        self.update.attempts = 0;
        self.update.intent = None;
        self.update.offer = Some(Offer {
            version: info.version.clone(),
            url: info.url.clone(),
            notes: info.notes.clone(),
            sha256: info.sha256.clone(),
            auto,
            auto_tried: false,
        });
        if cfg!(windows) {
            // 安装发生在客户端退出之后，那时已经断网：先把字节抓到暂存区。
            self.prepare();
        } else {
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_UPDATE_READY_MANUAL,
                    &[&info.version],
                ),
            );
        }
    }

    /// 控制面不再提供这份 offer（撤回）：把 offer 与授权显式清掉。
    pub(crate) fn clear_stale_update(&mut self) {
        let data = self.config.data.clone();
        self.update.withdraw(&data);
    }

    /// 用户点了“安装”（或面板上的重试）。
    pub(crate) fn apply_update(&mut self) -> Result<(), String> {
        if self.update.offer.is_none() {
            return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_NOTHING_TO_INSTALL).to_string());
        }
        // 用户主动重试：失败退避作废，次数从第一档重新记。
        if matches!(self.update.artifact, Artifact::Failed { .. }) {
            self.update.artifact = Artifact::None;
            self.update.attempts = 0;
        }
        self.update.intent = Some(Intent::Manual);
        self.try_apply()
    }

    /// 无人值守：策略允许就给这份 offer 记一次自动意图。每份只记一次。
    pub(crate) fn auto_apply(&mut self) {
        let Some(offer) = self.update.offer.as_mut() else {
            return;
        };
        if !offer.auto || offer.auto_tried || self.update.intent.is_some() {
            return;
        }
        offer.auto_tried = true;
        let version = offer.version.clone();
        self.update.intent = Some(Intent::Auto);
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_AUTO_APPLY, &[&version]),
        );
    }

    /// 推进更新：心跳与下载完成都只调这一个入口。
    ///
    /// 1. 自动策略到期就记下自动意图；
    /// 2. 有意图且工件就绪就交接；
    /// 3. 没有意图时按需准备工件（Windows 预取、失败退避重试）。
    pub(crate) fn advance_update(&mut self) {
        self.auto_apply();
        if self.update.intent.is_some() {
            if let Err(err) = self.try_apply() {
                self.logf(
                    "app",
                    &mesh_text::format(mesh_text::id::ENGINE_LOG_AUTO_APPLY_FAILED, &[&err]),
                );
            }
        } else {
            self.prepare();
        }
    }

    /// 有意图时把已验工件交接给启动器；没有已验工件就先准备。
    ///
    /// 「工件已就绪」与「安装事务已接收」在这里分开：改名成固定名只说明前者，
    /// 拉起启动器成功才说明后者。交接没被接收时工件与授权原样留着，下一轮心跳只
    /// 重试交接——已校验的字节不重下。
    fn try_apply(&mut self) -> Result<(), String> {
        let Some(intent) = self.update.intent else {
            return Ok(());
        };
        let Some(offer) = self.update.offer.as_ref() else {
            self.update.intent = None;
            return Ok(());
        };
        // 自动意图在交接前重新校验当前策略；手动意图只看 offer 还在不在。
        if matches!(intent, Intent::Auto) && !offer.auto {
            self.update.intent = None;
            return Ok(());
        }
        // 启动器替换还没结束：这一轮要拉起的正是那个正在被换掉的文件。延期交接，
        // 决策留在引擎里，不去猜两把锁的先后。要下载的分支不受影响。
        let ready = matches!(
            self.update.artifact,
            Artifact::Staged(_) | Artifact::Committed { .. }
        );
        if ready && self.launcher.task.is_some() {
            return Ok(());
        }
        let version = offer.version.clone();
        let sha256 = offer.sha256.clone();
        let committed = match self.update.artifact.clone() {
            Artifact::Staged(part) => {
                // 拿不到安装根写权：延期提交，已验工件留在暂存区，绝不重下。
                let Some(owner) = self.own_install_root() else {
                    return Ok(());
                };
                let committed = match commit_package(&part, &self.config.data, &sha256) {
                    Ok(path) => path,
                    Err(err) => {
                        drop(owner);
                        let _ = std::fs::remove_file(&part);
                        self.update.artifact = Artifact::Failed {
                            error: err.clone(),
                            retry_at: Instant::now(),
                        };
                        self.update.intent = None;
                        self.logf("app", &err);
                        return Err(err);
                    }
                };
                drop(owner);
                // 工件已就绪：字节在固定路径上，从这一刻起不再属于暂存区。
                self.update.artifact = Artifact::Committed {
                    error: String::new(),
                };
                self.logf(
                    "app",
                    &mesh_text::format(
                        mesh_text::id::ENGINE_LOG_DOWNLOADED_MANUAL_INSTALL,
                        &[&version],
                    ),
                );
                committed
            }
            // 上一轮已经提交过、只是交接没被接收：不再改名，直接重试交接。
            Artifact::Committed { .. } => self.config.data.join(layout::UPDATE_FILE),
            Artifact::None | Artifact::Failed { .. } => {
                self.prepare();
                return Ok(());
            }
        };
        match self.handoff(&committed, &version, &sha256) {
            Ok(()) => {
                // 安装事务已接收：工件与意图都归它，引擎不再持有；退出是交接的收尾。
                self.update.artifact = Artifact::None;
                self.update.intent = None;
                self.shutdown.run_and_exit(0)
            }
            Err(err) => {
                // 交接没被接收：保留已就绪的工件与失败事实，也不消耗自动尝试标记——
                // 下一轮心跳再来一次，包已经在盘上，不需要重下。
                self.update.artifact = Artifact::Committed { error: err.clone() };
                if let Some(offer) = self.update.offer.as_mut() {
                    offer.auto_tried = false;
                }
                Err(err)
            }
        }
    }

    /// 为当前 offer 准备本地工件：需要、且没有已验工件时才下载。
    ///
    /// 已有暂存件、已提交待交接的包（安装根被占过、上一次交接没被接收）或已有任务
    /// 在跑就直接返回——已校验的字节不重下。
    fn prepare(&mut self) {
        if self.update.task.is_some() {
            return;
        }
        let Some(offer) = self.update.offer.as_ref() else {
            return;
        };
        if matches!(
            self.update.artifact,
            Artifact::Staged(_) | Artifact::Committed { .. }
        ) {
            return;
        }
        // 没有安装意图时只有 Windows 预取需要字节；Unix 等用户点安装。
        if self.update.intent.is_none() && !cfg!(windows) {
            return;
        }
        // 退避只在失败态下成立；次数不在失败态里，起任务不会把它清掉。
        if let Artifact::Failed { retry_at, .. } = &self.update.artifact {
            if Instant::now() < *retry_at {
                return;
            }
        }
        if self.update.attempts >= MAX_DOWNLOAD_ATTEMPTS {
            return;
        }
        let Some(api) = self.api.clone() else {
            return;
        };
        let version = offer.version.clone();
        let url = offer.url.clone();
        let sha256 = offer.sha256.clone();
        self.next_task = self.next_task.wrapping_add(1);
        let task = DownloadTask {
            gen: self.next_task,
            version: version.clone(),
            sha256,
        };
        self.update.artifact = Artifact::None;
        self.update.task = Some(task.clone());
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_DOWNLOAD_STARTED, &[&version]),
        );
        let data = self.config.data.clone();
        let tx = self.self_tx.clone();
        let sent = task.clone();
        let spawned = thread::Builder::new()
            .name("mesh-download".to_string())
            .spawn(move || {
                let result = download_package(&api, &data, &url, &sent.sha256);
                let _ = tx.send(Cmd::Download { task: sent, result });
            });
        if spawned.is_err() {
            self.update.task = None;
            let message =
                mesh_text::get(mesh_text::id::ENGINE_TEXT_DOWNLOAD_THREAD_FAILED).to_string();
            self.note_download_failure(message);
        }
    }

    /// 一次下载失败：本份 offer 的失败次数加一，并按这一档安排退避。
    ///
    /// 次数记在 [`UpdateState::attempts`]，不从当前工件反推：起下载任务时工件会被
    /// 清空，反推出来的次数永远只有 1，连续失败既到不了上限也退不到更大的档。
    fn note_download_failure(&mut self, error: String) {
        self.update.attempts = self.update.attempts.saturating_add(1);
        let attempts = self.update.attempts;
        self.update.artifact = Artifact::Failed {
            error: error.clone(),
            retry_at: Instant::now() + retry_delay(attempts),
        };
        let version = self
            .update
            .offer
            .as_ref()
            .map(|offer| offer.version.clone())
            .unwrap_or_default();
        self.logf(
            "app",
            &mesh_text::format(
                mesh_text::id::ENGINE_LOG_DOWNLOAD_FAILED,
                &[&version, &attempts, &error],
            ),
        );
    }

    /// 下载线程把包写到 `.part` 后回报。
    ///
    /// 只有「这条结果仍然属于手上这份 offer、并且就是这个任务」时才接受：撤回、
    /// 同版本重发与换任务都会让旧结果失效，旧结果甚至不许留下任何文件。
    pub(crate) fn on_download(&mut self, task: DownloadTask, result: Result<PathBuf, String>) {
        if self.update.task.as_ref() != Some(&task) {
            // 不是手上这个任务（重复回报或早已作废）：只清它自己的临时文件。
            if let Ok(part) = &result {
                let _ = std::fs::remove_file(part);
            }
            return;
        }
        self.update.task = None;
        let current = self.update.offer.as_ref().is_some_and(|offer| {
            offer.version == task.version && offer.sha256.eq_ignore_ascii_case(&task.sha256)
        });
        match result {
            Ok(part) if current => {
                self.update.artifact = Artifact::Staged(part);
                let version = self
                    .update
                    .offer
                    .as_ref()
                    .map(|offer| offer.version.clone())
                    .unwrap_or_default();
                self.logf(
                    "app",
                    &mesh_text::format(
                        mesh_text::id::ENGINE_LOG_DOWNLOADED_MANUAL_INSTALL,
                        &[&version],
                    ),
                );
            }
            Ok(part) => {
                // 任务跑完时 offer 已经换了或撤回：这份内容不再被授权。
                let _ = std::fs::remove_file(part);
            }
            Err(err) if current => self.note_download_failure(err),
            Err(_) => {}
        }
        self.advance_update();
    }

    /// 把已提交的包交给启动器。返回 `Ok` 表示安装事务已被接收，调用方随即退出。
    ///
    /// 这里只回答「启动器有没有被拉起来」：拉不起来就是明确失败，工件与授权由调用方
    /// 保留重试；Windows 上客户端派生启动器之后立刻退出，安装是否真的成功由启动器
    /// 自己的确认与日志回答，客户端不再持有交接材料。
    ///
    /// 交接要带上客户端验过的那份整包摘要：包内清单只证明内容自洽，摘要才证明盘上
    /// 的字节还是被授权的那一份。Windows 走命令行；Unix 的摘要已经由
    /// [`commit_package`] 落在包旁边。
    fn handoff(&mut self, path: &Path, version: &str, sha256: &str) -> Result<(), String> {
        if cfg!(windows) {
            let launcher = self
                .config
                .root
                .join(mesh_install::platform::launcher_name());
            if !launcher.is_file() {
                let msg = mesh_text::format(
                    mesh_text::id::ENGINE_TEXT_LAUNCHER_MISSING,
                    &[&launcher.display()],
                );
                self.logf("app", &msg);
                return Err(msg);
            }
            let mut command = handoff_command(&launcher, path, sha256);
            command.current_dir(&self.config.root);
            if let Err(err) = command.spawn() {
                let msg =
                    mesh_text::format(mesh_text::id::ENGINE_TEXT_LAUNCHER_START_FAILED, &[&err]);
                self.logf("app", &msg);
                return Err(msg);
            }
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_INSTALL_STARTED, &[&version]),
            );
            // 启动器已经起来了，退出由调用方做（必须先拉起再退出，否则装不上，F7）。
            return Ok(());
        }
        // Linux：客户端只负责把包与它的摘要放到安装根，解包与启动交给常驻启动器；
        // 正常退出即可。
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_HANDOFF_LAUNCHER, &[&version]),
        );
        Ok(())
    }

    /// 按控制面公布的摘要替换安装根里的启动器（下载与替换放后台）。
    pub(crate) fn apply_launcher(
        &mut self,
        api: &Arc<remote::Client>,
        info: Option<&remote::LauncherInfo>,
    ) {
        let Some(info) = info else {
            return;
        };
        if info.url.is_empty() || !artifact::is_full_digest(&info.sha256) {
            return;
        }
        // 启动器以管理员权限替换自己，一次错误的替换要人工到场才能修好；
        // 所以这里和客户端包一样，签名不过就不动本地文件。
        if !remote::signing::verify_release_with(
            &self.release_key,
            remote::signing::SLOT_LAUNCHER,
            mesh_install::platform::id(),
            &info.version,
            &info.sha256,
            &info.signature,
        ) {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_SIGNATURE_INVALID),
            );
            return;
        }
        // 单飞、判重与占位在同一处完成：同一台机器只尝试一次，换不掉就等下个版本。
        if self.launcher.task.is_some() {
            return;
        }
        if self
            .launcher
            .installed_sum
            .eq_ignore_ascii_case(&info.sha256)
        {
            return;
        }
        if self.launcher.tried.as_deref() == Some(info.sha256.as_str()) {
            return;
        }
        // 摘要只证明「是不是同一份」，不证明「是不是更新的」：盘上这份自报的版本号
        // 是唯一的本地依据，它比下发的更新才换；没有记录时不判，放行这一次。
        if let Some(current) = self.launcher.installed_version.as_deref() {
            if !version::is_newer(&info.version, current) {
                self.logf(
                    "app",
                    &mesh_text::format(
                        mesh_text::id::ENGINE_LOG_LAUNCHER_NOT_NEWER,
                        &[&info.version, &current],
                    ),
                );
                // 记下结论：下一轮心跳不再重判同一份，也不再刷同一行日志。
                self.launcher.tried = Some(info.sha256.clone());
                return;
            }
        }
        let dir = self.config.data.join(UPDATE_DIR);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATE_FAILED, &[&err]),
            );
            return;
        }
        self.next_task = self.next_task.wrapping_add(1);
        let task = DownloadTask {
            gen: self.next_task,
            version: info.version.clone(),
            sha256: info.sha256.clone(),
        };
        self.launcher.task = Some(task.clone());
        self.launcher.tried = Some(info.sha256.clone());
        self.logf(
            "app",
            &mesh_text::format(
                mesh_text::id::ENGINE_LOG_LAUNCHER_DOWNLOAD,
                &[&info.version],
            ),
        );
        let api = Arc::clone(api);
        let data = self.config.data.clone();
        let install = self.config.root.clone();
        let log = Arc::clone(&self.log);
        let tx = self.self_tx.clone();
        let url = info.url.clone();
        let sent = task.clone();
        let spawned = thread::Builder::new()
            .name("mesh-launcher-update".to_string())
            .spawn(move || {
                let result = replace_launcher(&api, &data, &install, &url, &sent, &log);
                let _ = tx.send(Cmd::Launcher { task: sent, result });
            });
        if spawned.is_err() {
            self.launcher.task = None;
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_THREAD_FAILED),
            );
        }
    }

    /// 启动器替换的结果：成功就把「盘上那份」的事实改成新的。
    ///
    /// `Deferred` 表示这次只是没拿到安装根写权：它不是结论，`tried` 要放回去，
    /// 下一轮心跳拿同一份 offer 再试一次。
    pub(crate) fn on_launcher_result(&mut self, task: DownloadTask, result: LauncherResult) {
        if self.launcher.task.as_ref() != Some(&task) {
            return;
        }
        self.launcher.task = None;
        match result {
            LauncherResult::Replaced => {
                self.launcher.installed_sum = task.sha256;
                self.launcher.installed_version = Some(task.version);
            }
            LauncherResult::Deferred => self.launcher.tried = None,
            LauncherResult::Failed => {}
        }
    }

    /// 清掉 `update/` 里上一轮留下的临时文件。
    ///
    /// 只清这个目录：`update.tar.zst` 在数据根下，它是「请安装」的信号，不是临时
    /// 文件，任何时候都不该由客户端顺手删掉。
    pub(crate) fn clean_update_dir(&mut self) {
        let dir = self.config.data.join(UPDATE_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // 这一层既放过半截下载（文件）也放过临时目录，两种都要清得掉。
            let outcome = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match outcome {
                Ok(()) => self.logf(
                    "app",
                    &mesh_text::format(mesh_text::id::ENGINE_LOG_CLEAN_TEMP_FILE, &[&name]),
                ),
                Err(err) => self.logf(
                    "app",
                    &mesh_text::format(mesh_text::id::ENGINE_LOG_CLEAN_TEMP_FILE_FAILED, &[&err]),
                ),
            }
        }
    }

    /// 安装根的写权；拿不到就是“这一轮先不提交”，由调用方保留暂存件。
    fn own_install_root(&mut self) -> Option<mesh_install::lock::Lock> {
        // 锁在数据根下：安装根的写权由提权本身保证，这把锁只负责「谁在装」。
        match acquire_install_root(&self.config.data) {
            Ok(owner) => Some(owner),
            Err(err) => {
                self.logf("app", &err);
                None
            }
        }
    }
}

/// 交接给启动器的命令行：包、旧进程号，以及客户端验过的那份整包摘要。
///
/// 摘要就是发布签名盖住的那个值：客户端已经验过签名，启动器只需要再证明一遍
/// 「盘上的字节没换过」。
pub(crate) fn handoff_command(launcher: &Path, path: &Path, sha256: &str) -> std::process::Command {
    let mut command = std::process::Command::new(launcher);
    command
        .arg("--file")
        .arg(path)
        .arg("--wait-for")
        .arg(std::process::id().to_string())
        .arg("--sha256")
        .arg(sha256);
    command
}

/// 下载并校验更新包，落成 `<root>/update.tar.zst.part`。
///
/// 返回的是暂存文件，**不是**固定路径：改名由引擎在确认这次交接确实被授权之后
/// 做（[`commit_package`]）。半截文件、被撤回的 offer、被换掉的旧任务都不会在
/// 安装根下留下一个「请安装」的信号。
pub(crate) fn download_package(
    api: &remote::Client,
    root: &Path,
    url: &str,
    want: &str,
) -> Result<PathBuf, String> {
    let part = root.join(format!("{}.part", layout::UPDATE_FILE));
    let _ = std::fs::remove_file(&part);
    if let Err(err) = api.download(url, &part, MAX_UPDATE_BYTES, DOWNLOAD_TIMEOUT) {
        let _ = std::fs::remove_file(&part);
        return Err(err.to_string());
    }
    let Some(sum) = artifact::digest_file(&part) else {
        let _ = std::fs::remove_file(&part);
        return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_READ_DOWNLOAD_FAILED).to_string());
    };
    // 服务端没给摘要就不装：摘要为空时放行等于给「中间人换包」留了唯一一道缝。
    if want.is_empty() {
        let _ = std::fs::remove_file(&part);
        return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_NO_DIGEST_REFUSE).to_string());
    }
    if !sum.eq_ignore_ascii_case(want) {
        let _ = std::fs::remove_file(&part);
        return Err(mesh_text::format(
            mesh_text::id::ENGINE_TEXT_DIGEST_MISMATCH,
            &[&sum],
        ));
    }
    Ok(part)
}

/// 安装根的写权：`install.lock`，启动器 `Owner` 用的同一把。
fn acquire_install_root(root: &Path) -> Result<mesh_install::lock::Lock, String> {
    mesh_install::lock::acquire(&mesh_install::lock::Lock::owner_path(root)).map_err(
        |err| match err {
            mesh_install::Error::AlreadyRunning => {
                mesh_text::get(mesh_text::id::LAUNCHER_ERROR_INSTALL_ROOT_BUSY).to_string()
            }
            err => err.to_string(),
        },
    )
}

/// 用暂存件替换 `dst`，跨卷时也能原子落地。
///
/// 暂存在数据根、目标是安装根：两者常常不在同一个卷上（`%LOCALAPPDATA%` 在 C:，
/// 程序装在 D: 或 S:），`rename` 会直接给 `EXDEV`。跨卷时先复制到目标旁边的同名
/// 临时文件（这时是同一个卷），再改名覆盖——对外仍然只有「旧文件」与「新文件」两种
/// 可见状态，不会出现半个启动器。
fn replace_across_volumes(temp: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(temp, dst) {
        Ok(()) => return Ok(()),
        Err(err) if err.raw_os_error() == Some(EXDEV) => {}
        Err(err) => return Err(err),
    }
    let sibling = dst.with_extension("new");
    std::fs::copy(temp, &sibling)?;
    std::fs::rename(&sibling, dst)?;
    let _ = std::fs::remove_file(temp);
    Ok(())
}

/// 跨卷改名的错误码：Unix `EXDEV`，Windows `ERROR_NOT_SAME_DEVICE`。
#[cfg(unix)]
const EXDEV: i32 = 18;
#[cfg(windows)]
const EXDEV: i32 = 17;

/// 把校验过的暂存文件连同它的整包摘要放到固定名：从这一刻起它才是「请安装」的信号。
///
/// Unix 上摘要先落盘、包后改名：包名本身就是信号，常驻启动器可能在这一瞬间已经
/// 读走它——摘要晚到一次，那一份就只能按手动路径只验清单。Windows 的摘要走交接
/// 命令行，这里不落文件。
///
/// 调用方必须持有安装根写权。
pub(crate) fn commit_package(part: &Path, root: &Path, sha256: &str) -> Result<PathBuf, String> {
    #[cfg(unix)]
    let sum = {
        let sum = root.join(layout::UPDATE_DIGEST);
        mesh_install::atomic_replace(&sum, sha256.as_bytes(), 0o644)
            .map_err(|err| mesh_text::format(mesh_text::id::ENGINE_TEXT_PERSIST_FAILED, &[&err]))?;
        sum
    };
    #[cfg(not(unix))]
    let _ = sha256;
    let dst = root.join(layout::UPDATE_FILE);
    if let Err(err) = std::fs::rename(part, &dst) {
        // 包没成信号，摘要也不再属于任何一份：一起撤掉，别留下孤立的授权。
        #[cfg(unix)]
        let _ = std::fs::remove_file(&sum);
        return Err(mesh_text::format(
            mesh_text::id::ENGINE_TEXT_PERSIST_FAILED,
            &[&err],
        ));
    }
    Ok(dst)
}

/// 第 n 次失败之后多久再试：30s 起步翻倍，最多 10 分钟。
///
/// 节拍是心跳（30s 一次），所以这里给的是下限，实际发生时刻在下一轮心跳。
fn retry_delay(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(5);
    Duration::from_secs((30u64 << shift).min(600))
}

/// 后台替换启动器：下载、校验、拿写权、改名。
fn replace_launcher(
    api: &remote::Client,
    data: &Path,
    install: &Path,
    url: &str,
    task: &DownloadTask,
    log: &Log,
) -> LauncherResult {
    let temp = data.join(UPDATE_DIR).join("launcher.new");
    // 上一轮已经下好并校验过、只是没拿到安装根写权的那一份：直接复用，不重下。
    if !artifact::matches(&temp, &task.sha256) {
        let _ = std::fs::remove_file(&temp);
        if let Err(err) = api.download(url, &temp, MAX_UPDATE_BYTES, LAUNCHER_TIMEOUT) {
            log.add(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_DOWNLOAD_FAILED, &[&err]),
            );
            // 传输失败不是结论：下一轮心跳再试。判成失败会让这一份在进程余下的
            // 生命周期里再也不重试，一次网络抖动就永久停在一个旧启动器上。
            return LauncherResult::Deferred;
        }
        if !artifact::matches(&temp, &task.sha256) {
            log.add(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_DIGEST_MISMATCH),
            );
            let _ = std::fs::remove_file(&temp);
            return LauncherResult::Failed;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)) {
            log.add(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_CHMOD_FAILED, &[&err]),
            );
            let _ = std::fs::remove_file(&temp);
            return LauncherResult::Failed;
        }
    }
    // 换启动器也是写安装根：启动器正装着东西时不能动它的文件。
    let owner = match acquire_install_root(install) {
        Ok(owner) => owner,
        Err(err) => {
            log.add(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATE_FAILED, &[&err]),
            );
            // 这次不算结论：下一轮心跳再试（盘上那份没动）。已验工件留在暂存区，不重下。
            return LauncherResult::Deferred;
        }
    };
    let dst = install.join(mesh_install::platform::launcher_name());
    let outcome = match replace_across_volumes(&temp, &dst) {
        Ok(()) => {
            log.add(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATED, &[&task.version]),
            );
            LauncherResult::Replaced
        }
        Err(err) => {
            log.add(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_REPLACE_FAILED, &[&err]),
            );
            let _ = std::fs::remove_file(&temp);
            LauncherResult::Failed
        }
    };
    drop(owner);
    outcome
}
