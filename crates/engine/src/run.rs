//! 引擎线程：连接循环、命令处理、core 事件与退避重拉。
//!
//! 只有这个线程读写 [`Engine`] 里的状态。别的线程（心跳、STUN、下载、面板）只能通过
//! [`Cmd`] 投递结果、通过 [`StateCell`] 读快照。
//!
//! 连接循环的形状与 Go 的 `engine.run` 一致，只是“等退避”换成了 inbox 的
//! `recv_timeout`：等待期间设置请求、配置下发、core 事件照常处理，退避也不占线程。

use crate::autostart;
use crate::clock;
use crate::log::Log;
use crate::views::{Snapshot, StateCell};
use crate::{coreline, sync, update, Reply, Shutdown};
use mesh_install::layout;
use mesh_remote as remote;
use mesh_state::hints::ConfigHints;
use mesh_state::{ClientConfig, Store};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use crate::settings::SettingsInput;

/// 心跳间隔。
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 单次心跳的 HTTP 超时（Go 是 20s）。
pub(crate) const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(20);
/// 连接失败后的退避上限（Go：10s 起步，每次 ×1.5，最多 60s）。
const CONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// core 活过这么久才算“这次启动成功”，否则继续放大重拉退避。
pub(crate) const CORE_MIN_LIFETIME: Duration = Duration::from_secs(5);
/// 重拉 core 的退避上限（1s 起步翻倍，最多 30s）。
const REVIVE_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// 等 core 停止回复的上限。core 内部另有 5s 软停 + 3s 强杀的预算。
const STOP_REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// STUN 结果的刷新间隔。
pub(crate) const STUN_REFRESH_INTERVAL: Duration = Duration::from_secs(20);

/// 面向界面的连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unconfigured,
    Connecting,
    Online,
    Degraded,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Unconfigured => "unconfigured",
            Status::Connecting => "connecting",
            Status::Online => "online",
            Status::Degraded => "degraded",
            Status::Error => "error",
        }
    }
}

/// 引擎的入队点。后台路径只是“把结果交回来”，所有判断都在引擎线程里做。
pub enum Cmd {
    /// 心跳线程只负责取样，401 的分类处理在引擎里。
    Heartbeat(Result<Option<remote::Heartbeat>, remote::Error>),
    Core(mesh_core::Event),
    Stun(mesh_etrpc::StunInfo),
    Download {
        version: String,
        result: Result<PathBuf, String>,
    },
    Settings(SettingsInput, Reply<Result<Snapshot, String>>),
    Reset(Reply<Result<Snapshot, String>>),
    SetAutostart(bool, Reply<Result<Snapshot, String>>),
    ApplyUpdate(Reply<Result<(), String>>),
    Reconnect,
}

/// 用户动作失败（保存设置、切换自启、应用更新）由 client 弹一次框。
pub enum Notice {
    ActionFailed(String),
}

/// 引擎配置。版本号由 `build.rs` 注入后传进来，不从磁盘读。
pub struct Config {
    pub root: PathBuf,
    pub self_version: String,
    /// 本机主机名：配置里没写设备名时用它。
    pub hostname: String,
    /// `--disable-auto-update`：本地一票否决服务端的包级开关。
    pub auto_update_disabled: bool,
}

/// 一次 core 代管。`gen` 是引擎分配的代号，事件按它归并。
pub(crate) struct CoreRun {
    pub gen: u64,
    pub pid: u32,
    /// 这一代 core 实际加载的凭据 id（正式节点为空串）。
    pub cred_id: String,
    pub rpc_addr: SocketAddr,
    /// 收到 `Started` 之后才是“真的起来了”。
    pub started: bool,
    /// 就绪时刻（unix 秒），给快照用。
    pub started_unix: i64,
}

/// 待执行的一次重拉。
pub(crate) struct Revive {
    pub attempt: u32,
    pub due: Instant,
}

/// 本机身份摘要（认证方式、凭据过期、配置时间、加密算法）。
#[derive(Debug, Clone, Default)]
pub(crate) struct IdentityMeta {
    pub mode: String,
    pub cred_expiry: i64,
    pub config_at: i64,
    pub encryption: String,
}

pub(crate) const AUTH_FORMAL: &str = "formal";
pub(crate) const AUTH_CREDENTIAL: &str = "credential";
/// 「本地没有凭据」的显式声明；空串留给正式节点（服务端据此不签发）。
pub(crate) const NO_CREDENTIAL_ID: &str = "none";

pub(crate) struct Engine {
    pub(crate) config: Config,
    pub(crate) store: Store,
    pub(crate) cell: Arc<StateCell>,
    pub(crate) notices: Sender<Notice>,
    pub(crate) shutdown: Arc<Shutdown>,
    pub(crate) log: Arc<Log>,
    /// 日志打不开时的原因：快照的 lastError 会带上它（与 Go 的 logErr 一致）。
    pub(crate) log_error: String,

    pub(crate) inbox: Receiver<Cmd>,
    pub(crate) self_tx: Sender<Cmd>,
    pub(crate) core_tx: Sender<mesh_core::Cmd>,

    pub(crate) status: Status,
    pub(crate) last_error: String,
    pub(crate) client: Option<ClientConfig>,
    pub(crate) api: Option<Arc<remote::Client>>,
    pub(crate) last_hb: i64,
    pub(crate) core: Option<CoreRun>,
    /// 上一代 core 就绪的时刻：重拉档位靠它判断“刚起来就死”。
    pub(crate) last_core_started_at: Option<Instant>,
    /// 连续短命重启的档位（Go 的 `reviveAttempt`），上一代活够 5s 就回到 1。
    pub(crate) revive_attempt: u32,
    pub(crate) next_gen: u64,
    pub(crate) revive: Option<Revive>,
    pub(crate) auth_fails: u32,
    pub(crate) update: update::UpdateState,
    pub(crate) auto_update: bool,
    pub(crate) auto_tried: Option<String>,
    pub(crate) launcher_sum: String,
    pub(crate) launcher_tried: Option<String>,
    /// 开机自启任务是否存在。查询要起一个 `schtasks` 进程，所以缓存起来，由
    /// [`Engine::refresh_autostart`] 在启动、改动与每轮心跳后刷新。
    pub(crate) autostart_on: bool,
    pub(crate) identity: IdentityMeta,
    pub(crate) hints: ConfigHints,
    /// 验发布签名用的公钥（hex）。生产就是编进 remote 的那一把，测试换成自己
    /// 生成的密钥对——发布私钥不在这台机器上，只有这样才能测到接受路径。
    pub(crate) release_key: String,
    pub(crate) local_ip: String,
    pub(crate) core_version: String,
    pub(crate) core_path_used: String,
    pub(crate) network_name: String,
    pub(crate) stun: mesh_etrpc::StunInfo,
    pub(crate) hb_stop: Option<Sender<()>>,
    /// STUN 刷新线程的停止通道。与心跳一样按连接循环的生命周期管理：每次重连新起
    /// 一个而不停旧的，线程就会按心跳的节奏无限累积（core 长期起不来时 2 个/分钟）。
    pub(crate) stun_stop: Option<Sender<()>>,
    pub(crate) closed: bool,
}

/// 起引擎线程，返回它的命令入口与线程句柄。
///
/// 与 MODULES.md 的签名差异：引擎要给自己投递心跳结果与 core 事件，必须持有
/// `Sender<Cmd>`，所以这里自己建 channel 并把发送端交出去（调用方只拿 Sender）。
///
/// 同步注册一步收尾：`Shutdown::run_and_exit` 时先停 core。
pub fn spawn(
    config: Config,
    cell: Arc<StateCell>,
    notices: Sender<Notice>,
    shutdown: Arc<Shutdown>,
) -> (Sender<Cmd>, JoinHandle<()>) {
    let (tx, inbox) = channel();
    // 打不开 `config/` 就没法做任何事，也不该假装能跑：这里直接让进程死掉，
    // 由启动器按「启动确认失败」回滚（桌面端还会弹一次错误框）。
    let store = match Store::open(&config.root) {
        Ok(store) => store,
        Err(err) => panic!("打开状态目录失败: {err}"),
    };
    let log_path = config.root.join(layout::LOG).join("client.log");
    let (log, log_error) = match Log::open(&log_path) {
        Ok(log) => (log, String::new()),
        Err(err) => (
            Log::unavailable(&log_path),
            mesh_text::format(mesh_text::id::ENGINE_TEXT_LOG_UNAVAILABLE, &[&err]),
        ),
    };
    let log = Arc::new(log);
    // 安装根只留 Administrators 与 SYSTEM 可写：不收紧时任何本地账户都能替换
    // `mesh.exe`，而自启任务会以最高权限拉起它。失败不拦启动，但要留痕。
    if let Err(err) = mesh_state::harden_install_root(&config.root) {
        log.add(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_HARDEN_FAILED, &[&err]),
        );
    }
    // 收紧 `config/` 权限失败同样只记一条：密钥文件可能仍带着继承来的宽权限，
    // 而用户看不到任何异常。
    if let Some(err) = store.harden_error() {
        log.add(
            "app",
            &mesh_text::format(mesh_text::id::ENGINE_LOG_HARDEN_FAILED, &[&err]),
        );
    }

    // core 事件包成 Cmd::Core 投回引擎；core 输出的每一行先去掉时间戳、打码再落盘。
    let inbox_tx = tx.clone();
    let log_for_core = Arc::clone(&log);
    let core_tx = mesh_core::spawn(
        move |event| {
            let _ = inbox_tx.send(Cmd::Core(event));
        },
        move |line| {
            log_for_core.add("core", &coreline::clean(line));
        },
    );

    // Shutdown 里先停 core：这一步可能从引擎线程（更新交接）或 client 线程触发。
    let stop_tx = core_tx.clone();
    shutdown.register(move || {
        let (reply_tx, reply_rx) = channel();
        if stop_tx
            .send(mesh_core::Cmd::Stop { reply: reply_tx })
            .is_ok()
        {
            let _ = reply_rx.recv_timeout(STOP_REPLY_TIMEOUT);
        }
    });

    let launcher_sum =
        sync::file_digest(&config.root.join(mesh_install::platform::launcher_name()))
            .unwrap_or_default();
    let engine = Engine {
        store,
        cell,
        notices,
        shutdown,
        log,
        log_error,
        self_tx: tx.clone(),
        inbox,
        core_tx,
        status: Status::Unconfigured,
        last_error: String::new(),
        client: None,
        api: None,
        last_hb: 0,
        core: None,
        last_core_started_at: None,
        revive_attempt: 0,
        next_gen: 0,
        revive: None,
        auth_fails: 0,
        update: update::UpdateState::default(),
        auto_update: false,
        auto_tried: None,
        launcher_sum,
        launcher_tried: None,
        autostart_on: false,
        identity: IdentityMeta::default(),
        hints: ConfigHints::default(),
        release_key: remote::signing::RELEASE_PUBLIC_KEY.to_string(),
        local_ip: String::new(),
        core_version: String::new(),
        core_path_used: String::new(),
        network_name: String::new(),
        stun: mesh_etrpc::StunInfo::default(),
        hb_stop: None,
        stun_stop: None,
        closed: false,
        config,
    };
    let handle = thread::Builder::new()
        .name("mesh-engine".to_string())
        .spawn(move || engine.run())
        .expect("启动引擎线程失败");
    (tx, handle)
}

/// `pump` 的结果。
enum Flow {
    /// 给的时间用完了（该做下一件事，比如重连或重拉 core）。
    Timeout,
    /// 收到“要重来一遍连接循环”的命令（设置、重置、手动重连）。
    Restart,
    /// 命令端全部断开（进程要退了）。
    Closed,
}

impl Engine {
    fn run(mut self) {
        // 第一行永远写清自己在跑哪个版本：headless 上只有日志可看。
        self.logf(
            "app",
            &mesh_text::format(
                mesh_text::id::ENGINE_LOG_CLIENT_STARTED,
                &[&self.config.self_version],
            ),
        );
        self.clean_update_dir();
        self.refresh_identity();
        self.refresh_autostart();
        self.publish();
        // 先用磁盘上那份配置把 core 拉起来，再进连接循环：控制面不可达只该让状态
        // 降级，不该把数据面一起停掉。
        self.start_core_from_disk();
        self.connection_loop();
        self.stop_heartbeat();
        self.stop_stun_loop();
        let _ = self.stop_core();
        self.publish();
        self.log.close();
    }

    fn connection_loop(&mut self) {
        let mut connect_failures = 0u32;
        loop {
            let config = match self.load_client() {
                sync::Loaded::Failed(err) => {
                    self.set_client(None);
                    let msg = mesh_text::format(
                        mesh_text::id::ENGINE_LOG_LOCAL_STATE_READ_FAILED,
                        &[&err],
                    );
                    self.set_status(Status::Error, msg.clone());
                    self.logf("app", &msg);
                    if !self.wait(Duration::from_secs(5)) {
                        return;
                    }
                    continue;
                }
                sync::Loaded::Missing => {
                    self.set_client(None);
                    // 无 client.json / 无 device_id：设置页该出场了，不需要重连。
                    let last = self.last_error.clone();
                    self.set_status(Status::Unconfigured, last);
                    if !self.wait(Duration::from_secs(2)) {
                        return;
                    }
                    continue;
                }
                sync::Loaded::Configured(config) => config,
            };
            self.set_client(Some(config.clone()));
            self.set_status(Status::Connecting, String::new());
            self.publish();
            match self.connect(&config) {
                Err(sync::ConnectError::IdentityInvalid) => continue,
                Err(sync::ConnectError::Failed(err)) => {
                    if self.closed {
                        return;
                    }
                    connect_failures += 1;
                    self.set_status(Status::Error, err.clone());
                    self.logf(
                        "app",
                        &mesh_text::format(mesh_text::id::ENGINE_LOG_CONNECT_FAILED, &[&err]),
                    );
                    if !self.wait(connect_backoff(connect_failures)) {
                        return;
                    }
                    continue;
                }
                Ok(()) => {}
            }
            connect_failures = 0;
            // core 起不来（比如还没拿到凭据）不是连接失败：控制面已经通了，心跳要继续
            // 跑，否则凭据永远下不来。状态保留启动失败记下的原因。
            if self.core_running() {
                self.mark_online();
            }
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_CONNECTED_SERVER,
                    &[&config.server_url],
                ),
            );
            self.start_heartbeat();
            self.start_stun_loop();
            self.publish();
            // 连接建立之后就一直在这里处理命令，直到需要重连或退出。
            loop {
                match self.pump(self.next_deadline()) {
                    Flow::Timeout => self.on_deadline(),
                    Flow::Restart => break,
                    Flow::Closed => {
                        self.closed = true;
                        self.stop_heartbeat();
                        self.stop_stun_loop();
                        return;
                    }
                }
            }
            self.stop_heartbeat();
            self.stop_stun_loop();
            if self.closed {
                return;
            }
        }
    }

    /// 处理 inbox 直到 `timeout` 用完（`None` 表示一直等），期间每条消息都会发布快照。
    fn pump(&mut self, timeout: Option<Duration>) -> Flow {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            // 收尾已经开始：退出只有一条路（Shutdown），引擎不必再处理任何命令。
            if self.shutdown.triggered() {
                return Flow::Closed;
            }
            let wait = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Flow::Timeout;
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            let received = match wait {
                Some(wait) => self.inbox.recv_timeout(wait),
                None => self
                    .inbox
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(cmd) => {
                    if self.handle(cmd) {
                        return Flow::Restart;
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Flow::Timeout,
                Err(RecvTimeoutError::Disconnected) => return Flow::Closed,
            }
            self.publish();
        }
    }

    /// 等一段时间，期间照常处理命令。返回 false 表示命令端断开、该退出了。
    fn wait(&mut self, wait: Duration) -> bool {
        match self.pump(Some(wait)) {
            Flow::Closed => {
                self.closed = true;
                false
            }
            // 等待期间收到的“重连”请求：这次等待提前结束，按原来的路径重试即可。
            Flow::Timeout | Flow::Restart => true,
        }
    }

    /// 下一次该被唤醒的时刻：当前只有重拉 core 的退避。
    fn next_deadline(&self) -> Option<Duration> {
        let revive = self.revive.as_ref()?;
        Some(revive.due.saturating_duration_since(Instant::now()))
    }

    /// 到点了：尝试重拉 core。失败就再排一次（退避在**每次尝试之前**等）。
    fn on_deadline(&mut self) {
        let Some(revive) = self.revive.take() else {
            return;
        };
        if Instant::now() < revive.due {
            self.revive = Some(revive);
            return;
        }
        match self.start_core(false) {
            Ok(()) => {
                // 等 Started 事件；这一步只说明“已经把启动请求发出去了”。
                self.logf(
                    "app",
                    mesh_text::get(mesh_text::id::ENGINE_LOG_CORE_REVIVED),
                );
            }
            Err(err) => {
                let msg = mesh_text::format(
                    mesh_text::id::ENGINE_LOG_CORE_START_FAILED_ATTEMPT,
                    &[&revive.attempt, &err],
                );
                self.set_status(Status::Degraded, msg.clone());
                self.logf("app", &msg);
                self.schedule_revive(revive.attempt + 1);
            }
        }
    }

    /// 处理一条命令；返回 true 表示要重新走一遍连接循环。
    pub(crate) fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Heartbeat(result) => {
                self.on_heartbeat(result);
                false
            }
            Cmd::Core(event) => {
                self.on_core_event(event);
                false
            }
            Cmd::Stun(stun) => {
                self.stun = stun;
                false
            }
            Cmd::Download { version, result } => {
                self.on_download(version, result);
                false
            }
            Cmd::Settings(input, reply) => {
                let result = self.save_settings(input);
                let restart = result.is_ok();
                if let Err(err) = &result {
                    self.notify(&mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_SAVE_SETTINGS_FAILED,
                        &[&err],
                    ));
                }
                let snapshot = self.snapshot();
                let _ = reply.send(result.map(|()| snapshot).map_err(|err| err.to_string()));
                restart
            }
            Cmd::Reset(reply) => {
                let result = self.reset_config();
                let restart = result.is_ok();
                let snapshot = self.snapshot();
                let _ = reply.send(result.map(|()| snapshot).map_err(|err| err.to_string()));
                restart
            }
            Cmd::SetAutostart(enabled, reply) => {
                let result = self.set_autostart(enabled);
                if let Err(err) = &result {
                    self.notify(&mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_SET_AUTOSTART_FAILED,
                        &[&err],
                    ));
                }
                let snapshot = self.snapshot();
                let _ = reply.send(result.map(|()| snapshot).map_err(|err| err.to_string()));
                false
            }
            Cmd::ApplyUpdate(reply) => {
                let result = self.apply_update();
                let _ = reply.send(result);
                false
            }
            Cmd::Reconnect => {
                self.logf(
                    "app",
                    mesh_text::get(mesh_text::id::ENGINE_LOG_RECONNECT_MANUAL),
                );
                true
            }
        }
    }

    /// 心跳结果的处理：恢复状态、下发配置/更新/凭据、必要时让主循环重连。
    pub(crate) fn on_heartbeat(
        &mut self,
        result: Result<Option<remote::Heartbeat>, remote::Error>,
    ) {
        let api = match self.api.clone() {
            Some(api) => api,
            None => return,
        };
        let heartbeat = match result {
            Ok(heartbeat) => heartbeat,
            Err(err) => {
                if matches!(err, remote::Error::Unauthorized { .. }) {
                    // 401 不再一律清身份：吊销立即清，时钟偏差只降级提示，其余连续多次才清。
                    if self.handle_auth_rejected(&err) {
                        self.closed = false;
                    }
                    return;
                }
                let msg = mesh_text::format(mesh_text::id::ENGINE_LOG_HEARTBEAT_FAILED, &[&err]);
                self.logf("app", &msg);
                self.degrade(&msg);
                return;
            }
        };
        self.reset_auth_fails();
        self.last_hb = clock::unix_now();
        // 心跳成功且 core 在跑，说明链路是通的：error 多半来自上一次失败的配置同步，
        // 而服务端把配置改回去之后不会再有任何事件把状态拉回 online。
        let recovered = if self.core_running()
            && (self.status == Status::Degraded || self.status == Status::Error)
        {
            self.status = Status::Online;
            self.last_error.clear();
            true
        } else {
            false
        };
        if recovered {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_HEARTBEAT_RECOVERED),
            );
        }
        let res = heartbeat.unwrap_or_default();
        if res.app_update.is_some() {
            self.offer_update(&api, res.app_update.as_ref());
            self.auto_apply();
        } else {
            self.clear_stale_update();
        }
        // 启动器单独一路：控制面每次都带，客户端自己比摘要；下载放后台跑。
        self.apply_launcher(&api, res.launcher.as_ref());
        // 凭据：服务端只在签发的那一刻带私钥，之后就只剩 id。
        if self.apply_credential(&res) {
            self.trigger_restart();
        }
        if !res.config_hash.is_empty() && res.config_hash != self.hash() {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_CONFIG_CHANGED_SYNC),
            );
            self.apply_remote(&res);
        }
        // 自启状态只在启动、用户改动和这里刷新：快照两秒一次，不可能每次都去起进程。
        // 用心跳当节拍也顺带接住“任务被组策略或用户从系统里删掉”这种情况。
        self.refresh_autostart();
        // 心跳通了、core 却没在跑：让主循环重新走一遍 connect，它会补启动 core。
        // 缺凭据就是这一类——凭据只能从心跳里来，重试必须挂在心跳上。
        if !self.core_running() {
            self.trigger_restart();
        }
    }

    /// core 事件：只认当前这一代，其余都是主动停止/替换留下的迟到事件。
    pub(crate) fn on_core_event(&mut self, event: mesh_core::Event) {
        match event {
            mesh_core::Event::Started { gen, pid, node } => {
                let Some(core) = self.core.as_mut() else {
                    return;
                };
                if core.gen != gen {
                    return;
                }
                core.pid = pid;
                core.started = true;
                core.started_unix = clock::unix_now();
                self.last_core_started_at = Some(Instant::now());
                self.local_ip = strip_cidr(&node.ipv4).to_string();
                self.core_version = node.version.clone();
                self.stun = node.stun.clone();
                if !self.hints.network_name.is_empty() {
                    self.network_name = self.hints.network_name.clone();
                }
                self.revive = None;
                if self.api.is_some() {
                    self.mark_online();
                }
                self.logf(
                    "app",
                    &mesh_text::format(
                        mesh_text::id::ENGINE_LOG_CORE_READY,
                        &[&pid, &self.local_ip],
                    ),
                );
            }
            mesh_core::Event::StartFailed { gen, reason } => {
                if !self.core_matches(gen) {
                    return;
                }
                self.core = None;
                let msg =
                    mesh_text::format(mesh_text::id::ENGINE_LOG_CORE_START_FAILED, &[&reason]);
                self.set_status(Status::Error, msg.clone());
                self.logf("app", &msg);
                let attempt = self.on_start_failed_attempt();
                self.schedule_revive(attempt);
            }
            mesh_core::Event::Exited { gen, err } => {
                if !self.core_matches(gen) {
                    return;
                }
                self.core = None;
                let detail = err.unwrap_or_default();
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    mesh_text::format(mesh_text::id::ENGINE_TEXT_EXITED_DETAIL, &[&detail])
                };
                self.logf(
                    "app",
                    &mesh_text::format(mesh_text::id::ENGINE_LOG_CORE_EXITED, &[&suffix]),
                );
                self.set_status(
                    Status::Degraded,
                    mesh_text::get(mesh_text::id::ENGINE_TEXT_CORE_NOT_RUNNING_RESTARTING)
                        .to_string(),
                );
                let attempt = self.next_revive_attempt();
                self.schedule_revive(attempt);
            }
        }
    }

    /// 重拉 core 的退避：1s 起步翻倍，最多 30s。
    pub(crate) fn schedule_revive(&mut self, attempt: u32) {
        let attempt = attempt.max(1);
        self.revive = Some(Revive {
            attempt,
            due: Instant::now() + revive_backoff(attempt),
        });
    }

    /// 本次重拉该用第几档退避：上一代 core 活得够久才回到第一档。
    ///
    /// 判据是 `last_core_started_at`（上一次**成功就绪**的时刻），不是“当前这一代”：
    /// 崩溃之后 `core` 已经被清空，拿当前代判断会把档位永远重置成 1——那正是
    /// “短命 core 把退避冲掉”的现场。
    pub(crate) fn next_revive_attempt(&mut self) -> u32 {
        let short_lived = self
            .last_core_started_at
            .map(|at| at.elapsed() < CORE_MIN_LIFETIME)
            .unwrap_or(false);
        self.revive_attempt = if short_lived {
            // 刚起来就死：档位只增不减，避免重启节奏被 core 自己的退出速度决定。
            self.revive_attempt.saturating_add(1).max(1)
        } else {
            1
        };
        self.revive_attempt
    }

    /// 启动失败（core 根本没起来）也要放大档位：Go 的 `reviveCore` 每失败一次就
    /// `attempt++`，否则“每次都在 1 秒重试”会变成每秒拉起一个进程。
    pub(crate) fn on_start_failed_attempt(&mut self) -> u32 {
        self.revive_attempt = self.revive_attempt.saturating_add(1).max(1);
        self.revive_attempt
    }

    pub(crate) fn trigger_restart(&mut self) {
        // 用一条自己的命令把“重连”插到队尾：调用点遍布各个 handler，直接 return true
        // 会把正在处理的命令处理到一半。
        let _ = self.self_tx.send(Cmd::Reconnect);
    }

    /// 重读系统里的自启状态。
    ///
    /// 查询失败时保留上一次的值：读不出来不等于用户关掉了它，把复选框自己跳回未选中
    /// 只会让人以为设置丢了。
    pub(crate) fn refresh_autostart(&mut self) {
        match autostart::enabled() {
            Ok(on) => self.autostart_on = on,
            Err(err) => self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_READ_AUTOSTART_FAILED, &[&err]),
            ),
        }
    }

    /// 打开或关闭开机自启。托盘与面板共用这一条路径。
    pub(crate) fn set_autostart(&mut self, on: bool) -> Result<(), String> {
        if on {
            let launcher = self
                .config
                .root
                .join(mesh_install::platform::launcher_name());
            autostart::enable(&launcher)?;
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_AUTOSTART_ENABLED),
            );
        } else {
            autostart::disable()?;
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_AUTOSTART_DISABLED),
            );
        }
        self.refresh_autostart();
        Ok(())
    }

    fn notify(&self, message: &str) {
        let _ = self.notices.send(Notice::ActionFailed(message.to_string()));
    }

    pub(crate) fn set_status(&mut self, status: Status, error: String) {
        self.status = status;
        self.last_error = error;
    }

    fn set_client(&mut self, client: Option<ClientConfig>) {
        self.client = client;
    }

    pub(crate) fn logf(&self, source: &str, message: &str) {
        self.log.add(source, message);
    }

    /// 心跳通了、core 也在跑：状态置为在线。
    pub(crate) fn mark_online(&mut self) {
        self.status = Status::Online;
        self.last_error.clear();
    }

    /// 只把状态降级，不动错误行（已经在在线/连接中才降）。
    pub(crate) fn degrade(&mut self, message: &str) {
        if matches!(self.status, Status::Online | Status::Connecting) {
            self.status = Status::Degraded;
            self.last_error = message.to_string();
        }
    }

    pub(crate) fn core_running(&self) -> bool {
        self.core.as_ref().map(|core| core.started).unwrap_or(false)
    }

    fn core_matches(&self, gen: u64) -> bool {
        self.core
            .as_ref()
            .map(|core| core.gen == gen)
            .unwrap_or(false)
    }

    pub(crate) fn core_rpc_addr(&self) -> Option<SocketAddr> {
        self.core
            .as_ref()
            .filter(|core| core.started)
            .map(|core| core.rpc_addr)
    }

    /// 磁盘上 `config.toml` 的摘要；没有配置时为空串。
    ///
    /// 每次现算而不是记住服务端给过的值：文件被外部改坏时算出来的摘要就和服务端
    /// 记录的对不上，下一轮心跳会把正确配置重新要回来。
    pub(crate) fn hash(&self) -> String {
        match self.store.read_config() {
            Ok(Some(toml)) => config_digest(&toml),
            _ => String::new(),
        }
    }

    /// 停止 core：等它真的退出，失败就报出来（不能继续起第二个抢同一个虚拟 IP）。
    pub(crate) fn stop_core(&mut self) -> Result<(), String> {
        let Some(core) = self.core.take() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = channel();
        if self
            .core_tx
            .send(mesh_core::Cmd::Stop { reply: reply_tx })
            .is_err()
        {
            return Ok(());
        }
        match reply_rx.recv_timeout(STOP_REPLY_TIMEOUT) {
            Ok(Ok(())) => {
                self.local_ip.clear();
                Ok(())
            }
            Ok(Err(err)) => {
                self.logf(
                    "app",
                    &mesh_text::format(mesh_text::id::ENGINE_LOG_STOP_CORE_FAILED, &[&err]),
                );
                self.core = Some(core);
                Err(err)
            }
            Err(_) => {
                let err = mesh_text::format(
                    mesh_text::id::ENGINE_TEXT_STOP_CORE_TIMEOUT,
                    &[&STOP_REPLY_TIMEOUT.as_secs()],
                );
                self.logf("app", &err);
                self.core = Some(core);
                Err(err)
            }
        }
    }

    /// 按磁盘上的配置启动 core（连接控制面之前）。
    fn start_core_from_disk(&mut self) {
        let config = match self.store.load_client() {
            Ok(Some(config)) if !config.device_id.is_empty() => config,
            _ => return,
        };
        self.set_client(Some(config.clone()));
        if self.start_core_or_report(false) {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_CORE_STARTED_FROM_DISK),
            );
        }
    }

    /// 启动 core，失败只记录不向上抛：core 是数据面，控制面不可达不该让它跟着停。
    pub(crate) fn start_core_or_report(&mut self, replace: bool) -> bool {
        match self.start_core(replace) {
            Ok(()) => true,
            Err(err) => {
                let msg = mesh_text::format(mesh_text::id::ENGINE_LOG_CORE_START_FAILED, &[&err]);
                self.set_status(Status::Error, msg.clone());
                self.logf("app", &msg);
                false
            }
        }
    }

    /// 启动一代 core。`replace` 为真表示调用方刚换过配置：即使已经有 core 在跑
    /// （可能是崩溃恢复抢先拉起的、读的还是旧配置），也要停掉重起。
    pub(crate) fn start_core(&mut self, replace: bool) -> Result<(), String> {
        if self.core.is_some() {
            if !replace {
                return Ok(());
            }
            self.stop_core()?;
        }
        let Some(config) = self.client.clone() else {
            return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_NOT_REGISTERED).to_string());
        };
        // 提示字段现读现算（Go 的 hints() 也是每次读文件）：配置刚被写盘时，
        // 就绪判据与身份选择都必须基于新内容。
        self.refresh_identity();
        let core_path = mesh_core::locate(Path::new(&config.core_path))?;
        let rpc_addr = sync::rpc_listen_addr()?;
        let hostname = self.effective_hostname(&config);
        let hints = self.hints.clone();
        // 身份二选一，由配置里有没有 network_secret 决定：正式节点（含管理节点）用
        // secure_mode 的本地私钥，忽略本地凭据；其余节点只能用凭据，没有就不起 core。
        let (identity, cred_id) = if hints.formal() {
            let key = self.store.secure_key().map_err(|err| {
                mesh_text::format(mesh_text::id::ENGINE_TEXT_READ_NODE_KEY_FAILED, &[&err])
            })?;
            (mesh_core::Identity::Formal { key }, String::new())
        } else {
            match self.store.load_credential() {
                Ok(Some(credential)) => (
                    mesh_core::Identity::Credential {
                        id: credential.id.clone(),
                        secret: credential.secret,
                    },
                    credential.id,
                ),
                Ok(None) => return Err(sync::no_credential()),
                Err(err) => {
                    return Err(mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_READ_CREDENTIAL_FAILED,
                        &[&err],
                    ))
                }
            }
        };
        let spec = mesh_core::StartSpec {
            gen: self.next_gen.wrapping_add(1),
            core_path: core_path.clone(),
            config_path: self.store.config_path(),
            rpc_addr,
            hostname: hostname.clone(),
            rpc_whitelist: sync::rpc_whitelist(),
            identity,
            expect: mesh_core::Expect {
                hostname: hints.hostname.clone(),
                ipv4: hints.ipv4.clone(),
            },
        };
        self.next_gen = spec.gen;
        self.core = Some(CoreRun {
            gen: spec.gen,
            pid: 0,
            cred_id,
            rpc_addr,
            started: false,
            started_unix: 0,
        });
        self.core_path_used = core_path.display().to_string();
        if self.core_tx.send(mesh_core::Cmd::Start(spec)).is_err() {
            self.core = None;
            return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_CORE_THREAD_CLOSED).to_string());
        }
        Ok(())
    }

    fn start_heartbeat(&mut self) {
        self.stop_heartbeat();
        let Some(api) = self.api.clone() else {
            return;
        };
        let cell = Arc::clone(&self.cell);
        let tx = self.self_tx.clone();
        let (stop_tx, stop_rx) = channel();
        thread::Builder::new()
            .name("mesh-heartbeat".to_string())
            .spawn(move || loop {
                // 进入循环时不立刻 beat：connect 刚用同样的参数打过一次，紧接着再来一次
                // 只是重复上报；更要紧的是 core 未运行时会让“心跳→重连→心跳”形成零延迟
                // 回路（core 起不来时实测每秒数百次心跳）。
                match stop_rx.recv_timeout(HEARTBEAT_INTERVAL) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let published = cell.get();
                let result = api.heartbeat(
                    &published.beat.config_hash,
                    &published.beat.app_version,
                    &published.beat.credential_id,
                    published.beat.app_update_known.as_deref(),
                    HEARTBEAT_TIMEOUT,
                );
                if tx.send(Cmd::Heartbeat(result)).is_err() {
                    return;
                }
            })
            .expect("启动心跳线程失败");
        self.hb_stop = Some(stop_tx);
    }

    fn stop_heartbeat(&mut self) {
        if let Some(stop) = self.hb_stop.take() {
            let _ = stop.send(());
        }
    }

    fn start_stun_loop(&mut self) {
        self.stop_stun_loop();
        let cell = Arc::clone(&self.cell);
        let tx = self.self_tx.clone();
        let (stop_tx, stop_rx) = channel();
        thread::Builder::new()
            .name("mesh-stun".to_string())
            .spawn(move || loop {
                let published = cell.get();
                // core 不在跑时 rpc 是 None：省掉一次必然失败的查询。
                if let Some(addr) = published.rpc {
                    if let Ok(node) = mesh_etrpc::show_node_info(addr, Duration::from_secs(3)) {
                        if tx.send(Cmd::Stun(node.stun)).is_err() {
                            return;
                        }
                    }
                }
                // 用停止通道代替 sleep：退出与重连都不必等满一个刷新周期。
                match stop_rx.recv_timeout(STUN_REFRESH_INTERVAL) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            })
            .expect("启动 STUN 刷新线程失败");
        self.stun_stop = Some(stop_tx);
    }

    fn stop_stun_loop(&mut self) {
        if let Some(stop) = self.stun_stop.take() {
            let _ = stop.send(());
        }
    }
}

/// 连接失败的退避：10s 起步，每次 ×1.5，最多 60s。
pub(crate) fn connect_backoff(failures: u32) -> Duration {
    let mut wait = Duration::from_secs(10);
    let mut step = 1;
    while step < failures && wait < CONNECT_BACKOFF_MAX {
        wait = wait.mul_f64(1.5);
        step += 1;
    }
    wait.min(CONNECT_BACKOFF_MAX)
}

/// 重拉 core 的退避：1s 起步翻倍，最多 30s。
pub(crate) fn revive_backoff(attempt: u32) -> Duration {
    let mut wait = Duration::from_secs(1);
    let mut step = 1;
    while step < attempt && wait < REVIVE_BACKOFF_MAX {
        wait *= 2;
        step += 1;
    }
    wait.min(REVIVE_BACKOFF_MAX)
}

/// `config_hash`：`sha256(toml)` 的前 16 位十六进制。
pub(crate) fn config_digest(toml: &str) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(toml.as_bytes());
    hex::encode(sum)[..16].to_string()
}

pub(crate) fn strip_cidr(ip: &str) -> &str {
    match ip.find('/') {
        Some(index) => &ip[..index],
        None => ip,
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn connect_backoff_grows_and_caps() {
        assert_eq!(connect_backoff(1), Duration::from_secs(10));
        assert_eq!(connect_backoff(2), Duration::from_secs(15));
        let mut previous = Duration::ZERO;
        for failures in 1..=12 {
            let wait = connect_backoff(failures);
            assert!(wait >= previous, "退避不应变小：{failures}");
            assert!(wait <= CONNECT_BACKOFF_MAX);
            previous = wait;
        }
        assert_eq!(previous, CONNECT_BACKOFF_MAX);
    }

    #[test]
    fn revive_backoff_doubles_and_caps() {
        assert_eq!(revive_backoff(1), Duration::from_secs(1));
        assert_eq!(revive_backoff(2), Duration::from_secs(2));
        assert_eq!(revive_backoff(3), Duration::from_secs(4));
        let mut previous = Duration::ZERO;
        for attempt in 1..=10 {
            let wait = revive_backoff(attempt);
            assert!(wait >= previous);
            assert!(wait <= REVIVE_BACKOFF_MAX);
            previous = wait;
        }
        assert_eq!(previous, REVIVE_BACKOFF_MAX);
    }

    #[test]
    fn config_digest_is_the_first_16_hex_chars_of_sha256() {
        // 与 Go 的 configDigest 一致：sha256 全量十六进制的前 16 位。
        assert_eq!(config_digest(""), "e3b0c44298fc1c14");
        assert_eq!(config_digest("hostname = \"x\"\n").len(), 16);
    }
}
