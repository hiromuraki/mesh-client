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
use std::time::Duration;

/// 更新包上限，和启动器里的 200MB 对齐。
pub(crate) const MAX_UPDATE_BYTES: u64 = 200 << 20;
/// 下载更新包的总超时（Go 是 20 分钟）。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// 下载启动器的总超时。
const LAUNCHER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 与启动器里的 `LIMIT` 一致：两次还是起不来就说明包本身有问题。
const FAILURE_LIMIT: u32 = 2;
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
            self.logf(
                "app",
                &format!("忽略服务端下发的非法版本号 {:?}", info.version),
            );
            return;
        }
        // 降级防护：发布记录被指回旧包时不要跟着往回装。
        if !self.config.self_version.is_empty()
            && !version::is_newer(&info.version, &self.config.self_version)
        {
            self.logf(
                "app",
                &format!(
                    "忽略不比当前新的版本 {}（当前 {}）",
                    info.version, self.config.self_version
                ),
            );
            return;
        }
        let failures = self.update_failures(&info.version);
        if failures >= FAILURE_LIMIT {
            self.logf(
                "app",
                &format!(
                    "跳过 {}：已连续失败 {failures} 次，需要人工处理",
                    info.version
                ),
            );
            return;
        }
        // 契约里包的地址一律是绝对地址；不是就记一句，不去猜。
        if !(info.url.starts_with("http://") || info.url.starts_with("https://")) {
            self.logf("app", &format!("更新地址不是绝对地址: {:?}", info.url));
            return;
        }
        // 正在下载/交接：这份状态由下载线程独占，心跳不要碰。
        if self.update.busy || self.update.phase == Phase::Downloading {
            return;
        }
        // 同一个版本号重新发布时内容会变，所以“是不是同一份 offer”看摘要，不看版本号：
        // 摘要一致（或服务端没给摘要的早期记录）才认为手上这份就是它。
        let same_offer = self.update.version == info.version
            && (info.sha256.is_empty() || self.update.sha256.eq_ignore_ascii_case(&info.sha256));
        let settled = matches!(self.update.phase, Phase::Ready | Phase::Failed);
        if settled && same_offer {
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
                &format!("服务端报告新版本 {}，开始下载", info.version),
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
            &format!("服务端报告新版本 {}，可在托盘或面板里安装", info.version),
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
            &format!("无人值守：自动安装 {}", self.update.version),
        );
        if let Err(err) = self.apply_update() {
            self.logf("app", &format!("自动安装失败: {err}"));
        }
    }

    /// 用户点了“安装”（或自动安装）：Windows 交接，Unix 先下载再交接。
    pub(crate) fn apply_update(&mut self) -> Result<(), String> {
        if self.update.busy {
            return Err("更新已在进行中".to_string());
        }
        // 客户端自己查得出来的失败不要拖到进程退出之后。
        if !layout::valid_version(&self.update.version) {
            let msg = format!("服务端下发的版本号不合法: {:?}", self.update.version);
            self.fail_update(&msg);
            return Err(msg);
        }
        match self.update.phase {
            Phase::Downloading => return Err("更新还在下载".to_string()),
            // 失败态允许用户直接重试：同样一份包、同一个版本，不必等下一轮心跳。
            Phase::Ready | Phase::Failed => {}
            Phase::Idle => return Err("没有可安装的更新".to_string()),
        }
        if cfg!(windows) {
            // 包已经预取到本地；没有本地包说明还在下载或下载失败过。
            let path = self.update.path.clone();
            if !path.as_deref().map(Path::is_file).unwrap_or(false) {
                return Err("更新包不在本地，等下一轮心跳重新下发".to_string());
            }
            self.update.busy = true;
            self.update.apply_after_download = false;
            return self.handoff();
        }
        let Some(api) = self.api.clone() else {
            return Err("尚未连接服务端，无法下载更新".to_string());
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
                self.logf("app", &format!("{version} 已下载，可在托盘或面板里安装"));
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
                self.logf("app", &format!("下载 {version} 失败: {err}"));
            }
        }
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
                let msg = format!("找不到启动器 {}", launcher.display());
                self.fail_update(&msg);
                return Err(msg);
            }
            let Some(path) = self.update.path.clone() else {
                let msg = "更新包不在本地".to_string();
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
                let msg = format!("启动安装器失败: {err}");
                self.fail_update(&msg);
                return Err(msg);
            }
            self.logf("app", &format!("开始安装 {version}，客户端即将退出"));
            // 必须先把启动器拉起来再退出，否则新版本装不上（F7）。
            self.shutdown.run_and_exit(0);
        }
        // Linux：客户端只负责把包放到安装根，解包与启动交给常驻启动器；正常退出即可。
        self.logf("app", &format!("已下载 {version}，交给启动器安装"));
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
        if info.url.is_empty() || info.sha256.is_empty() {
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
            self.logf("app", &format!("更新启动器失败: {err}"));
            return;
        }
        let api = Arc::clone(api);
        let root = self.config.root.clone();
        let log = Arc::clone(&self.log);
        let version = info.version.clone();
        let url = info.url.clone();
        let want = info.sha256.clone();
        self.logf("app", &format!("下载启动器 {version}"));
        let spawned = thread::Builder::new()
            .name("mesh-launcher-update".to_string())
            .spawn(move || {
                let temp = root.join(UPDATE_DIR).join("launcher.new");
                let _ = std::fs::remove_file(&temp);
                if let Err(err) = api.download(&url, &temp, MAX_UPDATE_BYTES, LAUNCHER_TIMEOUT) {
                    log.add("app", &format!("下载启动器失败: {err}"));
                    return;
                }
                let Some(sum) = file_digest(&temp) else {
                    log.add("app", "校验启动器失败");
                    let _ = std::fs::remove_file(&temp);
                    return;
                };
                if !sum.eq_ignore_ascii_case(&want) {
                    log.add("app", "启动器摘要不符，已放弃");
                    let _ = std::fs::remove_file(&temp);
                    return;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(err) =
                        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
                    {
                        log.add("app", &format!("设置启动器权限失败: {err}"));
                        return;
                    }
                }
                let dst = root.join(mesh_install::platform::launcher_name());
                match std::fs::rename(&temp, &dst) {
                    Ok(()) => log.add("app", &format!("启动器已更新到 {version}")),
                    Err(err) => {
                        log.add("app", &format!("替换启动器失败: {err}"));
                        let _ = std::fs::remove_file(&temp);
                    }
                }
            });
        if spawned.is_err() {
            self.logf("app", "启动启动器更新线程失败");
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
                Ok(()) => self.logf("app", &format!("清理更新临时文件 {name}")),
                Err(err) => self.logf("app", &format!("清理更新临时文件失败: {err}")),
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
            let message = "启动下载线程失败".to_string();
            self.fail_update(&message);
            self.logf("app", &message);
        }
    }
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
        return Err("读取下载结果失败".to_string());
    };
    // 服务端没给摘要就不装：摘要为空时放行等于给「中间人换包」留了唯一一道缝。
    if want.is_empty() {
        let _ = std::fs::remove_file(&part);
        return Err("服务端没有给出包摘要，拒绝安装".to_string());
    }
    if !sum.eq_ignore_ascii_case(want) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("包摘要不符：得到 {sum}"));
    }
    if let Err(err) = std::fs::rename(&part, &dst) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("落盘失败: {err}"));
    }
    Ok(dst)
}
