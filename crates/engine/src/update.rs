//! 客户端自更新：下载、校验摘要、交给启动器安装。
//!
//! 对应 Go 的 `engine/update.go`。三条不能改的行为：
//!
//! - 包先落到 `<root>/update.tar.zst.part`，校验通过才改名成 `update.tar.zst`：固定名字
//!   本身就是「请安装」的信号，半截文件绝不能被当成可安装的包。
//! - **服务端没给摘要就拒绝安装**：摘要为空时放行等于给「中间人换包」留了一道缝。
//! - 失败态保留到换版本或用户重试，否则下一轮心跳会把提示抹掉，界面改口说“已就绪”。

use crate::run::Engine;
use crate::sync::file_digest;
use crate::Cmd;
use mesh_install::{layout, version};
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
    /// 这份 offer 已经失败过几次下载。
    pub attempts: u32,
    /// 下一次自动重试的时刻；退避由它算，不额外排定时器：心跳就是节拍。
    pub retry_at: Option<Instant>,
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
    /// 或被 `clear_stale_update` 清掉）自然就是 `None`。
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
    pub(crate) fn offer_update(
        &mut self,
        api: &Arc<remote::Client>,
        info: Option<&remote::UpdateInfo>,
    ) {
        let Some(info) = info else {
            return;
        };
        // 包级开关每次都更新——包括精简响应：管理员可能刚改过设备级覆盖，而
        // `appUpdate.autoUpdate` 已经是覆盖后的值，客户端照单全收，不再自己判一次。
        self.auto_update = info.auto_update && !self.config.auto_update_disabled;
        // 精简响应：服务端确认“你手上就是这份发布包”。
        //
        // 只认开关，别的什么都不动：不校验 url/版本号（精简形状里本来就没有 url），
        // 不写日志（这正是要省掉的那份每 30 秒重复），不碰 update 的 phase/version/
        // notes/sha256/path（否则会把“已知”错当成“新下发”，把已就绪的包重置成待下载）。
        if info.unchanged {
            return;
        }
        // 版本号会被拼进安装目录里的路径，先按 layout 的规则挡掉非法值。
        if !layout::valid_version(&info.version) {
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
        // 正在下载/交接：这份状态由下载线程独占，心跳不要碰。
        if self.update.busy || self.update.phase == Phase::Downloading {
            return;
        }
        // 同一个版本号重新发布时内容会变，所以“是不是同一份 offer”看摘要，不看版本号。
        let same_offer = self.update.version == info.version
            && self.update.sha256.eq_ignore_ascii_case(&info.sha256);
        let settled = matches!(self.update.phase, Phase::Ready | Phase::Failed);
        if settled && same_offer {
            // 下载失败过的包在退避到期后自动重下一次：发布文件服务短暂不可达时，
            // 不该让提示永远停在错误上，也不该要求用户自己去点重试。
            self.retry_failed_download(api);
            return;
        }
        self.update = UpdateState {
            phase: Phase::Ready,
            version: info.version.clone(),
            notes: info.notes.clone(),
            url: info.url.clone(),
            sha256: info.sha256.clone(),
            ..UpdateState::default()
        };
        // Windows 先把包抓到本地：安装发生在客户端退出之后，那时 mesh 已经断了。
        if cfg!(windows) {
            self.update.phase = Phase::Downloading;
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_DOWNLOAD_STARTED, &[&info.version]),
            );
            self.spawn_download(
                Arc::clone(api),
                info.version.clone(),
                info.url.clone(),
                info.sha256.clone(),
            );
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

    /// 无人值守模式：直接安装刚记录下来的更新。只试一次。
    pub(crate) fn auto_apply(&mut self) {
        if !self.auto_update {
            return;
        }
        if self.update.phase != Phase::Ready || self.update.busy || self.update.version.is_empty() {
            return;
        }
        if self.auto_tried.as_deref() == Some(self.update.version.as_str()) {
            return;
        }
        self.auto_tried = Some(self.update.version.clone());
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
        if !layout::valid_version(&self.update.version) {
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
        if cfg!(windows) {
            // 包已经预取到本地；没有本地包说明下载失败过或被人删掉了。
            // 这时重新下一份、下完直接交接——面板上的「重试」走的就是这条路径。
            let path = self.update.path.clone();
            if !path.as_deref().map(Path::is_file).unwrap_or(false) {
                let Some(api) = self.api.clone() else {
                    return Err(
                        mesh_text::get(mesh_text::id::ENGINE_TEXT_NO_SERVER_FOR_DOWNLOAD)
                            .to_string(),
                    );
                };
                // 用户主动重试：次数重新给满。
                self.update.attempts = 0;
                self.update.retry_at = None;
                self.update.phase = Phase::Downloading;
                self.update.error.clear();
                self.update.apply_after_download = true;
                let version = self.update.version.clone();
                self.logf(
                    "app",
                    &mesh_text::format(
                        mesh_text::id::ENGINE_LOG_REDOWNLOAD_FOR_INSTALL,
                        &[&version],
                    ),
                );
                self.spawn_download(
                    api,
                    version,
                    self.update.url.clone(),
                    self.update.sha256.clone(),
                );
                return Ok(());
            }
            self.update.busy = true;
            self.update.apply_after_download = false;
            return self.handoff();
        }
        let Some(api) = self.api.clone() else {
            return Err(
                mesh_text::get(mesh_text::id::ENGINE_TEXT_NO_SERVER_FOR_DOWNLOAD).to_string(),
            );
        };
        self.update.busy = true;
        self.update.phase = Phase::Downloading;
        self.update.error.clear();
        self.update.apply_after_download = true;
        self.spawn_download(
            api,
            self.update.version.clone(),
            self.update.url.clone(),
            self.update.sha256.clone(),
        );
        Ok(())
    }

    /// 下载线程把包落到 `<root>/update.tar.zst` 后回报。
    pub(crate) fn on_download(&mut self, version: String, result: Result<PathBuf, String>) {
        if self.update.version != version {
            // 下载期间服务端又下发了别的版本，这次的结果作废。
            return;
        }
        match result {
            Ok(path) => {
                self.update.sum = file_digest(&path).unwrap_or_default();
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
    fn retry_failed_download(&mut self, api: &Arc<remote::Client>) {
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
        self.update.phase = Phase::Downloading;
        self.update.error.clear();
        self.update.retry_at = None;
        let attempt = self.update.attempts + 1;
        self.logf(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_REDOWNLOAD, &[&version, &attempt]),
        );
        self.spawn_download(
            Arc::clone(api),
            version,
            self.update.url.clone(),
            self.update.sha256.clone(),
        );
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

    /// 清掉“服务端已经不再提供”的更新提示。下载中或正在交接时不动。
    pub(crate) fn clear_stale_update(&mut self) {
        if self.update.busy
            || self.update.phase == Phase::Downloading
            || self.update.phase == Phase::Idle
        {
            return;
        }
        self.update = UpdateState::default();
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
        // 判重与占位在同一处完成：同一台机器只尝试一次，换不掉就等下个版本。
        if self.launcher_sum.eq_ignore_ascii_case(&info.sha256)
            || self.launcher_tried.as_deref() == Some(info.sha256.as_str())
        {
            return;
        }
        self.launcher_tried = Some(info.sha256.clone());
        let dir = self.config.root.join(UPDATE_DIR);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATE_FAILED, &[&err]),
            );
            return;
        }
        let api = Arc::clone(api);
        let root = self.config.root.clone();
        let log = Arc::clone(&self.log);
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
                let dst = root.join(mesh_install::platform::launcher_name());
                match std::fs::rename(&temp, &dst) {
                    Ok(()) => log.add(
                        "app",
                        &mesh_text::format(mesh_text::id::ENGINE_LOG_LAUNCHER_UPDATED, &[&version]),
                    ),
                    Err(err) => {
                        log.add(
                            "app",
                            &mesh_text::format(
                                mesh_text::id::ENGINE_LOG_LAUNCHER_REPLACE_FAILED,
                                &[&err],
                            ),
                        );
                        let _ = std::fs::remove_file(&temp);
                    }
                }
            });
        if spawned.is_err() {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_LAUNCHER_THREAD_FAILED),
            );
        }
    }

    /// 清掉 `update/` 里上一轮留下的临时文件。
    pub(crate) fn clean_update_dir(&mut self) {
        let dir = self.config.root.join(UPDATE_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            match std::fs::remove_dir_all(&path) {
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

    fn spawn_download(
        &mut self,
        api: Arc<remote::Client>,
        version: String,
        url: String,
        want: String,
    ) {
        let root = self.config.root.clone();
        let tx = self.self_tx.clone();
        let spawned = thread::Builder::new()
            .name("mesh-download".to_string())
            .spawn(move || {
                let result = download_update(&api, &root, &url, &want);
                let _ = tx.send(Cmd::Download { version, result });
            });
        if spawned.is_err() {
            let message =
                mesh_text::get(mesh_text::id::ENGINE_TEXT_DOWNLOAD_THREAD_FAILED).to_string();
            self.fail_update(&message);
            self.logf("app", &message);
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

/// 下载并校验更新包：落 `.part` → 算摘要 → 改名成固定的 `update.tar.zst`。
pub(crate) fn download_update(
    api: &remote::Client,
    root: &Path,
    url: &str,
    want: &str,
) -> Result<PathBuf, String> {
    let dst = root.join(layout::UPDATE_FILE);
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
    if let Err(err) = std::fs::rename(&part, &dst) {
        let _ = std::fs::remove_file(&part);
        return Err(mesh_text::format(
            mesh_text::id::ENGINE_TEXT_PERSIST_FAILED,
            &[&err],
        ));
    }
    Ok(dst)
}
