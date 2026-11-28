//! 引擎线程：一个事件循环，控制面与 core 各按自己的期限推进。
//!
//! 只有这个线程读写 [`Engine`] 里的状态。别的线程（心跳、下载、面板、STUN）只能通过
//! [`Cmd`] 投递结果、通过 [`StateCell`] 读快照。
//!
//! 循环只有一个：每次唤醒取「下一次联系控制面」与「下一次重拉 core」里最早的那个，
//! 到点各推进各的。控制面不可达只会让心跳退避，不会让 core 的监督停下来。

use crate::autostart;
use crate::clock;
use crate::log::Log;
use crate::startup::StartupBlocked;
use crate::views::{Snapshot, StateCell};
use crate::{coreline, startup, sync, update, Reply, Shutdown};
use mesh_install::layout;
use mesh_remote as remote;
use mesh_state::hints::{parse_hints, ConfigHints};
use mesh_state::{ClientConfig, Store};
use std::net::SocketAddr;
use std::path::PathBuf;
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
///
/// 一分钟而不是几秒：崩溃循环里的 core 往往能就绪、再在半分钟到一分钟内倒掉
/// （网卡创建失败约 15 秒）。判据太短等于每次崩溃都算成功，退避永远回到第一档。
pub(crate) const CORE_HEALTHY_LIFETIME: Duration = Duration::from_secs(60);
/// 重拉 core 的退避上限（1s 起步翻倍，最多 30s）。
const REVIVE_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// 自动重拉的次数上限。8 次配 1/2/4/8/16/30/30/30 的退避约两分钟：够等过一次
/// 瞬时的设备/服务抖动，又不至于让无提示的循环一直转下去。
pub(crate) const MAX_REVIVE_ATTEMPTS: u32 = 8;
/// 等 core 停止回复的上限。core 内部另有 5s 软停 + 3s 强杀的预算。
const STOP_REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// STUN 结果的刷新间隔。
pub(crate) const STUN_REFRESH_INTERVAL: Duration = Duration::from_secs(20);
/// 本地身份读不出来时的重试间隔。
const LOCAL_STATE_RETRY: Duration = Duration::from_secs(5);

/// 面向界面的连接状态。只由 `Engine::status` 从两个生命周期推导，处理器不直接写它。
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
    /// 心跳线程只负责取样与发请求；响应处理在引擎里，首次与周期走同一段。
    Heartbeat {
        /// 发这条结果的心跳代次。重连/换身份之后的旧结果由引擎丢掉。
        gen: u64,
        result: Result<Option<remote::Heartbeat>, remote::Error>,
    },
    Core(mesh_core::Event),
    /// STUN 刷新结果。带 core 代次：查询期间换了 core，这份结果就不属于现在这个实例。
    Stun {
        gen: u64,
        info: mesh_etrpc::StunInfo,
    },
    /// 一份下载任务的完成消息。带着任务代次：撤回、同版本重发与换任务之后，
    /// 旧任务的结果不许消费新状态。
    Download {
        task: update::DownloadTask,
        result: Result<PathBuf, String>,
    },
    /// 启动器替换的结果。锁被占是“没结论”，不是失败。
    Launcher {
        task: update::DownloadTask,
        result: update::LauncherResult,
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
    /// 主程序所在目录。`easytier-core` 必须与它同层，所以这里给的是「同层」的
    /// 具体路径，而不是让引擎去拼 `bin`：测试与直接运行主程序的场景各有各的布局。
    pub program_dir: PathBuf,
    pub self_version: String,
    /// 本机主机名：配置里没写设备名时用它。
    pub hostname: String,
    /// `--disable-auto-update`：本地一票否决服务端的包级开关。
    pub auto_update_disabled: bool,
}

/// 控制面生命周期的推进点。`due` 是唯一的时间状态：请求在途时为 `None`，否则是
/// 下一次心跳（例行或重试）的时刻。
#[derive(Default)]
pub(crate) struct Control {
    /// 心跳代次：每发一次加一，旧代次的结果不认。
    pub gen: u64,
    /// 连续失败次数：连接退避的档位。
    pub failures: u32,
    /// 下一次联系控制面的时刻。
    pub due: Option<Instant>,
}

/// 一代 core 的期望输入。
///
/// 期望规格与运行实例实际应用的规格相等就不换进程，不等就换：配置同步、改名、
/// 凭据轮换与重新绑定都走这一条判据，不再各写一套“要不要重启”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoreSpec {
    /// 身份代次：控制面地址 + 设备 id。两者一起才是一代——设备 id 是客户端生成的，
    /// 两个控制面上可以是同一个。
    pub generation: String,
    /// 磁盘上 `config.toml` 的摘要（16 位十六进制）。
    pub config_hash: String,
    /// 实际传给 `--hostname` 的名字。
    pub hostname: String,
    /// 身份二选一；凭据节点带着 core 实际加载的那张 id。
    pub auth: AuthMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthMode {
    Formal,
    Credential(String),
}

/// 一次 core 代管。`gen` 是引擎分配的代号，事件按它归并；`spec` 是它实际应用的
/// 规格；`pid` 为零表示还没就绪（`Started` 之后才有进程号）。
pub(crate) struct CoreRun {
    pub gen: u64,
    pub pid: u32,
    pub spec: CoreSpec,
    pub rpc_addr: SocketAddr,
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

    pub(crate) client: Option<ClientConfig>,
    /// 下载与启动器替换用的连接；不参与任何“是否在线”的判断。
    pub(crate) api: Option<Arc<remote::Client>>,
    pub(crate) control: Control,
    pub(crate) last_hb: i64,
    /// 最近一次失败的说明（界面上的错误行）。
    pub(crate) last_error: String,
    /// 需要人看一眼的失败（本地状态、配置、core 启动）。为空表示没有这种失败。
    pub(crate) fault: String,

    pub(crate) core: Option<CoreRun>,
    /// 上一代 core 就绪的时刻：重拉档位靠它判断“刚起来就死”。
    pub(crate) last_core_started_at: Option<Instant>,
    /// 连续短命重启的档位（Go 的 `reviveAttempt`），上一代活够
    /// `CORE_HEALTHY_LIFETIME` 就回到 1。
    pub(crate) revive_attempt: u32,
    pub(crate) next_gen: u64,
    pub(crate) revive: Option<Revive>,
    /// 重拉次数用完之后的停手标记。
    ///
    /// 与 `revive == None` 不是一回事：没有期限也可能只是还没排。停手之后心跳、
    /// 启动与重拉都不能再把 core 拉起来，唯一出口是 `Cmd::Reconnect`。
    pub(crate) core_halted: bool,
    /// core 输出的最后一行（错误行优先），停手时的 fault 文案用它说清真正的原因。
    pub(crate) core_tail: Arc<coreline::CoreTail>,

    pub(crate) auth_fails: u32,
    pub(crate) update: update::UpdateState,
    /// 下载任务的代次，只增不减：换 offer 会重置其状态，代次不能跟着重置，
    /// 否则旧任务的完成消息会认领新任务。
    pub(crate) next_task: u64,
    /// 启动器槽位的本地状态。与 `update` 分开：那份会随客户端包 offer 重建。
    pub(crate) launcher: update::LauncherState,
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
    /// STUN 刷新线程的停止通道：整条引擎线程一个，不再随重连新建。
    pub(crate) stun_stop: Option<Sender<()>>,
}

/// 起引擎线程，返回它的命令入口与线程句柄。
///
/// 与 MODULES.md 的签名差异：引擎要给自己投递心跳结果与 core 事件，必须持有
/// `Sender<Cmd>`，所以这里自己建 channel 并把发送端交出去（调用方只拿 Sender）。
///
/// 同步注册一步收尾：`Shutdown::run_and_exit` 时先停 core。
///
/// 安装目录安全检查不通过时返回 [`StartupBlocked`]：ERROR 日志已由闸门写入，
/// 此时没有拉起任何线程，调用方负责让进程以非零退出码结束。
pub fn spawn(
    config: Config,
    cell: Arc<StateCell>,
    notices: Sender<Notice>,
    shutdown: Arc<Shutdown>,
) -> Result<(Sender<Cmd>, JoinHandle<()>), StartupBlocked> {
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
    // 安装根与 `config/` 的权限决定谁能替换下一次以提权身份启动的可执行文件。
    // 收紧或校验不通过时闸门已经写过 ERROR 日志，这里直接终止启动，不拉起 core。
    startup::check(
        &config.root,
        &config.program_dir,
        store.harden_error(),
        &log,
    )?;

    // core 事件包成 Cmd::Core 投回引擎；core 输出的每一行先去掉时间戳、打码再落盘。
    //
    // 落盘之外还留一份最后一行：引擎看不到 core 的 stdout，崩溃循环停手时除了退出码
    // 就只有这一句能说清“为什么起不来”，而它必须由这里顺手带出来。
    let inbox_tx = tx.clone();
    let log_for_core = Arc::clone(&log);
    let core_tail = Arc::new(coreline::CoreTail::default());
    let tail_for_core = Arc::clone(&core_tail);
    let core_tx = mesh_core::spawn(
        move |event| {
            let _ = inbox_tx.send(Cmd::Core(event));
        },
        move |line| {
            let cleaned = coreline::clean(line);
            tail_for_core.observe(&cleaned);
            log_for_core.add("core", &cleaned);
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

    // 启动器的两件事实只读一次：安装根下那份的摘要，与它自报的版本号。
    //
    // 这里读到的 `launcher.json` 描述的是「上一次被拉起的那个启动器」，本进程
    // 不重置也不重读：替换成功之后盘上那份已经是新的，事实由替换结果更新
    // （见 `Engine::on_launcher_result`），只有下一次启动才是新的在跑。
    let launcher = update::LauncherState::read(&config.root);
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
        client: None,
        api: None,
        control: Control::default(),
        last_hb: 0,
        last_error: String::new(),
        fault: String::new(),
        core: None,
        last_core_started_at: None,
        revive_attempt: 0,
        next_gen: 0,
        revive: None,
        core_halted: false,
        core_tail,
        auth_fails: 0,
        update: update::UpdateState::default(),
        next_task: 0,
        launcher,
        autostart_on: false,
        identity: IdentityMeta::default(),
        hints: ConfigHints::default(),
        release_key: remote::signing::RELEASE_PUBLIC_KEY.to_string(),
        local_ip: String::new(),
        core_version: String::new(),
        core_path_used: String::new(),
        network_name: String::new(),
        stun: mesh_etrpc::StunInfo::default(),
        stun_stop: None,
        config,
    };
    let handle = thread::Builder::new()
        .name("mesh-engine".to_string())
        .spawn(move || engine.run())
        .expect("启动引擎线程失败");
    Ok((tx, handle))
}

/// `pump` 的结果。
enum Flow {
    /// 给的时间用完了（该推进到期的那一件事）。
    Timeout,
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
        self.reload_identity();
        // 先用磁盘上那份配置把 core 拉起来，再开始心跳：控制面不可达只该让状态
        // 降级，不该把数据面一起停掉。
        self.start_core_from_disk();
        self.start_stun_loop();
        self.publish();
        loop {
            if self.shutdown.triggered() {
                break;
            }
            match self.pump() {
                Flow::Timeout => {
                    self.on_deadline();
                    self.publish();
                }
                Flow::Closed => break,
            }
        }
        self.stop_stun_loop();
        let _ = self.stop_core();
        self.publish();
        self.log.close();
    }

    /// 处理 inbox，直到某一边的期限到点或命令端断开；每条消息之后发布快照。
    ///
    /// 期限在每条命令之后重新算：命令本身可能把下一次心跳提前（设置、重连），
    /// 拿消息进来之前的期限继续等会让它白等一个周期。
    fn pump(&mut self) -> Flow {
        loop {
            // 收尾已经开始：退出只有一条路（Shutdown），引擎不必再处理任何命令。
            if self.shutdown.triggered() {
                return Flow::Closed;
            }
            let received = match self.next_deadline() {
                Some(wait) => self.inbox.recv_timeout(wait),
                None => self
                    .inbox
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(cmd) => {
                    self.handle(cmd);
                    self.publish();
                }
                Err(RecvTimeoutError::Timeout) => return Flow::Timeout,
                Err(RecvTimeoutError::Disconnected) => return Flow::Closed,
            }
        }
    }

    /// 下一次该被唤醒的时刻：控制面与 core 各自期限里最早的那个。
    pub(crate) fn next_deadline(&self) -> Option<Duration> {
        let control = self.control.due;
        let revive = self.revive.as_ref().map(|revive| revive.due);
        control
            .into_iter()
            .chain(revive)
            .min()
            .map(|due| due.saturating_duration_since(Instant::now()))
    }

    /// 到点了：两边各自看自己的期限，互不影响。
    pub(crate) fn on_deadline(&mut self) {
        let now = Instant::now();
        if self.revive.as_ref().is_some_and(|revive| revive.due <= now) {
            self.revive_core();
        }
        if self.control.due.is_some_and(|due| due <= now) {
            self.beat();
        }
    }

    /// 处理一条命令。
    pub(crate) fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Heartbeat { gen, result } => self.on_heartbeat(gen, result),
            Cmd::Core(event) => self.on_core_event(event),
            Cmd::Stun { gen, info } => {
                // 只有仍在跑的那一代 core 的探测结果才算数。
                if self.core.as_ref().is_some_and(|core| core.gen == gen) {
                    self.stun = info;
                }
            }
            Cmd::Download { task, result } => self.on_download(task, result),
            Cmd::Launcher { task, result } => self.on_launcher_result(task, result),
            Cmd::Settings(input, reply) => {
                let result = self.save_settings(input);
                if let Err(err) = &result {
                    self.notify(&mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_SAVE_SETTINGS_FAILED,
                        &[&err],
                    ));
                }
                let snapshot = self.snapshot();
                let _ = reply.send(result.map(|()| snapshot).map_err(|err| err.to_string()));
            }
            Cmd::Reset(reply) => {
                let result = self.reset_config();
                let snapshot = self.snapshot();
                let _ = reply.send(result.map(|()| snapshot).map_err(|err| err.to_string()));
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
            }
            Cmd::ApplyUpdate(reply) => {
                let result = self.apply_update();
                let _ = reply.send(result);
            }
            Cmd::Reconnect => {
                self.logf(
                    "app",
                    mesh_text::get(mesh_text::id::ENGINE_LOG_RECONNECT_MANUAL),
                );
                self.rebind();
                self.revive_now();
            }
        }
    }

    /// 一次心跳请求。没有身份就先从盘上读一遍；读不到就等设置页。
    fn beat(&mut self) {
        if self.client.is_none() {
            self.reload_identity();
        }
        let Some(request) = self.beat_request() else {
            return;
        };
        self.control.gen = self.control.gen.wrapping_add(1);
        let gen = self.control.gen;
        // 结果回来之前不再发第二次：请求在途时没有“下一次联系控制面”的时刻。
        self.control.due = None;
        let tx = self.self_tx.clone();
        let spawned = thread::Builder::new()
            .name("mesh-heartbeat".to_string())
            .spawn(move || {
                let result = request.api.heartbeat(
                    &request.hash,
                    &request.app_version,
                    &request.credential_id,
                    HEARTBEAT_TIMEOUT,
                );
                let _ = tx.send(Cmd::Heartbeat { gen, result });
            });
        if let Err(err) = spawned {
            self.note_control_failure(&mesh_text::format(
                mesh_text::id::ENGINE_LOG_CONNECT_FAILED,
                &[&err],
            ));
        }
    }

    /// 这一轮心跳要上报的事实。取样在引擎线程里做，与发请求的线程无关。
    fn beat_request(&mut self) -> Option<BeatRequest> {
        let config = self.client.clone()?;
        let key = match self.store.signing_key() {
            Ok(key) => key,
            Err(err) => {
                self.note_control_failure(&mesh_text::format(
                    mesh_text::id::ENGINE_TEXT_READ_DEVICE_KEY_FAILED,
                    &[&err],
                ));
                return None;
            }
        };
        let api = Arc::new(remote::Client::new(
            &config.server_url,
            &config.device_id,
            key,
        ));
        self.api = Some(Arc::clone(&api));
        Some(BeatRequest {
            api,
            hash: self.hash(),
            app_version: self.config.self_version.clone(),
            credential_id: self.credential_id(),
        })
    }

    /// 心跳结果：成功走唯一的响应处理，失败按退避排下一次。
    ///
    /// `gen` 不是当前代次就直接丢掉：那条结果说的是另一条连接上的事——它可能把
    /// 状态改回在线、用旧响应里的 URL 去下载、把旧控制面的配置与凭据落盘，甚至
    /// 按旧连接的 401 清掉刚换好的身份。
    pub(crate) fn on_heartbeat(
        &mut self,
        gen: u64,
        result: Result<Option<remote::Heartbeat>, remote::Error>,
    ) {
        if gen != self.control.gen {
            return;
        }
        let api = match self.api.clone() {
            Some(api) => api,
            None => return,
        };
        match result {
            Ok(heartbeat) => {
                self.control.failures = 0;
                self.last_hb = clock::unix_now();
                self.reset_auth_fails();
                self.control.due = Some(Instant::now() + HEARTBEAT_INTERVAL);
                self.on_beat(api, heartbeat.unwrap_or_default());
            }
            Err(err) => {
                // 只有 401 可能是身份问题：服务端明确回 revoked，或连续校验失败。
                // 5xx（包括服务端 nonce 缓存饱和）是服务端自己的临时状况，走下面的
                // 控制面失败退避——既不计身份失败次数，也不清本地身份。
                if matches!(err, remote::Error::Unauthorized { .. }) {
                    self.control.failures = self.control.failures.saturating_add(1);
                    if self.handle_auth_rejected(&err) {
                        // 身份已被服务端判定无效：回设置页，不再有心跳期限。
                        self.unbind();
                        return;
                    }
                    self.control.due =
                        Some(Instant::now() + connect_backoff(self.control.failures));
                    return;
                }
                self.note_control_failure(&mesh_text::format(
                    mesh_text::id::ENGINE_LOG_HEARTBEAT_FAILED,
                    &[&err],
                ));
            }
        }
    }

    /// 一次成功心跳的响应处理。首次心跳与之后每一轮走同一段，没有“首轮”分支。
    fn on_beat(&mut self, api: Arc<remote::Client>, heartbeat: remote::Heartbeat) {
        // 凭据先落盘：core 只在下一次启动时读到它，`ensure_core` 会因为规格变化换进程。
        if self.save_credential(&api, &heartbeat) {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_CREDENTIAL_RECEIVED_RESTART),
            );
        }
        self.offer_update(heartbeat.app_update.as_ref());
        // 启动器单独一路：控制面每次都带，客户端自己比摘要；下载放后台跑。
        self.apply_launcher(&api, heartbeat.launcher.as_ref());
        // 配置只有一个入口条件：响应确实带正文。204、只带发布信息或只带凭据的响应
        // 没有配置可应用——那三者都是正常心跳，不是「配置变化却没带正文」。
        if !heartbeat.config_b64.is_empty() {
            if heartbeat.config_hash != self.hash() {
                self.logf(
                    "app",
                    mesh_text::get(mesh_text::id::ENGINE_LOG_CONFIG_CHANGED_SYNC),
                );
            }
            self.apply_remote(&heartbeat);
        }
        // 自启状态只在启动、用户改动和这里刷新：快照两秒一次，不可能每次都去起进程。
        self.refresh_autostart();
        // 心跳是节拍：自动安装、被推迟的提交与失败的下载都在这里推进。
        self.advance_update();
        // 规格比较与状态汇合只留这一个最终入口：上面每一步只写事实，换不换 core
        // 由这里按「期望规格 == 运行实例规格」定（缺凭据也要等下一轮心跳）。
        self.ensure_core_or_report();
    }

    /// 记一次控制面失败：退避档位加一，下一次心跳排到退避之后。
    ///
    /// 取样失败（读私钥、起线程）与请求失败走同一条路径，否则前者会一直停在
    /// “connecting”而看起来什么都没发生。
    fn note_control_failure(&mut self, message: &str) {
        self.control.failures = self.control.failures.saturating_add(1);
        self.last_error = message.to_string();
        self.logf("app", message);
        self.control.due = Some(Instant::now() + connect_backoff(self.control.failures));
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
                core.started_unix = clock::unix_now();
                self.last_core_started_at = Some(Instant::now());
                self.local_ip = strip_cidr(&node.ipv4).to_string();
                self.core_version = node.version.clone();
                self.stun = node.stun.clone();
                self.revive = None;
                // core 启动失败留下的错误不再成立；两个平面都正常时由 publish 收口。
                self.fault.clear();
                if !self.hints.network_name.is_empty() {
                    self.network_name = self.hints.network_name.clone();
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
                self.fault = msg.clone();
                self.logf("app", &msg);
                let attempt = self.on_start_failed_attempt();
                self.schedule_revive(attempt);
            }
            mesh_core::Event::Exited { gen, err } => {
                if !self.core_matches(gen) {
                    return;
                }
                self.core = None;
                self.local_ip.clear();
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
                self.last_error =
                    mesh_text::get(mesh_text::id::ENGINE_TEXT_CORE_NOT_RUNNING_RESTARTING)
                        .to_string();
                let attempt = self.next_revive_attempt();
                self.schedule_revive(attempt);
            }
        }
    }

    /// 排一次重拉；次数用完就停手，把原因写进 fault 并记一行日志。
    ///
    /// 停手不是“恢复了”：core 没在跑，故障也还在。继续按 30s 重拉只是把同一段失败
    /// 一遍遍刷进日志，而界面永远停在“正在重启”。到这里就结束自动重试，等人来。
    pub(crate) fn schedule_revive(&mut self, attempt: u32) {
        let attempt = attempt.max(1);
        if attempt > MAX_REVIVE_ATTEMPTS {
            self.revive = None;
            self.revive_attempt = MAX_REVIVE_ATTEMPTS;
            self.core_halted = true;
            let msg = self.crash_loop_message();
            self.fault = msg.clone();
            self.logf("app", &msg);
            return;
        }
        self.revive = Some(Revive {
            attempt,
            due: Instant::now() + revive_backoff(attempt),
        });
    }

    /// 停手时那句完整的话：重拉次数 + core 自己最后留下的错误。
    ///
    /// 具体原因只有 core 知道（网卡、权限、驱动），客户端能做的就是把那一行原样带到
    /// 用户眼前，而不是继续说“正在重启”。
    fn crash_loop_message(&self) -> String {
        let detail = self.core_tail.last().unwrap_or_else(|| {
            mesh_text::get(mesh_text::id::ENGINE_TEXT_CORE_NO_OUTPUT).to_string()
        });
        mesh_text::format(
            mesh_text::id::ENGINE_TEXT_CORE_CRASH_LOOP,
            &[&MAX_REVIVE_ATTEMPTS, &detail],
        )
    }

    /// 本次重拉该用第几档退避：上一代 core 活得够久才回到第一档。
    ///
    /// 判据是 `last_core_started_at`（上一次**成功就绪**的时刻），不是“当前这一代”：
    /// 崩溃之后 `core` 已经被清空，拿当前代判断会把档位永远重置成 1——那正是
    /// “短命 core 把退避冲掉”的现场。
    ///
    /// 阈值是 `CORE_HEALTHY_LIFETIME`（60 秒）：能就绪、随后在半分钟内倒掉的 core
    /// 仍然算这次重启失败，档位继续往上走，直到 `MAX_REVIVE_ATTEMPTS` 停手。
    pub(crate) fn next_revive_attempt(&mut self) -> u32 {
        let short_lived = self
            .last_core_started_at
            .map(|at| at.elapsed() < CORE_HEALTHY_LIFETIME)
            .unwrap_or(false);
        self.revive_attempt = if short_lived {
            // 刚起来就死：档位只增不减，避免重启节奏被 core 自己的退出速度决定。
            self.revive_attempt.saturating_add(1).max(1)
        } else {
            1
        };
        self.revive_attempt
    }

    /// 一次收敛失败（起不来，或旧实例停不掉）同样放大档位：Go 的 `reviveCore` 每失败
    /// 一次就 `attempt++`，否则“每次都在 1 秒重试”会变成每秒拉起一个进程。
    pub(crate) fn on_start_failed_attempt(&mut self) -> u32 {
        self.revive_attempt = self.revive_attempt.saturating_add(1).max(1);
        self.revive_attempt
    }

    /// 到点的重拉：按当前期望规格补一次启动。
    fn revive_core(&mut self) {
        let Some(revive) = self.revive.take() else {
            return;
        };
        if Instant::now() < revive.due {
            self.revive = Some(revive);
            return;
        }
        match self.ensure_core() {
            Ok(()) => {
                if self.core.is_some() {
                    self.logf(
                        "app",
                        mesh_text::get(mesh_text::id::ENGINE_LOG_CORE_REVIVED),
                    );
                }
            }
            Err(err) => {
                let msg = mesh_text::format(
                    mesh_text::id::ENGINE_LOG_CORE_START_FAILED_ATTEMPT,
                    &[&revive.attempt, &err],
                );
                self.fault = msg.clone();
                self.logf("app", &msg);
                self.schedule_revive(revive.attempt + 1);
            }
        }
    }

    /// 开始新的一轮控制面同步：在途结果作废，下一次心跳立刻发生。
    ///
    /// core 的重拉档位也跟着归零：换绑与“重新连接”都是用户在做一次新的开始，
    /// 上一代身份留下的停手标记不该延续到新的一轮上，否则引擎会一直拒绝启动 core。
    pub(crate) fn rebind(&mut self) {
        self.control.gen = self.control.gen.wrapping_add(1);
        self.control.failures = 0;
        self.control.due = Some(Instant::now());
        self.last_hb = 0;
        self.revive_attempt = 0;
        self.core_halted = false;
    }

    /// 手动重连：清掉停手时写下的失败，立刻再排一次重拉。
    ///
    /// “重新连接”是崩溃循环之后唯一的恢复入口：用户按下它时网卡与权限通常已经修好，
    /// 没有理由再等一个心跳周期或一段退避。
    pub(crate) fn revive_now(&mut self) {
        self.fault.clear();
        self.revive = Some(Revive {
            attempt: 1,
            due: Instant::now(),
        });
    }

    /// 没有身份可用：控制面不再有期限，等设置页或下一次注册。
    ///
    /// 认证失败计数属于刚被丢掉的那一代身份，跟着一起清：下一份身份从零开始记。
    pub(crate) fn unbind(&mut self) {
        self.client = None;
        self.reset_auth_fails();
        self.fault.clear();
        self.control.gen = self.control.gen.wrapping_add(1);
        self.control.failures = 0;
        self.control.due = None;
        self.last_hb = 0;
    }

    /// 从磁盘重读身份。首次启动、设置页与吊销都走这里，不另设状态。
    pub(crate) fn reload_identity(&mut self) {
        match self.load_client() {
            sync::Loaded::Failed(err) => {
                self.client = None;
                let msg =
                    mesh_text::format(mesh_text::id::ENGINE_LOG_LOCAL_STATE_READ_FAILED, &[&err]);
                self.fault = msg.clone();
                self.logf("app", &msg);
                self.control.due = Some(Instant::now() + LOCAL_STATE_RETRY);
            }
            sync::Loaded::Missing => {
                self.client = None;
                self.fault.clear();
                self.control.due = None;
            }
            sync::Loaded::Configured(config) => {
                self.client = Some(config);
                self.control.due = Some(Instant::now());
            }
        }
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

    pub(crate) fn logf(&self, source: &str, message: &str) {
        self.log.add(source, message);
    }

    /// 界面状态：只由两个生命周期与一个失败槽推导，没有处理器直接写它。
    ///
    /// | 事实 | 状态 |
    /// | --- | --- |
    /// | 有确定失败（本地状态、配置、core 启动） | `error` |
    /// | 没有身份 | `unconfigured` |
    /// | 还没有任何输赢（没心跳结果、没失败、没排重拉） | `connecting` |
    /// | 心跳通且 core 就绪 | `online` |
    /// | 其余（控制面或 core 有一边没就绪） | `degraded` |
    pub(crate) fn status(&self) -> Status {
        if !self.fault.is_empty() {
            return Status::Error;
        }
        if self.client.is_none() {
            return Status::Unconfigured;
        }
        if self.last_hb == 0 && self.control.failures == 0 && self.revive.is_none() {
            return Status::Connecting;
        }
        if self.control.failures == 0 && self.last_hb > 0 && self.core_running() {
            Status::Online
        } else {
            Status::Degraded
        }
    }

    /// 两个平面都恢复了：上一次失败的原因不再成立。发布快照之前统一收口一次。
    ///
    /// 停手留下的 fault 不在此列：那里可能是“旧 core 停不掉”而 core 仍在跑，
    /// 收口掉它会让界面在停止自动重拉之后又显示成正常。
    pub(crate) fn settle(&mut self) {
        if self.core_halted {
            return;
        }
        if self.core_running() && self.control.failures == 0 && self.last_hb > 0 {
            self.fault.clear();
            self.last_error.clear();
        }
    }

    pub(crate) fn core_running(&self) -> bool {
        self.core
            .as_ref()
            .map(|core| core.pid != 0)
            .unwrap_or(false)
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
            .filter(|core| core.pid != 0)
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
        self.ensure_core_or_report();
        if self.core.is_some() {
            self.logf(
                "app",
                mesh_text::get(mesh_text::id::ENGINE_LOG_CORE_STARTED_FROM_DISK),
            );
        }
    }

    /// 期望规格：当前身份 + 磁盘配置 + 实际设备名 + 身份方式。
    ///
    /// `Ok(None)` 表示还没到能启动的时候（没有身份，或还没有配置）——不是失败。
    /// `Err` 表示配置在、但启动条件不成立（解析不了、没有凭据、私钥读不出）。
    pub(crate) fn desired_spec(&self) -> Result<Option<CoreSpec>, String> {
        let Some(config) = self.client.as_ref() else {
            return Ok(None);
        };
        let toml = match self.store.read_config() {
            Ok(Some(toml)) => toml,
            Ok(None) => return Ok(None),
            Err(err) => {
                return Err(mesh_text::format(
                    mesh_text::id::ENGINE_TEXT_CONFIG_UNREADABLE,
                    &[&err],
                ))
            }
        };
        let hints = parse_hints(&toml);
        if let Some(err) = hints.parse_error.as_deref() {
            return Err(mesh_text::format(
                mesh_text::id::ENGINE_TEXT_CONFIG_UNREADABLE,
                &[&err],
            ));
        }
        let auth = if hints.formal() {
            // 读不出来就不启动：正式节点没有私钥等于没有身份。
            self.store.secure_key().map_err(|err| {
                mesh_text::format(mesh_text::id::ENGINE_TEXT_READ_NODE_KEY_FAILED, &[&err])
            })?;
            AuthMode::Formal
        } else {
            match self.store.load_credential() {
                Ok(Some(credential)) => AuthMode::Credential(credential.id),
                Ok(None) => return Err(sync::no_credential()),
                Err(err) => {
                    return Err(mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_READ_CREDENTIAL_FAILED,
                        &[&err],
                    ))
                }
            }
        };
        Ok(Some(CoreSpec {
            generation: identity_key(config),
            config_hash: config_digest(&toml),
            hostname: self.effective_hostname(config),
            auth,
        }))
    }

    /// 让运行实例与期望规格一致：相同不重复拉起，不同就换进程。
    ///
    /// 这是“要不要换 core”的唯一判据；配置同步、改名、凭据轮换与重新绑定都汇到这里。
    /// 规则是完整的：**没有期望规格**（删了配置、清了身份、换绑之后还没拿到新配置）
    /// 同样要求没有运行实例。删文件只决定下一次读哪份配置，不决定那个已经连着旧网络
    /// 的进程会退——旧数据面与当前身份的关系不能只由磁盘清理保证。
    pub(crate) fn ensure_core(&mut self) -> Result<(), String> {
        // 停手之后到这里一律不启动：心跳、启动与重拉都走这条收敛，漏掉它
        // “停止自动重拉”就只剩一条日志，下一轮心跳又会把 core 拉起来。
        if self.core_halted {
            return Ok(());
        }
        let spec = self.desired_spec().map_err(|err| {
            mesh_text::format(mesh_text::id::ENGINE_LOG_CORE_START_FAILED, &[&err])
        })?;
        let Some(spec) = spec else {
            if let Err(err) = self.stop_core() {
                // 停不掉：失败要可见（stop_core 已记一行），并按重拉档位再试——
                // 控制面不可达时没有心跳可等，重试只能靠这条期限。
                let attempt = self.on_start_failed_attempt();
                self.schedule_revive(attempt);
                return Err(mesh_text::format(
                    mesh_text::id::ENGINE_LOG_STOP_CORE_FAILED,
                    &[&err],
                ));
            }
            return Ok(());
        };
        if self.core.as_ref().map(|core| &core.spec) == Some(&spec) {
            return Ok(());
        }
        self.stop_core().map_err(|err| {
            mesh_text::format(mesh_text::id::ENGINE_LOG_STOP_CORE_FAILED, &[&err])
        })?;
        self.start_core(spec)
            .map_err(|err| mesh_text::format(mesh_text::id::ENGINE_LOG_CORE_START_FAILED, &[&err]))
    }

    /// 收敛失败只记成一次确定失败，不向上抛：core 是数据面，控制面不可达不该让它跟着停。
    ///
    /// 失败原因由 `ensure_core` 自己写成完整的一句（启动失败与停止失败各有各的
    /// 说法），这里只负责让它可见。
    pub(crate) fn ensure_core_or_report(&mut self) {
        if let Err(err) = self.ensure_core() {
            self.fault = err.clone();
            self.logf("app", &err);
        }
    }

    /// 启动一代 core。调用方已经确认这一代与运行实例不同。
    pub(crate) fn start_core(&mut self, spec: CoreSpec) -> Result<(), String> {
        self.refresh_identity();
        if self.client.is_none() {
            return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_NOT_REGISTERED).to_string());
        }
        let core_path = mesh_core::locate(&self.config.program_dir)?;
        let rpc_addr = sync::rpc_listen_addr()?;
        let identity = match &spec.auth {
            AuthMode::Formal => {
                let key = self.store.secure_key().map_err(|err| {
                    mesh_text::format(mesh_text::id::ENGINE_TEXT_READ_NODE_KEY_FAILED, &[&err])
                })?;
                mesh_core::Identity::Formal { key }
            }
            AuthMode::Credential(id) => match self.store.load_credential() {
                Ok(Some(credential)) if &credential.id == id => mesh_core::Identity::Credential {
                    id: credential.id,
                    secret: credential.secret,
                },
                Ok(_) => return Err(sync::no_credential()),
                Err(err) => {
                    return Err(mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_READ_CREDENTIAL_FAILED,
                        &[&err],
                    ));
                }
            },
        };
        let start = mesh_core::StartSpec {
            gen: self.next_gen.wrapping_add(1),
            core_path: core_path.clone(),
            config_path: self.store.config_path(),
            rpc_addr,
            hostname: spec.hostname.clone(),
            rpc_whitelist: sync::rpc_whitelist(),
            identity,
            // 就绪判据来自磁盘上这份配置；与 spec 的摘要同源。
            expect: mesh_core::Expect {
                hostname: self.hints.hostname.clone(),
                ipv4: self.hints.ipv4.clone(),
            },
        };
        self.next_gen = start.gen;
        self.core_path_used = core_path.display().to_string();
        self.core = Some(CoreRun {
            gen: start.gen,
            pid: 0,
            spec,
            rpc_addr,
            started_unix: 0,
        });
        // 新的一代从空开始：上一代的最后一行不能拿来解释这一代的失败。
        self.core_tail.clear();
        if self.core_tx.send(mesh_core::Cmd::Start(start)).is_err() {
            self.core = None;
            return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_CORE_THREAD_CLOSED).to_string());
        }
        Ok(())
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
                        if tx
                            .send(Cmd::Stun {
                                gen: published.core_gen,
                                info: node.stun,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                // 用停止通道代替 sleep：退出不必等满一个刷新周期。
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

    /// 同步打一次心跳并把结果交给响应处理。只给单元测试用：生产走后台线程。
    #[cfg(test)]
    pub(crate) fn beat_now(&mut self) {
        let Some(request) = self.beat_request() else {
            return;
        };
        self.control.gen = self.control.gen.wrapping_add(1);
        let gen = self.control.gen;
        self.control.due = None;
        let result = request.api.heartbeat(
            &request.hash,
            &request.app_version,
            &request.credential_id,
            HEARTBEAT_TIMEOUT,
        );
        self.on_heartbeat(gen, result);
    }
}

/// 一次心跳请求的取样结果。
struct BeatRequest {
    api: Arc<remote::Client>,
    hash: String,
    app_version: String,
    credential_id: String,
}

/// 身份代次：控制面地址 + 设备 id。
fn identity_key(config: &ClientConfig) -> String {
    format!("{}\n{}", config.server_url, config.device_id)
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
