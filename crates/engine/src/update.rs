//! 客户端自更新：下载、校验摘要、交给启动器安装。
//!
//! 对应 Go 的 `engine/update.go`。三条不能改的行为：
//!
//! - 包先落到 `<root>/update.tar.zst.part`，摘要对得上、并且这次下载仍然是当前
//!   那份 offer 时才改名成 `update.tar.zst`：固定名字本身就是「请安装」的信号，
//!   半截文件、被撤回的 offer、被换掉的旧任务都不许留下它。
//! - **服务端没给摘要就拒绝安装**：摘要为空时放行等于给「中间人换包」留了一道缝。
//! - 失败态保留到换版本或用户重试，否则下一轮心跳会把提示抹掉，界面改口说“已就绪”。
//!
//! 一份 offer 的身份是「版本 + 摘要」：同一版本号重新发布时内容会变，撤回、重发与
//! 策略变更都必须显式迁移状态。下载任务另带自己的代次回来（见 [`DownloadTask`]），
//! 旧任务完成时手上已经不是那一份，结果只能作废。

use crate::run::Engine;
use crate::sync::file_digest;
use crate::Cmd;
use mesh_install::{layout, version};
use mesh_remote as remote;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// 更新包上限，和启动器里的 200MB 对齐。
pub(crate) const MAX_UPDATE_BYTES: u64 = 200 << 20;
/// 下载更新包的总超时（Go 是 20 分钟）。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// 下载启动器的总超时。
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 与启动器里的 `LIMIT` 一致：两次还是起不来就说明包本身有问题。
const FAILURE_LIMIT: u32 = 2;
/// 下载失败后自动重试的次数上限。用完就停下来等人处理，别把日志和发布文件服务
/// 一起刷满。
const MAX_DOWNLOAD_ATTEMPTS: u32 = 5;
/// 启动器替换用的临时目录（Go 也叫 `update/`）。
const UPDATE_DIR: &str = "update";

/// 自更新状态。下载与解包都在启动器里完成，所以只有“可安装”一态。
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

/// 一次下载任务的标识。
///
/// `gen` 是代次，不是 offer 身份：同一份 offer 重下、或者下载中换了 offer，都会
/// 起一个不同的任务。完成消息带着它回来，引擎据此判断「这条结果还算数吗」——
/// 只比版本号不够（同版本重发内容会变），只比摘要也不够（同一份 offer 可能重下过）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadTask {
    pub gen: u64,
    pub version: String,
    pub sha256: String,
}

/// 手上这份 offer 及其进度。
///
/// 版本与摘要永远成对描述同一份 offer：判重、自动安装资格、下载完成都按它们一起
/// 判断。不另设「已知的摘要」这类副本——两份状态迟早分叉。
#[derive(Debug, Clone, Default)]
pub(crate) struct UpdateState {
    pub phase: Phase,
    pub version: String,
    pub notes: String,
    pub url: String,
    pub sha256: String,
    /// 已下载到本地的包（Windows 预取与 Unix 下载都会填）。
    pub path: Option<PathBuf>,
    /// 本地包的完整摘要，交接时交给启动器。
    pub sum: String,
    pub error: String,
    /// 正在下载或交接：心跳不得在此期间改写状态。
    pub busy: bool,
    /// 下载完成后是否立刻交接（用户点了安装 / 自动安装）。
    pub apply_after_download: bool,
    /// 这份 offer 允不允许自动安装：服务端策略与本机否决合起来的结果。
    ///
    /// 与 offer 同生共死，而不是放在引擎上：换一份 offer 就是另一份授权；下载中
    /// 管理端也可能改掉它（精简响应只带这个开关）。
    pub auto: bool,
    /// 这份 offer 已经失败过几次下载。
    pub attempts: u32,
    /// 下一次自动重试的时刻；退避由它算，不额外排定时器：心跳就是节拍。
    pub retry_at: Option<Instant>,
    /// 正在跑的下载任务。任何时刻最多一个：两个线程写同一个 `.part` 只会互相
    /// 截断。任务结束时清空；被换掉的任务由它自己的完成消息认领。
    pub task: Option<DownloadTask>,
    /// 已经下载并校验好、但还没提交到固定路径的包。
    ///
    /// 提交要拿安装根写权（启动器装着东西的时候不能硬写），拿不到就留在这里：
    /// 包是好的，别为了一个正在进行的安装重下一遍。下一轮心跳重试提交。
    pub part: Option<PathBuf>,
}

impl UpdateState {
    fn has_offer(&self) -> bool {
        !self.version.is_empty()
    }

    /// 手上这份是不是 `version` + `sha256` 那一份。
    fn is(&self, version: &str, sha256: &str) -> bool {
        self.has_offer() && self.version == version && self.sha256.eq_ignore_ascii_case(sha256)
    }
}

/// 启动器槽位的本地状态。
///
/// 与 [`UpdateState`] 分开：那份状态会随客户端包 offer 整体重建为默认值，而启动器
/// 的已知摘要、尝试记录、版本与在途任务要活到进程结束——否则一次普通的客户端
/// offer 变化就会把正在跑的替换任务放开给第二个任务，两份下载互相截断。
///
/// `sum` 与 `version` 描述的是**盘上**那份启动器：启动时读一次，替换成功之后用
/// 已验签的结果更新。它们不描述正在运行的启动器——两者只在本进程没有替换过时
/// 相同；替换成功后盘上已经是新的，而正在跑的那个还是旧的，直到它下次被拉起。
/// 判降级只能用盘上那份的版本，否则会把设备换回一个更旧的启动器。
#[derive(Default)]
pub(crate) struct LauncherState {
    /// 安装根下这份启动器的完整摘要。
    pub sum: String,
    /// 本进程对哪一份 offer 已经有结论（换掉了，或拒绝换成）：同一份不再重试。
    pub tried: Option<String>,
    /// 盘上这份启动器自报的版本号；读不到就是 None，那时不判降级。
    pub version: Option<String>,
    /// 替换任务正在跑。
    ///
    /// 换启动器是进程级动作，暂存文件只有一个：两个任务同时进来，后一个的
    /// 「先删再下」会把前一个正在写的文件截断，两边都算出对不上的摘要。
    /// 同一时刻只放一个进去，后来的等下一轮心跳再说。
    pub busy: Arc<AtomicBool>,
}

impl LauncherState {
    /// 启动时读一次盘上的事实：安装根里那份启动器的摘要，与它自报的版本号。
    ///
    /// 只在这一个地方读：之后由替换结果更新（[`Engine::on_launcher_result`]）。
    /// 记下的是「盘上那份」，不是正在跑的那一份——两者只在没有替换过时相同。
    pub(crate) fn read(root: &Path) -> LauncherState {
        LauncherState {
            sum: crate::sync::file_digest(&root.join(mesh_install::platform::launcher_name()))
                .unwrap_or_default(),
            version: layout::read_launcher_version(root),
            ..LauncherState::default()
        }
    }
}

/// 启动器替换的占坑守卫：线程从任何一条路径退出都把坑放掉，包括提前 return。
struct LauncherBusy(Arc<AtomicBool>);

impl Drop for LauncherBusy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 64 位十六进制的完整 sha256。发布链用「摘要来自控制面、内容来自任意地址」这一对
/// 判据确认包没被换过，所以两边都必须是完整摘要。
fn is_full_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Engine {
    /// 下一轮心跳要上报的「我手上是哪一份发布包」。
    ///
    /// 来源就是 `update.sha256`——它是“内存里这份 offer 的摘要”，与进度（`phase`）无关：
    /// Ready / Downloading / Failed 都仍然是同一份 offer，都该照报；`Idle`（没有 offer，
    /// 或被撤回清掉）自然就是 `None`。
    ///
    /// 特意不另设一个 `known_sha` 字段：两个字段迟早出现“清了 offer 忘了清 known”的
    /// 分叉，而这里 known 与 offer 是同一件事，只有一个来源才不可能不一致。
    pub(crate) fn known_app_update(&self) -> Option<String> {
        if self.update.sha256.is_empty() {
            None
        } else {
            Some(self.update.sha256.clone())
        }
    }

    /// 记下服务端报告的新版本。安装动作交给启动器。
    ///
    /// 一份 offer 的处理只有两个出口：同一份就只更新自动安装资格（下载中不打断
    /// 任务），另一份就整体换掉（撤回、同版本重发、换版本都走这一步）。
    pub(crate) fn offer_update(&mut self, info: Option<&remote::UpdateInfo>) {
        let Some(info) = info else {
            return;
        };
        let auto = info.auto_update && !self.config.auto_update_disabled;
        // 精简响应：服务端确认“你手上就是这份发布包”。
        //
        // 只认开关，别的什么都不动：不校验 url/版本号（精简形状里本来就没有 url），
        // 不写日志（这正是要省掉的那份每 30 秒重复），不碰 update 的 phase/version/
        // notes/sha256/path（否则会把“已知”错当成“新下发”，把已就绪的包重置成待下载）。
        if info.unchanged {
            if self.update.has_offer() {
                // 管理端可能刚改过设备级覆盖：这份 offer 的授权跟着变。
                self.update.auto = auto;
            }
            // 下载失败过的 offer 仍要按退避重试。客户端上报的「我有哪一份」与
            // 进度无关（Failed 也照报），服务端据此一直回精简形状，重试分支
            // 一次都进不去——最常见的失败（文件服务抖一下）就永远等不到第二次
            // 机会，界面停在错误上直到有人手动点重试。
            if self.update.phase == Phase::Failed {
                self.retry_failed_download();
            }
            return;
        }
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
        let failures = self.update_failures(&info.version);
        if failures >= FAILURE_LIMIT {
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_SKIP_FAILED_VERSION,
                    &[&info.version, &failures],
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
        // 服务端每次发布都会现算完整摘要，缺失只说明对面不是我们以为的控制面。
        if !is_full_digest(&info.sha256) {
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_IGNORE_NO_DIGEST, &[&info.version]),
            );
            return;
        }
        // 摘要只证明内容没被换过，签名才证明它出自发布方：控制面被拿下时能改
        // 摘要，却签不出新签名。这是整条更新链上唯一的信任锚。
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
        // 交接中：安装器已经拉起、进程马上退出，这一轮不再改任何状态。
        if self.update.busy {
            return;
        }
        if self.update.is(&info.version, &info.sha256) {
            // 同一份：策略可能刚变，进度与在跑的下载由现有状态机管。
            self.update.auto = auto;
            if self.update.phase == Phase::Failed {
                self.retry_failed_download();
            }
            return;
        }
        self.start_offer(info, auto);
    }

    /// 换上一份新的 offer。撤回、同版本重发与换版本走的是同一条状态迁移。
    ///
    /// 在跑的下载任务留在原地：线程没法取消，而且 `.part` 只有一个，再起一个只会
    /// 互相截断。它的完成消息回来时身份已经对不上，由 [`Engine::on_download`]
    /// 判为过期，接手新 offer 的下载。
    fn start_offer(&mut self, info: &remote::UpdateInfo, auto: bool) {
        let task = self.update.task.take();
        // 上一份 offer 已经下好但还没提交的包：内容不再被授权，删掉它自己的
        // 临时文件（固定路径上的包仍然不归客户端清）。
        if let Some(part) = self.update.part.take() {
            let _ = std::fs::remove_file(part);
        }
        self.update = UpdateState {
            phase: Phase::Ready,
            version: info.version.clone(),
            notes: info.notes.clone(),
            url: info.url.clone(),
            sha256: info.sha256.clone(),
            auto,
            task,
            ..UpdateState::default()
        };
        // Windows 先把包抓到本地：安装发生在客户端退出之后，那时 mesh 已经断了。
        if cfg!(windows) {
            self.update.phase = Phase::Downloading;
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_DOWNLOAD_STARTED, &[&info.version]),
            );
            self.begin_download();
            return;
        }
        self.logf(
            "app",
            &mesh_text::format(
                mesh_text::id::ENGINE_LOG_UPDATE_READY_MANUAL,
                &[&info.version],
            ),
        );
    }

    /// 无人值守模式：直接安装刚记录下来的更新。每份 offer 只试一次。
    pub(crate) fn auto_apply(&mut self) {
        if self.update.phase != Phase::Ready || self.update.busy || !self.update.has_offer() {
            return;
        }
        if !self.update.auto {
            return;
        }
        // 包已下好但还没提交（安装根被别人占着）：等提交成功之后由
        // accept_download 再调一次，这里不做任何动作，免得重下一遍。
        if self.update.part.is_some() {
            return;
        }
        // 按摘要去重而不是按版本号：同版本重发是另一份 offer，该给它一次机会。
        if self.auto_tried.as_deref() == Some(self.update.sha256.as_str()) {
            return;
        }
        self.auto_tried = Some(self.update.sha256.clone());
        self.logf(
            "app",
            &mesh_text::format(
                mesh_text::id::ENGINE_LOG_AUTO_APPLY,
                &[&self.update.version],
            ),
        );
        if let Err(err) = self.apply_update() {
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_AUTO_APPLY_FAILED, &[&err]),
            );
        }
    }

    /// 用户点了“安装”（或自动安装）：Windows 交接，Unix 先下载再交接。
    pub(crate) fn apply_update(&mut self) -> Result<(), String> {
        if self.update.busy {
            return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_UPDATE_IN_PROGRESS).to_string());
        }
        // 客户端自己查得出来的失败不要拖到进程退出之后。
        if !version::valid(&self.update.version) {
            let shown = format!("{:?}", self.update.version);
            let msg = mesh_text::format(mesh_text::id::ENGINE_TEXT_VERSION_INVALID, &[&shown]);
            self.fail_update(&msg);
            return Err(msg);
        }
        match self.update.phase {
            Phase::Downloading => {
                return Err(
                    mesh_text::get(mesh_text::id::ENGINE_TEXT_UPDATE_DOWNLOADING).to_string(),
                )
            }
            // 失败态允许用户直接重试：同样一份包、同一个版本，不必等下一轮心跳。
            Phase::Ready | Phase::Failed => {}
            Phase::Idle => {
                return Err(
                    mesh_text::get(mesh_text::id::ENGINE_TEXT_NOTHING_TO_INSTALL).to_string(),
                )
            }
        }
        // 上一轮下好、但没拿到安装根写权的那份包：先试提交，别重下已经校验过的字节。
        if self.update.part.is_some() {
            self.resume_commit();
        }
        // 包已经在本地（预取，或被推迟的提交刚刚完成）：两个平台都是直接交接。
        if self
            .update
            .path
            .as_deref()
            .map(Path::is_file)
            .unwrap_or(false)
        {
            self.update.busy = true;
            self.update.apply_after_download = false;
            return self.handoff();
        }
        if self.api.is_none() {
            return Err(
                mesh_text::get(mesh_text::id::ENGINE_TEXT_NO_SERVER_FOR_DOWNLOAD).to_string(),
            );
        }
        // 没有本地包说明下载失败过、被人删掉，或者安装根一直被别人占着。
        // 重新下一份、下完直接交接——面板上的「重试」走的就是这条路径。
        self.update.attempts = 0;
        self.update.retry_at = None;
        self.update.phase = Phase::Downloading;
        self.update.error.clear();
        self.update.apply_after_download = true;
        if cfg!(windows) {
            let version = self.update.version.clone();
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_REDOWNLOAD_FOR_INSTALL,
                    &[&version],
                ),
            );
        }
        self.begin_download();
        Ok(())
    }

    /// 下载线程把包写到 `.part` 后回报。
    ///
    /// 只有「这条结果仍然属于手上这份 offer、并且就是这个任务」时才接受：撤回、
    /// 同版本重发与换任务都会让旧结果失效，旧结果甚至不许留下固定路径上的包。
    pub(crate) fn on_download(&mut self, task: DownloadTask, result: Result<PathBuf, String>) {
        if self.update.task.as_ref() != Some(&task) {
            // 不是手上这个任务（重复回报或早已作废）：只清它自己的临时文件，
            // 也绝不碰正在跑的那个任务。固定路径上的包不动——那是交接信号，
            // 只有消耗它的安装事务能清（F3）。
            if let Ok(part) = &result {
                let _ = std::fs::remove_file(part);
            }
            return;
        }
        self.update.task = None;
        if !self.update.is(&task.version, &task.sha256) {
            // 任务跑完时 offer 已经换了或撤回：这份内容不再被授权。
            if let Ok(part) = &result {
                let _ = std::fs::remove_file(part);
            }
            if self.update.phase == Phase::Downloading {
                // 新 offer 还在等下载：旧任务已经让路，接着下。
                self.begin_download();
            }
            return;
        }
        match result {
            Ok(part) => {
                // 提交要在安装根写权之内：启动器可能正在装上一份，两边会踩到
                // 同一个文件名。拿不到就留在这里，下一轮心跳再提交。
                let Some(owner) = self.own_install_root_for_package(&part) else {
                    return;
                };
                // 被接受才改成固定名字：`update.tar.zst` 一出现就是「请安装」。
                let path = match commit_package(&part, &self.config.root) {
                    Ok(path) => path,
                    Err(err) => {
                        drop(owner);
                        let _ = std::fs::remove_file(&part);
                        self.fail_update(&err);
                        self.logf("app", &err);
                        return;
                    }
                };
                drop(owner);
                self.accept_download(path);
            }
            Err(err) => {
                let version = self.update.version.clone();
                self.update.phase = Phase::Failed;
                self.update.error = err.clone();
                self.update.busy = false;
                self.update.attempts = self.update.attempts.saturating_add(1);
                self.update.retry_at = Some(Instant::now() + retry_delay(self.update.attempts));
                self.logf(
                    "app",
                    &mesh_text::format(
                        mesh_text::id::ENGINE_LOG_DOWNLOAD_FAILED,
                        &[&version, &self.update.attempts, &err],
                    ),
                );
            }
        }
    }

    /// 下载失败且退避到期时重下一次。次数用完就停，错误提示留在界面上等人处理。
    fn retry_failed_download(&mut self) {
        if self.update.phase != Phase::Failed || self.update.busy || self.update.url.is_empty() {
            return;
        }
        if self.update.attempts >= MAX_DOWNLOAD_ATTEMPTS {
            return;
        }
        if !self
            .update
            .retry_at
            .map(|at| Instant::now() >= at)
            .unwrap_or(true)
        {
            return;
        }
        let version = self.update.version.clone();
        let attempt = self.update.attempts + 1;
        self.update.phase = Phase::Downloading;
        self.update.error.clear();
        self.update.retry_at = None;
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_REDOWNLOAD, &[&version, &attempt]),
        );
        self.begin_download();
    }

    /// 把安装交给启动器，然后退出。
    fn handoff(&mut self) -> Result<(), String> {
        let version = self.update.version.clone();
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
                self.fail_update(&msg);
                return Err(msg);
            }
            let Some(path) = self.update.path.clone() else {
                let msg = mesh_text::get(mesh_text::id::ENGINE_TEXT_PACKAGE_NOT_LOCAL).to_string();
                self.fail_update(&msg);
                return Err(msg);
            };
            let mut command = std::process::Command::new(&launcher);
            command
                .arg("--file")
                .arg(&path)
                .arg("--wait-for")
                .arg(std::process::id().to_string());
            if !self.update.sum.is_empty() {
                command.arg("--sha256").arg(&self.update.sum);
            }
            command.current_dir(&self.config.root);
            if let Err(err) = command.spawn() {
                let msg =
                    mesh_text::format(mesh_text::id::ENGINE_TEXT_LAUNCHER_START_FAILED, &[&err]);
                self.fail_update(&msg);
                return Err(msg);
            }
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_INSTALL_STARTED, &[&version]),
            );
            // 必须先把启动器拉起来再退出，否则新版本装不上（F7）。
            self.shutdown.run_and_exit(0);
        }
        // Linux：客户端只负责把包放到安装根，解包与启动交给常驻启动器；正常退出即可。
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_HANDOFF_LAUNCHER, &[&version]),
        );
        self.shutdown.run_and_exit(0);
    }

    /// 控制面不再提供这份 offer（撤回）：把 offer 显式清掉。
    ///
    /// 下载中也照清：线程取消不了，但它的完成消息会带着旧身份回来被判为过期，
    /// 不会把内容留下、更不会去安装。固定路径上的包不动——只有消耗它的安装事务
    /// 能清（F3）。在跑的任务留在原地等它自己结束，避免两个线程写同一个 `.part`。
    pub(crate) fn clear_stale_update(&mut self) {
        if self.update.busy || self.update.phase == Phase::Idle {
            return;
        }
        let task = self.update.task.take();
        if let Some(part) = self.update.part.take() {
            let _ = std::fs::remove_file(part);
        }
        self.update = UpdateState {
            task,
            part: None,
            ..UpdateState::default()
        };
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
        if info.url.is_empty() || !is_full_digest(&info.sha256) {
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
        // 判重与占位在同一处完成：同一台机器只尝试一次，换不掉就等下个版本。
        if self.launcher.sum.eq_ignore_ascii_case(&info.sha256)
            || self.launcher.tried.as_deref() == Some(info.sha256.as_str())
        {
            return;
        }
        // 摘要只证明「是不是同一份」，不证明「是不是更新的」：一份当年真签过的旧
        // 三元组能让设备退回还没有清单校验的旧启动器。盘上这份自己报的版本号是
        // 唯一的本地依据，它比下发的更新才换；没有记录时不判，放行这一次。
        //
        // 比的是**盘上**那份：替换成功后引擎已经把 version 更新成新的那一版，
        // 所以一次进程里有多次发布时不会把设备来回换。
        if let Some(current) = self.launcher.version.as_deref() {
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
        let dir = self.config.root.join(UPDATE_DIR);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATE_FAILED, &[&err]),
            );
            return;
        }
        // 占坑放在最后：上面几条都是「不干」，不该把坑占住。
        if self.launcher.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let busy_guard = LauncherBusy(Arc::clone(&self.launcher.busy));
        self.launcher.tried = Some(info.sha256.clone());
        let api = Arc::clone(api);
        let root = self.config.root.clone();
        let log = Arc::clone(&self.log);
        let tx = self.self_tx.clone();
        let version = info.version.clone();
        let url = info.url.clone();
        let want = info.sha256.clone();
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_DOWNLOAD, &[&version]),
        );
        let spawned = thread::Builder::new()
            .name("mesh-launcher-update".to_string())
            .spawn(move || {
                let _busy = busy_guard;
                let temp = root.join(UPDATE_DIR).join("launcher.new");
                let _ = std::fs::remove_file(&temp);
                if let Err(err) = api.download(&url, &temp, MAX_UPDATE_BYTES, LAUNCHER_TIMEOUT) {
                    log.add(
                        "app",
                        &mesh_text::format(
                            mesh_text::id::ENGINE_LOG_LAUNCHER_DOWNLOAD_FAILED,
                            &[&err],
                        ),
                    );
                    return;
                }
                let Some(sum) = file_digest(&temp) else {
                    log.add(
                        "app",
                        mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_VERIFY_FAILED),
                    );
                    let _ = std::fs::remove_file(&temp);
                    return;
                };
                if !sum.eq_ignore_ascii_case(&want) {
                    log.add(
                        "app",
                        mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_DIGEST_MISMATCH),
                    );
                    let _ = std::fs::remove_file(&temp);
                    return;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(err) =
                        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
                    {
                        log.add(
                            "app",
                            &mesh_text::format(
                                mesh_text::id::ENGINE_LOG_LAUNCHER_CHMOD_FAILED,
                                &[&err],
                            ),
                        );
                        return;
                    }
                }
                // 换启动器也是写安装根：启动器正装着东西时不能动它的文件。
                let owner = match own_install_root(&root) {
                    Ok(owner) => owner,
                    Err(err) => {
                        log.add(
                            "app",
                            &mesh_text::format(
                                mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATE_FAILED,
                                &[&err],
                            ),
                        );
                        let _ = std::fs::remove_file(&temp);
                        // 这次不算结论：下一轮心跳再试（盘上那份没动）。
                        let _ = tx.send(Cmd::Launcher {
                            version,
                            sha256: want,
                            replaced: false,
                            retry: true,
                        });
                        return;
                    }
                };
                let dst = root.join(mesh_install::platform::launcher_name());
                let replaced = match std::fs::rename(&temp, &dst) {
                    Ok(()) => {
                        log.add(
                            "app",
                            &mesh_text::format(
                                mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATED,
                                &[&version],
                            ),
                        );
                        true
                    }
                    Err(err) => {
                        log.add(
                            "app",
                            &mesh_text::format(
                                mesh_text::id::ENGINE_LOG_LAUNCHER_REPLACE_FAILED,
                                &[&err],
                            ),
                        );
                        let _ = std::fs::remove_file(&temp);
                        false
                    }
                };
                drop(owner);
                // 结果交回引擎线程：盘上那份换成什么，只有状态机知道该记成什么。
                let _ = tx.send(Cmd::Launcher {
                    version,
                    sha256: want,
                    replaced,
                    retry: false,
                });
            });
        if spawned.is_err() {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_THREAD_FAILED),
            );
        }
    }

    /// 启动器替换的结果：成功就把「盘上那份」的事实改成新的。
    ///
    /// `retry` 表示这次只是没拿到安装根写权：它不是结论，`tried` 要放回去，
    /// 下一轮心跳拿同一份 offer 再试一次。
    ///
    /// 不写 `launcher.json`——那份记录只由启动器自己写（它是唯一确定「我就是那个
    /// 文件」的进程）。因此「正在运行的启动器」与「盘上启动器」只在本进程没换过
    /// 启动器时相同：换过之后盘上已经是新的，而要到下一次拉起才是新的在跑。
    pub(crate) fn on_launcher_result(
        &mut self,
        version: String,
        sha256: String,
        replaced: bool,
        retry: bool,
    ) {
        if retry {
            // 安装根被启动器占着：这不是结论，下一轮心跳拿同一份 offer 再来。
            self.launcher.tried = None;
            return;
        }
        if !replaced {
            // 换失败：盘上那份没变，事实也不变；tried 已经记过，同一份不再重试。
            return;
        }
        self.launcher.sum = sha256;
        self.launcher.version = Some(version);
    }

    /// 清掉 `update/` 里上一轮留下的临时文件。
    ///
    /// 只清这个目录：`update.tar.zst` 在安装根下，它是「请安装」的信号，不是临时
    /// 文件，任何时候都不该由客户端顺手删掉（F3）。
    pub(crate) fn clean_update_dir(&mut self) {
        let dir = self.config.root.join(UPDATE_DIR);
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

    /// 启动器落盘的失败计数；读不到就是 0（文件只在失败之后才存在）。
    fn update_failures(&self, version: &str) -> u32 {
        mesh_install::failures::Failures::load(&self.config.root).count(version)
    }

    fn fail_update(&mut self, message: &str) {
        self.update.phase = Phase::Failed;
        self.update.error = message.to_string();
        self.update.busy = false;
    }

    /// 为当前 offer 起一次下载。单飞由 `update.task` 保证：有任务在跑就等它回来，
    /// 它的完成消息会接手这一次（见 `on_download`）。
    fn begin_download(&mut self) {
        if self.update.task.is_some() || self.update.phase != Phase::Downloading {
            return;
        }
        // 重新下一个包：上一份「等下写权」的记录作废（同一个 .part 会被这次下载覆盖）。
        self.update.part = None;
        let Some(api) = self.api.clone() else {
            let message =
                mesh_text::get(mesh_text::id::ENGINE_TEXT_NO_SERVER_FOR_DOWNLOAD).to_string();
            self.fail_update(&message);
            return;
        };
        self.next_download = self.next_download.wrapping_add(1);
        let task = DownloadTask {
            gen: self.next_download,
            version: self.update.version.clone(),
            sha256: self.update.sha256.clone(),
        };
        let root = self.config.root.clone();
        let url = self.update.url.clone();
        let want = task.sha256.clone();
        let tx = self.self_tx.clone();
        let sent = task.clone();
        let spawned = thread::Builder::new()
            .name("mesh-download".to_string())
            .spawn(move || {
                let result = download_package(&api, &root, &url, &want);
                let _ = tx.send(Cmd::Download { task: sent, result });
            });
        if spawned.is_err() {
            let message =
                mesh_text::get(mesh_text::id::ENGINE_TEXT_DOWNLOAD_THREAD_FAILED).to_string();
            self.fail_update(&message);
            self.logf("app", &message);
            return;
        }
        self.update.task = Some(task);
    }
}

impl Engine {
    /// 提交包的安装根写权。拿不到就把包留在 `update.part` 里等下一轮。
    ///
    /// 返回 `None` 表示这一轮不提交也不报错：包是好的，安装根正被启动器用着
    /// （它装着东西的时候我们去改同一批文件只会互相打脸）。到点由
    /// [`Engine::resume_commit`] 接着提交，不重下。
    fn own_install_root_for_package(&mut self, part: &Path) -> Option<mesh_install::lock::Lock> {
        match own_install_root(&self.config.root) {
            Ok(owner) => Some(owner),
            Err(err) => {
                self.logf("app", &err);
                self.update.part = Some(part.to_path_buf());
                self.update.phase = Phase::Ready;
                self.update.busy = false;
                self.update.error.clear();
                None
            }
        }
    }

    /// 上一轮拿不到安装根写权时留下的包：再试一次提交。
    ///
    /// 节拍用心跳：交接触发只有用户点安装与自动安装两种，两者本来就挂在心跳上。
    pub(crate) fn resume_commit(&mut self) {
        let Some(part) = self.update.part.take() else {
            return;
        };
        if !part.is_file() {
            // 包被人删了：这一份作废，下一轮心跳重新下发时再下。
            return;
        }
        let Some(owner) = self.own_install_root_for_package(&part) else {
            return;
        };
        let path = match commit_package(&part, &self.config.root) {
            Ok(path) => path,
            Err(err) => {
                drop(owner);
                let _ = std::fs::remove_file(&part);
                self.fail_update(&err);
                self.logf("app", &err);
                return;
            }
        };
        drop(owner);
        self.accept_download(path);
    }

    /// 提交完成之后：记下本地包，按当前请求交接或等自动安装。
    fn accept_download(&mut self, path: PathBuf) {
        // 摘要已经在下载线程里核对过：这里不必再把整个包读一遍。
        self.update.sum = self.update.sha256.clone();
        self.update.path = Some(path);
        self.update.phase = Phase::Ready;
        self.update.busy = false;
        self.update.error.clear();
        let version = self.update.version.clone();
        self.logf(
            "app",
            &mesh_text::format(
                mesh_text::id::ENGINE_LOG_DOWNLOADED_MANUAL_INSTALL,
                &[&version],
            ),
        );
        if self.update.apply_after_download {
            self.update.apply_after_download = false;
            let _ = self.handoff();
        } else {
            self.auto_apply();
        }
    }
}

/// 第 n 次失败之后多久再试：30s 起步翻倍，最多 10 分钟。
///
/// 节拍是心跳（30s 一次），所以这里给的是下限，实际发生时刻在下一轮心跳。
fn retry_delay(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(5);
    Duration::from_secs((30u64 << shift).min(600))
}

/// 下载并校验更新包，落成 `<root>/update.tar.zst.part`。
///
/// 返回的是临时文件，**不是**固定路径：改名由引擎在确认这条结果还算数之后做
/// （[`commit_package`]）。半截文件、被撤回的 offer、被换掉的旧任务都不会在
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
    let Some(sum) = file_digest(&part) else {
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
///
/// 客户端有两次「写安装根」——把下载好的包提交到固定路径、替换根下的启动器。
/// 启动器正在装东西时这两步会踩到它正在换的文件，所以都要在这把锁之内完成。
/// 拿不到就失败（`AlreadyRunning`），由调用方留待下一轮：不排队、不硬写。
fn own_install_root(root: &Path) -> Result<mesh_install::lock::Lock, String> {
    mesh_install::lock::acquire(&mesh_install::lock::Lock::owner_path(root)).map_err(|err| {
        // 锁被占着要说得具体：用户看到的是「安装根正被另一个启动器使用」，
        // 而不是一句泛泛的失败。其它错误原样带出去。
        match err {
            mesh_install::Error::AlreadyRunning => {
                mesh_text::get(mesh_text::id::LAUNCHER_ERROR_INSTALL_ROOT_BUSY).to_string()
            }
            err => err.to_string(),
        }
    })
}

/// 把校验过的临时文件改成固定名：从这一刻起它才是「请安装」的信号。
///
/// 调用方必须持有安装根写权（`own_install_root`）。
pub(crate) fn commit_package(part: &Path, root: &Path) -> Result<PathBuf, String> {
    let dst = root.join(layout::UPDATE_FILE);
    std::fs::rename(part, &dst)
        .map_err(|err| mesh_text::format(mesh_text::id::ENGINE_TEXT_PERSIST_FAILED, &[&err]))?;
    Ok(dst)
}
