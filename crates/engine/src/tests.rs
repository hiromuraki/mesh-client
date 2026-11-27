//! 引擎单测：用假 core（命令通道）与假服务端（std TcpListener）驱动状态机。
//!
//! 不启动真的 easytier-core、不真的注册设备：这台机器上客户端要 UAC，而且测试应当
//! 只验证“判断”本身。

use crate::log::Log;
use crate::run::{config_digest, Cmd, Config, CoreRun, Engine, Notice, Status};
use crate::update::{Phase, UpdateState};
use crate::views::{Published, StateCell};
use crate::Shutdown;
use mesh_core::Cmd as CoreCmd;
use mesh_remote as remote;
use mesh_state::{ClientConfig, Credential, Store};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// 假 core 收到的东西。
///
/// 只是测试里的记录通道，`Start` 带着整份 spec（约 250 字节）不值得再套 Box。
#[allow(clippy::large_enum_variant)]
pub(crate) enum Recorded {
    Start(mesh_core::StartSpec),
    Stop,
}

pub(crate) struct Harness {
    pub engine: Engine,
    pub core: Receiver<Recorded>,
    /// 用户动作失败的通知。目前只有非 Windows 的自启测试读它：Windows 上那条路径
    /// 会真的改系统，测试不碰。
    #[cfg_attr(windows, allow(dead_code))]
    pub notices: Receiver<Notice>,
    pub dir: PathBuf,
}

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mesh-engine-test-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 假 core：记下 Start（连 spec 一起），Stop 立即回 Ok。
fn harness(tag: &str) -> Harness {
    let dir = temp_dir(tag);
    let store = Store::open(&dir).unwrap();
    let log = Arc::new(Log::open(&dir.join("log").join("client.log")).unwrap());
    let (self_tx, inbox) = channel();
    let (core_tx, commands) = channel();
    let (record_tx, record_rx) = channel();
    std::thread::spawn(move || loop {
        match commands.recv() {
            Ok(CoreCmd::Start(spec)) => {
                if record_tx.send(Recorded::Start(spec)).is_err() {
                    return;
                }
            }
            Ok(CoreCmd::Stop { reply }) => {
                let _ = record_tx.send(Recorded::Stop);
                let _ = reply.send(Ok(()));
            }
            Err(_) => return,
        }
    });
    let cell = Arc::new(StateCell::new(Published::default()));
    let (notice_tx, notice_rx) = channel();
    let engine = Engine {
        config: Config {
            root: dir.clone(),
            program_dir: dir.join("bin"),
            self_version: "2026.11.26-1".to_string(),
            hostname: "test-host".to_string(),
            auto_update_disabled: false,
        },
        store,
        cell,
        notices: notice_tx,
        shutdown: Shutdown::new(),
        log,
        log_error: String::new(),
        inbox,
        self_tx,
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
        update: Default::default(),
        auto_tried: None,
        next_download: 0,
        launcher: Default::default(),
        autostart_on: false,
        identity: Default::default(),
        hints: Default::default(),
        release_key: release_key_hex(),
        local_ip: String::new(),
        core_version: String::new(),
        core_path_used: String::new(),
        network_name: String::new(),
        stun: Default::default(),
        hb_stop: None,
        hb_gen: 0,
        stun_stop: None,
        closed: false,
    };
    // 安装根里放一份假 core：locate 只认安装根的 bin/，测试走的就是这条路。
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let placed = bin.join(CORE_FILE);
    if std::fs::hard_link(fake_core(), &placed).is_err() {
        std::fs::copy(fake_core(), &placed).unwrap();
    }
    Harness {
        engine,
        core: record_rx,
        notices: notice_rx,
        dir,
    }
}

impl Harness {
    fn next_core(&self) -> Recorded {
        self.core
            .recv_timeout(Duration::from_secs(5))
            .expect("假 core 没有收到命令")
    }

    fn expect_no_core(&self) {
        match self.core.recv_timeout(Duration::from_millis(300)) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(_) => panic!("不该给 core 发命令"),
            Err(err) => panic!("core 通道异常: {err}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 安装根 `bin/` 里 core 的文件名，与 `mesh_core::locate` 的 CORE_NAME 一致。
const CORE_FILE: &str = if cfg!(windows) {
    "easytier-core.exe"
} else {
    "easytier-core"
};

/// 假 core 可执行文件：与 mesh-core 的集成测试用同一个例子程序。
fn fake_core() -> PathBuf {
    let name = if cfg!(windows) {
        "fake_core.exe"
    } else {
        "fake_core"
    };
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest.join("../../target/debug/examples").join(name),
        manifest.join("../../target/release/examples").join(name),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("找不到假 core {name}：先跑 `cargo build --examples -p mesh-core`");
}

fn saved_client(harness: &Harness) -> ClientConfig {
    saved_client_named(harness, "")
}

fn saved_client_named(harness: &Harness, hostname: &str) -> ClientConfig {
    let config = ClientConfig {
        server_url: "http://127.0.0.1:1".to_string(),
        device_id: "dev-1".to_string(),
        hostname: hostname.to_string(),
        created_at: 1_700_000_000,
    };
    harness.engine.store.save_client(&config).unwrap();
    config
}

/// 装一个假连接并把它交给引擎。
///
/// 下载线程真的会去连 127.0.0.1:1（随即失败）：测试只要求「当前有一条连接」，
/// 不要求真的下载到什么。
fn api(harness: &mut Harness) -> Arc<remote::Client> {
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness.engine.api = Some(Arc::clone(&api));
    api
}

fn running_core() -> CoreRun {
    CoreRun {
        gen: 1,
        pid: 4242,
        cred_id: "cred-1".to_string(),
        rpc_addr: "127.0.0.1:15888".parse::<SocketAddr>().unwrap(),
        started: true,
        started_unix: 1_700_000_000,
    }
}

fn save_cred(harness: &Harness, id: &str) {
    harness
        .engine
        .store
        .save_credential(&Credential {
            id: id.to_string(),
            // 磁盘上的私钥是 base64（内容不重要，但必须能编解码往返）。
            secret: "Y3JlZC0x".to_string(),
            expires_at: 0,
            protect: String::new(),
        })
        .unwrap();
}

// ------------------------------------------------------------ 401 的三支处理

#[test]
fn revoked_clears_the_device_identity() {
    let mut harness = harness("revoked");
    saved_client(&harness);
    let cleared = harness
        .engine
        .handle_auth_rejected(&remote::Error::Unauthorized {
            code: "revoked".to_string(),
            server_time: None,
        });
    assert!(cleared, "revoked 必须立刻清身份");
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "");
    let snapshot = harness.engine.snapshot();
    assert_eq!(snapshot.status, "unconfigured");
    assert!(
        snapshot.last_error.contains("revoked"),
        "{}",
        snapshot.last_error
    );
}

#[test]
fn a_clock_skew_only_degrades_and_keeps_the_identity() {
    let mut harness = harness("skew");
    saved_client(&harness);
    // 本机比服务端快 10 分钟（Date 头是服务端时间）。
    let server_time = SystemTime::now() - Duration::from_secs(600);
    for attempt in 1..=10 {
        let cleared = harness
            .engine
            .handle_auth_rejected(&remote::Error::Unauthorized {
                code: "unauthorized".to_string(),
                server_time: Some(server_time),
            });
        assert!(!cleared, "时钟偏差不能清身份（第 {attempt} 次）");
    }
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1", "device_id 被时钟偏差清掉了");
    let snapshot = harness.engine.snapshot();
    assert_eq!(snapshot.status, "degraded");
    assert!(
        snapshot.last_error.contains("本机时间"),
        "{}",
        snapshot.last_error
    );
}

#[test]
fn three_plain_401s_clear_the_identity() {
    let mut harness = harness("401");
    saved_client(&harness);
    let plain = remote::Error::Unauthorized {
        code: "unauthorized".to_string(),
        server_time: None,
    };
    assert!(!harness.engine.handle_auth_rejected(&plain), "第 1 次不清");
    assert!(!harness.engine.handle_auth_rejected(&plain), "第 2 次不清");
    assert!(harness.engine.handle_auth_rejected(&plain), "第 3 次才清");
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "");
}

#[test]
fn a_successful_request_resets_the_auth_failure_counter() {
    let mut harness = harness("reset");
    saved_client(&harness);
    let plain = remote::Error::Unauthorized {
        code: "unauthorized".to_string(),
        server_time: None,
    };
    for _ in 0..2 {
        assert!(!harness.engine.handle_auth_rejected(&plain));
    }
    harness.engine.reset_auth_fails();
    // 计数清零之后，再连两次仍然不该清身份。
    for _ in 0..2 {
        assert!(!harness.engine.handle_auth_rejected(&plain));
    }
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1");
}

// ------------------------------------------------------------ 凭据上报（F1、F8）

/// 控制面把上报的 id 当成「设备正在用这张」的证据：确认之后才撤销旧的。
/// 所以只有 core 真的加载过的那张才配上报名，磁盘上刚写的那张不算。
#[test]
fn credential_id_is_only_the_one_the_core_confirmed() {
    let mut harness = harness("cred");
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-2");
    harness.engine.refresh_identity();
    // 磁盘上有、core 还没起来：持有但未确认。报空串，服务端既不确认也不签发
    // ——此刻确认等于替一张还没被加载的私钥背书。
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Held("cred-2".to_string())
    );
    assert_eq!(harness.engine.credential_id(), "");
    // core 起来了，加载的是 cred-2：这才是「设备在用这张」。
    harness.engine.core = Some(CoreRun {
        cred_id: "cred-2".to_string(),
        ..running_core()
    });
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Loaded("cred-2".to_string())
    );
    assert_eq!(harness.engine.credential_id(), "cred-2");
    // 磁盘换成 cred-1，core 还在用 cred-2：报 core 那张（确认切换要等新进程），
    // 同时判定为过期，下一轮要换 core。
    save_cred(&harness, "cred-1");
    assert_eq!(harness.engine.credential_id(), "cred-2");
    assert!(harness.engine.credential_stale());
    // core 停下后凭据随之作废：又回到「持有但未确认」，不再替它背书。
    harness.engine.core = None;
    assert_eq!(harness.engine.credential_id(), "");
    assert!(
        !harness.engine.credential_stale(),
        "没在跑的 core 没有「过期」"
    );
}

#[test]
fn a_missing_credential_is_reported_as_none() {
    let mut harness = harness("cred-none");
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    harness.engine.refresh_identity();
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Missing
    );
    assert_eq!(harness.engine.credential_id(), "none");
}

#[test]
fn a_formal_node_reports_an_empty_credential_id() {
    let mut harness = harness("formal");
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n[network_identity]\nnetwork_secret = \"s3cret\"\n")
        .unwrap();
    harness.engine.refresh_identity();
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Formal
    );
    assert_eq!(harness.engine.credential_id(), "");
    assert!(!harness.engine.credential_stale());
    // 正式节点就算磁盘上还留着一张凭据，也不该把它算成自己的身份。
    save_cred(&harness, "left-over");
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Formal
    );
}

// ------------------------------------------------------------ 配置下发与 core 启停

#[test]
fn a_new_config_is_written_and_the_core_started() {
    let mut harness = harness("apply");
    let config = saved_client_named(&harness, "laptop");
    save_cred(&harness, "cred-1");
    let toml = "hostname = \"laptop\"\nipv4 = \"10.126.126.100/24\"\n";
    harness
        .engine
        .apply_locked(&config, toml, &config_digest(toml))
        .unwrap();
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
    match harness.next_core() {
        Recorded::Start(spec) => {
            assert_eq!(spec.core_path, harness.dir.join("bin").join(CORE_FILE));
            assert_eq!(spec.config_path, harness.engine.store.config_path());
            assert_eq!(spec.hostname, "laptop");
            // 就绪判据来自刚写下去的配置。
            assert_eq!(spec.expect.hostname, "laptop");
            match spec.identity {
                mesh_core::Identity::Credential { id, secret } => {
                    assert_eq!(id, "cred-1");
                    assert_eq!(secret, "Y3JlZC0x");
                }
                _ => panic!("凭据节点应当用 ET_CREDENTIAL"),
            }
        }
        _ => panic!("应先启动 core"),
    }
    assert!(harness.engine.core.is_some(), "core 状态应记下来");
}

#[test]
fn an_unchanged_config_neither_rewrites_nor_restarts() {
    let mut harness = harness("same");
    let config = saved_client(&harness);
    let toml = "hostname = \"laptop\"\nipv4 = \"10.126.126.100/24\"\n";
    harness.engine.store.write_config(toml).unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.core = Some(running_core());
    harness
        .engine
        .apply_locked(&config, toml, &config_digest(toml))
        .unwrap();
    harness.expect_no_core();
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
}

#[test]
fn an_unchanged_config_still_starts_a_core_that_is_not_running() {
    let mut harness = harness("restart-after-crash");
    let config = saved_client(&harness);
    let toml = "hostname = \"laptop\"\nipv4 = \"10.126.126.100/24\"\n";
    harness.engine.store.write_config(toml).unwrap();
    save_cred(&harness, "cred-1");
    harness
        .engine
        .apply_locked(&config, toml, &config_digest(toml))
        .unwrap();
    assert!(matches!(harness.next_core(), Recorded::Start(_)));
}

#[test]
fn a_changed_config_stops_the_old_core_before_starting_the_new_one() {
    let mut harness = harness("replace");
    let config = saved_client_named(&harness, "new");
    harness
        .engine
        .store
        .write_config("hostname = \"old\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.core = Some(running_core());
    let toml = "hostname = \"new\"\nipv4 = \"10.126.126.100/24\"\n";
    harness
        .engine
        .apply_locked(&config, toml, &config_digest(toml))
        .unwrap();
    assert!(matches!(harness.next_core(), Recorded::Stop), "先停旧的");
    match harness.next_core() {
        Recorded::Start(spec) => assert_eq!(spec.hostname, "new"),
        _ => panic!("再起新的"),
    }
}

#[test]
fn starting_without_a_credential_keeps_the_control_plane_alive() {
    let mut harness = harness("no-credential");
    saved_client(&harness);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    harness.engine.refresh_identity();
    assert!(!harness.engine.start_core_or_report(false));
    let snapshot = harness.engine.snapshot();
    assert_eq!(snapshot.status, "error");
    assert!(
        snapshot.last_error.contains("启动 core 失败"),
        "{}",
        snapshot.last_error
    );
    harness.expect_no_core();
}

#[test]
fn a_short_lived_core_grows_the_revive_backoff() {
    let mut harness = harness("revive");
    // 从未起过 core：第一档。
    assert_eq!(harness.engine.next_revive_attempt(), 1);
    // 上一代刚起来就退出：档位继续放大。
    harness.engine.last_core_started_at = Some(Instant::now());
    assert_eq!(harness.engine.next_revive_attempt(), 2);
    assert_eq!(harness.engine.next_revive_attempt(), 3);
    // 上一代活够了：回到第一档。
    harness.engine.last_core_started_at = Some(Instant::now() - Duration::from_secs(60));
    assert_eq!(harness.engine.next_revive_attempt(), 1);
    // core 根本没起来（StartFailed）也要放大，否则会每秒拉起一个进程。
    harness.engine.revive_attempt = 0;
    assert_eq!(harness.engine.on_start_failed_attempt(), 1);
    assert_eq!(harness.engine.on_start_failed_attempt(), 2);
}

#[test]
fn a_started_core_marks_online_when_connected() {
    let mut harness = harness("started");
    let key = harness.engine.store.signing_key().unwrap();
    harness.engine.api = Some(Arc::new(remote::Client::new(
        "http://127.0.0.1:1",
        "dev-1",
        key,
    )));
    harness.engine.core = Some(CoreRun {
        started: false,
        ..running_core()
    });
    harness.engine.on_core_event(mesh_core::Event::Started {
        gen: 1,
        pid: 4242,
        node: mesh_etrpc::NodeInfo {
            peer_id: 7,
            ipv4: "10.126.126.100/24".to_string(),
            hostname: "laptop".to_string(),
            inst_id: String::new(),
            version: "2.7.1-abc".to_string(),
            stun: Default::default(),
        },
    });
    let snapshot = harness.engine.snapshot();
    assert_eq!(snapshot.status, "online");
    assert!(snapshot.core_running);
    assert_eq!(snapshot.core_pid, 4242);
    assert_eq!(snapshot.local_ip, "10.126.126.100");
    assert!(snapshot.core_started_at > 1_700_000_000, "就绪时刻应被刷新");
}

#[test]
fn a_stale_core_event_is_ignored() {
    let mut harness = harness("stale");
    harness.engine.core = Some(running_core());
    harness.engine.on_core_event(mesh_core::Event::Exited {
        gen: 99,
        err: Some("退出码 1".to_string()),
    });
    // 代号对不上：这是主动停止/替换留下的迟到事件，不该触发重拉。
    assert!(harness.engine.core.is_some());
    assert!(harness.engine.revive.is_none());
    // 当前这一代退出才排重拉，并且状态降级。
    harness.engine.on_core_event(mesh_core::Event::Exited {
        gen: 1,
        err: Some("退出码 1".to_string()),
    });
    assert!(harness.engine.core.is_none());
    assert!(harness.engine.revive.is_some());
    assert_eq!(harness.engine.snapshot().status, "degraded");
}

// ------------------------------------------------------------ 线程入口

#[test]
fn the_engine_thread_starts_publishes_and_exits() {
    let dir = temp_dir("spawn");
    let cell = Arc::new(StateCell::new(Published::default()));
    let (notice_tx, _notice_rx) = channel();
    let shutdown = Shutdown::new();
    let (tx, handle) = crate::spawn(
        Config {
            root: dir.clone(),
            program_dir: dir.join("bin"),
            self_version: "2026.11.26-9".to_string(),
            hostname: "test-host".to_string(),
            auto_update_disabled: false,
        },
        Arc::clone(&cell),
        notice_tx,
        Arc::clone(&shutdown),
    );
    // 没有 client.json：状态停在该停在的地方，但快照要先发出来。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let published = cell.get();
        if published.snapshot.app_version == "2026.11.26-9" {
            assert_eq!(published.snapshot.status, "unconfigured");
            assert!(published.rpc.is_none(), "没有 core 就不该有 rpc 地址");
            drop(tx);
            shutdown.run();
            handle.join().expect("引擎线程应干净退出");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("引擎没有发布首份快照");
}

// ------------------------------------------------------------ 用户动作

// 真建一条 `Mesh Client` 任务会改掉跑测试的这台机器的开机行为，所以成功的
// SetAutostart 只在真机验收里走，单元测试只覆盖两个平台各自能安全覆盖的那一半。

#[cfg(not(windows))]
#[test]
fn autostart_reports_a_missing_capability() {
    let mut harness = harness("autostart");
    let (reply_tx, reply_rx) = channel();
    let restart = harness.engine.handle(Cmd::SetAutostart(true, reply_tx));
    assert!(!restart, "自启失败不该触发重连");
    let reply = reply_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(reply.is_err(), "非 Windows 上没有自启能力");
    match harness
        .notices
        .recv_timeout(Duration::from_secs(5))
        .expect("失败动作要通知 client 弹框")
    {
        Notice::ActionFailed(message) => {
            assert!(message.contains("开机自启"), "message = {message}")
        }
    }
}

#[cfg(windows)]
#[test]
fn autostart_is_reported_as_supported() {
    let harness = harness("autostart");
    assert!(
        harness.engine.snapshot().autostart_supported,
        "Windows 上快照要如实报告这项能力，否则面板与托盘都不会出现入口"
    );
}

// ------------------------------------------------------------ 心跳

#[test]
fn heartbeat_without_a_running_core_asks_for_a_reconnect() {
    let mut harness = harness("hb-restart");
    let key = harness.engine.store.signing_key().unwrap();
    harness.engine.api = Some(Arc::new(remote::Client::new(
        "http://127.0.0.1:1",
        "dev-1",
        key,
    )));
    harness.engine.on_heartbeat(harness.engine.hb_gen, Ok(None));
    match harness.engine.inbox.try_recv() {
        Ok(Cmd::Reconnect) => {}
        Ok(_) => panic!("应当排一个重连命令"),
        Err(err) => panic!("应当排一个重连命令: {err}"),
    }
}

#[test]
fn a_successful_heartbeat_recovers_the_status() {
    let mut harness = harness("hb-recover");
    let key = harness.engine.store.signing_key().unwrap();
    harness.engine.api = Some(Arc::new(remote::Client::new(
        "http://127.0.0.1:1",
        "dev-1",
        key,
    )));
    harness.engine.core = Some(running_core());
    harness
        .engine
        .set_status(Status::Error, "上一次失败".to_string());
    harness.engine.on_heartbeat(harness.engine.hb_gen, Ok(None));
    let snapshot = harness.engine.snapshot();
    assert_eq!(snapshot.status, "online");
    assert_eq!(snapshot.last_error, "");
    assert!(snapshot.last_heartbeat > 0);
}

#[test]
fn a_heartbeat_401_does_not_clear_the_identity_on_the_first_strike() {
    let mut harness = harness("hb-401");
    let key = harness.engine.store.signing_key().unwrap();
    harness.engine.api = Some(Arc::new(remote::Client::new(
        "http://127.0.0.1:1",
        "dev-1",
        key,
    )));
    saved_client(&harness);
    harness.engine.on_heartbeat(
        harness.engine.hb_gen,
        Err(remote::Error::Unauthorized {
            code: "unauthorized".to_string(),
            server_time: None,
        }),
    );
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1");
    assert_eq!(harness.engine.snapshot().status, "degraded");
}

/// 上一代心跳线程带回来的结果属于旧连接：重连之后它不能再改状态、不能再下发。
#[test]
fn a_stale_heartbeat_result_is_ignored() {
    let mut harness = harness("hb-stale");
    let key = harness.engine.store.signing_key().unwrap();
    harness.engine.api = Some(Arc::new(remote::Client::new(
        "http://127.0.0.1:1",
        "dev-1",
        key,
    )));
    harness.engine.core = Some(running_core());
    harness
        .engine
        .set_status(Status::Error, "上一次失败".to_string());
    // 重连过一次：当前代次是 2，代次 1 的结果已经过期。
    harness.engine.hb_gen = 2;

    harness.engine.on_heartbeat(1, Ok(None));

    assert_eq!(
        harness.engine.snapshot().status,
        "error",
        "旧连接的心跳不能把状态改回 online"
    );
    assert_eq!(
        harness.engine.snapshot().last_heartbeat,
        0,
        "旧连接的心跳不该刷新 last_heartbeat"
    );

    // 当前代次照常生效。
    harness.engine.on_heartbeat(2, Ok(None));
    assert_eq!(harness.engine.snapshot().status, "online");
}
// ------------------------------------------------------------ 更新

/// 合法的完整摘要（64 位十六进制）。发布链要求摘要必填，测试里的 offer 必须带它。
fn digest(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

/// 测试用的发布私钥。真正的发布私钥不在这台机器上，所以测试自己生成一把：
/// 公钥交给引擎，私钥用来签 offer——「有效签名被接受」这条路径才测得到。
fn release_signing_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
}

/// 引擎验签用的那一半。
fn release_key_hex() -> String {
    hex::encode(release_signing_key().verifying_key().to_bytes())
}

/// 按发布私钥的规矩给一个槽位签出签名（hex）。
fn sign_release(slot: &str, version: &str, sha256: &str) -> String {
    use ed25519_dalek::Signer;
    let message =
        remote::signing::signing_string(slot, mesh_install::platform::id(), version, sha256);
    hex::encode(release_signing_key().sign(message.as_bytes()).to_bytes())
}

/// 改了 offer 的版本或摘要之后重新签一次，否则验签会先把它拦下来。
fn resign(info: &mut remote::UpdateInfo) {
    info.signature = sign_release(remote::signing::SLOT_CLIENT, &info.version, &info.sha256);
}

fn update_info(version: &str) -> remote::UpdateInfo {
    let sha256 = digest(0xab);
    remote::UpdateInfo {
        version: version.to_string(),
        url: "http://127.0.0.1:1/pkg.tar.zst".to_string(),
        notes: String::new(),
        signature: sign_release(remote::signing::SLOT_CLIENT, version, &sha256),
        sha256,
        auto_update: false,
        unchanged: false,
    }
}

/// 精简响应：服务端确认“你手上就是这份包”，只留开关。
fn slim_update_info(version: &str, auto_update: bool) -> remote::UpdateInfo {
    remote::UpdateInfo {
        version: version.to_string(),
        url: String::new(),
        notes: String::new(),
        sha256: String::new(),
        signature: String::new(),
        auto_update,
        unchanged: true,
    }
}

/// 一条已经“到手”的 offer（Ready：内容齐、可安装）。
fn ready_offer(version: &str, sha256: &str) -> UpdateState {
    UpdateState {
        phase: Phase::Ready,
        version: version.to_string(),
        notes: "旧说明".to_string(),
        url: format!("http://127.0.0.1:1/{version}.tar.zst"),
        sha256: sha256.to_string(),
        path: Some(PathBuf::from("/tmp/pkg.tar.zst")),
        sum: sha256.to_string(),
        ..UpdateState::default()
    }
}

fn beat_known(harness: &Harness) -> Option<String> {
    harness.engine.publish();
    harness.engine.cell.get().beat.app_update_known.clone()
}

#[test]
fn an_invalid_version_is_ignored_before_downloading() {
    let mut harness = harness("update-invalid");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("../../evil")));
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert_eq!(harness.engine.update.version, "2026.11.26-2");
    assert_ne!(harness.engine.update.phase, Phase::Idle);
}

/// 一次已经失败、正在退避中的下载。
fn failed_download(harness: &mut Harness, attempts: u32, retry_in: i64) {
    harness.engine.update.phase = Phase::Failed;
    harness.engine.update.error = "下载中断".to_string();
    harness.engine.update.attempts = attempts;
    harness.engine.update.retry_at = Some(if retry_in >= 0 {
        Instant::now() + Duration::from_secs(retry_in as u64)
    } else {
        Instant::now() - Duration::from_secs((-retry_in) as u64)
    });
}

#[test]
fn a_failure_is_kept_until_the_backoff_expires() {
    let mut harness = harness("update-failure");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 1, 30);
    // 退避还没到：心跳既不能把失败态抹成“已就绪”，也不能抢着重下。
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert_eq!(harness.engine.update.phase, Phase::Failed);
    assert_eq!(harness.engine.update.error, "下载中断");
}

#[test]
fn a_failed_download_is_retried_once_the_backoff_expires() {
    let mut harness = harness("update-retry");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 1, -1);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert_eq!(harness.engine.update.phase, Phase::Downloading);
    assert!(harness.engine.update.error.is_empty(), "重试时错误行要清掉");
}

#[test]
fn downloading_gives_up_after_the_attempt_limit() {
    let mut harness = harness("update-give-up");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 5, -1);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert_eq!(
        harness.engine.update.phase,
        Phase::Failed,
        "次数用完就该停下来等人处理，不能一直刷发布文件服务"
    );
}

/// 固定路径上的包是「请安装」的交接信号，不是客户端的临时文件：启动清理与作废
/// 一次下载都不许动它（F3）。
#[test]
fn the_engine_never_clears_the_pending_package() {
    let mut harness = harness("pending-package");
    let pending = harness.dir.join("update.tar.zst");
    std::fs::write(&pending, b"pending").unwrap();
    // 启动时的临时目录清理只碰 update/。
    let temp = harness.dir.join("update");
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(temp.join("launcher.new"), b"half-download").unwrap();
    harness.engine.clean_update_dir();
    assert!(pending.is_file(), "启动清理删掉了待安装的包");
    assert!(!temp.join("launcher.new").exists());
    // 撤回之后它也在。
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));
    harness.engine.clear_stale_update();
    assert!(pending.is_file(), "撤回不能替安装事务清包");
}
#[cfg(windows)]
#[test]
fn a_missing_package_is_downloaded_again_when_the_user_asks_to_apply() {
    let mut harness = harness("update-redownload");
    let api = api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    harness.engine.api = Some(Arc::clone(&api));
    // 下载失败之后的状态：没有本地包。面板上的“重试”就是在这个状态上点的。
    failed_download(&mut harness, 1, 30);
    harness.engine.update.path = None;
    let result = harness.engine.apply_update();
    assert!(result.is_ok(), "点重试要重新下一份，而不是报“包不在本地”");
    assert_eq!(harness.engine.update.phase, Phase::Downloading);
    assert!(
        harness.engine.update.apply_after_download,
        "用户点了重试，下完就该直接装"
    );
    assert_eq!(
        harness.engine.update.attempts, 0,
        "用户主动重试要把次数还回来"
    );
}

#[test]
fn a_new_version_resets_the_failure() {
    let mut harness = harness("update-new-version");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 3, 60);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-3")));
    assert_eq!(harness.engine.update.version, "2026.11.26-3");
    assert_ne!(harness.engine.update.phase, Phase::Failed);
    assert_eq!(
        harness.engine.update.attempts, 0,
        "换版本要重新给满重试次数"
    );
}

#[test]
fn a_download_in_progress_is_not_overwritten_by_a_heartbeat() {
    let mut harness = harness("update-busy");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    harness.engine.update.busy = true;
    harness.engine.update.phase = Phase::Downloading;
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-3")));
    assert_eq!(harness.engine.update.version, "2026.11.26-2");
    assert_eq!(harness.engine.update.phase, Phase::Downloading);
    assert!(harness.engine.update.busy);
}

#[test]
fn nothing_is_reported_before_the_first_offer() {
    let harness = harness("known-none");
    harness.engine.publish();
    assert!(harness.engine.cell.get().beat.app_update_known.is_none());
    assert!(harness.engine.known_app_update().is_none());
}

#[test]
fn a_full_offer_is_reported_in_the_next_heartbeat() {
    let mut harness = harness("known-full");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    // 完整响应把摘要记进内存：下一轮心跳据此告诉服务端“我手上是这一份”。
    assert_eq!(beat_known(&harness).as_deref(), Some(digest(0xab).as_str()));
}

#[test]
fn a_slim_offer_keeps_the_offer_and_does_not_download() {
    let mut harness = harness("known-slim");
    api(&mut harness);
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));
    harness.engine.update.auto = false;
    harness
        .engine
        .offer_update(Some(&slim_update_info("2026.11.26-2", true)));

    // ① 不覆盖已有信息：进度、版本、说明、地址、摘要、包路径全都还是原来那份。
    assert_eq!(harness.engine.update.phase, Phase::Ready);
    assert_eq!(harness.engine.update.version, "2026.11.26-2");
    assert_eq!(harness.engine.update.notes, "旧说明");
    assert_eq!(
        harness.engine.update.url,
        "http://127.0.0.1:1/2026.11.26-2.tar.zst"
    );
    assert_eq!(harness.engine.update.sha256, digest(0xab));
    assert!(harness.engine.update.path.is_some());
    assert!(!harness.engine.update.busy);
    // ② 不触发下载：Windows 的完整分支会把它改成 Downloading，这里必须还是 Ready。
    assert_ne!(harness.engine.update.phase, Phase::Downloading);
    // ③ 只把开关更新掉（管理员可能刚改过设备级覆盖）。
    assert!(harness.engine.update.auto);
    // ④ 下一轮心跳仍然报同一份摘要：known 派生自 offer 的 sha，不受 phase 影响。
    assert_eq!(beat_known(&harness).as_deref(), Some(digest(0xab).as_str()));
}

#[test]
fn a_slim_offer_with_nothing_known_is_a_no_op() {
    let mut harness = harness("known-slim-empty");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&slim_update_info("2026.11.26-2", true)));
    // 手上没有 offer（理论上不该出现：重启后会带不上 known，服务端就该回完整内容）。
    // 这里只要求：不 panic、不凭空造出一份下载任务、开关照常更新。
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
    // 没有 offer 就没有「这份 offer 的授权」可记：开关随下一次完整响应到达。
    assert!(!harness.engine.update.auto);
    assert!(beat_known(&harness).is_none());
}

#[test]
fn clearing_the_offer_stops_reporting_it() {
    let mut harness = harness("known-clear");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert!(beat_known(&harness).is_some());
    // 服务端不再提供更新（设备已升级或发布被撤回）：offer 清掉，known 跟着消失，
    // 否则会一直“认领”一份已经不存在的发布。
    harness.engine.update.phase = Phase::Ready;
    harness.engine.clear_stale_update();
    assert!(harness.engine.update.version.is_empty());
    assert!(beat_known(&harness).is_none());
}

#[test]
fn an_unsigned_offer_is_ignored() {
    let mut harness = harness("update-unsigned");
    api(&mut harness);
    let mut info = update_info("2026.11.26-2");
    info.signature.clear();
    harness.engine.offer_update(Some(&info));
    // 摘要与版本都合法，只有签名缺席：那也不能记下来，否则下一轮心跳会把它
    // 当成“已就绪”交给启动器。
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
}

#[test]
fn a_signature_for_another_slot_is_ignored() {
    let mut harness = harness("update-wrong-slot");
    api(&mut harness);
    let mut info = update_info("2026.11.26-2");
    // 槽位是签名串的一行：同一份字节签成启动器槽位，客户端包这边就不该通过。
    info.signature = sign_release(remote::signing::SLOT_LAUNCHER, &info.version, &info.sha256);
    harness.engine.offer_update(Some(&info));
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
}

#[test]
fn a_launcher_with_an_invalid_signature_is_not_replaced() {
    let mut harness = harness("launcher-signature");
    let api = api(&mut harness);
    let sum = digest(0xcd);
    let info = remote::LauncherInfo {
        version: "2026.11.26-2".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_CLIENT, "2026.11.26-2", &sum),
    };
    harness.engine.apply_launcher(&api, Some(&info));
    // 没进替换流程：tried 只在验签通过之后才占位。
    assert!(harness.engine.launcher.tried.is_none());
}

#[test]
fn a_signed_launcher_is_offered_for_replacement() {
    let mut harness = harness("launcher-signed");
    let api = api(&mut harness);
    let sum = digest(0xce);
    let info = remote::LauncherInfo {
        version: "2026.11.26-2".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_LAUNCHER, "2026.11.26-2", &sum),
    };
    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(harness.engine.launcher.tried.as_deref(), Some(sum.as_str()));
}

/// 盘上那份启动器自报的版本比下发的更新才换：摘要证明来源，不证明新鲜度。
#[test]
fn a_launcher_older_than_the_recorded_one_is_refused() {
    let mut harness = harness("launcher-older");
    let api = api(&mut harness);
    let sum = digest(0xcf);
    let info = remote::LauncherInfo {
        version: "2026.11.26-2".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_LAUNCHER, "2026.11.26-2", &sum),
    };
    // 盘上这份自报 2026.11.26-5：下发的 2026.11.26-2 更旧。
    harness.engine.launcher.version = Some("2026.11.26-5".to_string());

    harness.engine.apply_launcher(&api, Some(&info));

    assert!(
        !harness.engine.config.root.join("update").exists(),
        "更旧的启动器不该走到建暂存目录那一步"
    );
    assert_eq!(
        harness.engine.launcher.tried.as_deref(),
        Some(sum.as_str()),
        "记下结论，下一轮心跳不再重判同一份"
    );
}

/// 比记录新的照常进替换流程。
#[test]
fn a_launcher_newer_than_the_recorded_one_passes() {
    let mut harness = harness("launcher-newer");
    let api = api(&mut harness);
    let sum = digest(0xd0);
    let info = remote::LauncherInfo {
        version: "2026.11.26-7".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_LAUNCHER, "2026.11.26-7", &sum),
    };
    harness.engine.launcher.version = Some("2026.11.26-5".to_string());

    harness.engine.apply_launcher(&api, Some(&info));

    assert_eq!(harness.engine.launcher.tried.as_deref(), Some(sum.as_str()));
}

/// 新进程启动时只从盘上读一次事实：启动器自报的版本号 + 文件摘要。
#[test]
fn a_fresh_process_reads_the_launcher_facts_from_the_install_root() {
    let harness = harness("launcher-startup");
    let launcher = harness
        .engine
        .config
        .root
        .join(mesh_install::platform::launcher_name());
    std::fs::write(&launcher, b"launcher-bytes").unwrap();
    mesh_install::layout::write_launcher_version(&harness.engine.config.root, "2026.11.26-7")
        .unwrap();

    let facts = crate::update::LauncherState::read(&harness.engine.config.root);

    assert_eq!(facts.version.as_deref(), Some("2026.11.26-7"));
    assert_eq!(
        facts.sum,
        crate::sync::file_digest(&launcher).unwrap(),
        "摘要描述的就是安装根里那份文件"
    );
}

/// 提交包要拿安装根写权（`install.lock`）：启动器正装着东西时客户端必须让路，
/// 但包不能丢——留成 `.part`，下一轮心跳接着提交。
#[test]
fn committing_a_download_waits_for_the_install_root() {
    let mut harness = harness("download-lock");
    // 别人（启动器）正握着安装根。
    let owner =
        mesh_install::lock::acquire(&mesh_install::lock::Lock::owner_path(&harness.dir)).unwrap();
    let sha = digest(0xab);
    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-2".to_string(),
        sha256: sha.clone(),
    };
    harness.engine.next_download = 1;
    harness.engine.update = ready_offer("2026.11.26-2", &sha);
    harness.engine.update.phase = Phase::Downloading;
    harness.engine.update.task = Some(task.clone());
    harness.engine.update.path = None;
    let part = harness.dir.join("update.tar.zst.part");
    std::fs::write(&part, b"package-bytes").unwrap();

    harness.engine.on_download(task, Ok(part.clone()));

    // 拿不到写权：包留在临时文件里，固定路径没出现，也不算一次下载失败。
    assert_eq!(harness.engine.update.phase, Phase::Ready);
    assert!(harness.engine.update.path.is_none());
    assert!(part.is_file(), "包不能丢：下一轮还要提交它");
    assert!(!harness.dir.join("update.tar.zst").exists());
    assert!(harness.engine.update.error.is_empty());
    assert_eq!(harness.engine.update.attempts, 0);

    // 启动器让开之后，下一轮把它提交掉。
    drop(owner);
    harness.engine.resume_commit();
    assert!(harness.dir.join("update.tar.zst").is_file());
    assert!(!part.exists());
    assert_eq!(harness.engine.update.sum, sha);
}

/// 没拿到安装根写权不算结论：同一份启动器 offer 下一轮还要再试。
#[test]
fn a_launcher_replacement_blocked_by_the_install_root_is_retried() {
    let mut harness = harness("launcher-retry");
    let api = api(&mut harness);
    let sum = digest(0xd5);
    let info = remote::LauncherInfo {
        version: "2026.11.26-9".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_LAUNCHER, "2026.11.26-9", &sum),
    };
    harness.engine.launcher.tried = Some(sum.clone());

    harness
        .engine
        .on_launcher_result("2026.11.26-9".to_string(), sum.clone(), false, true);
    assert!(harness.engine.launcher.tried.is_none(), "没拿到锁不算结论");

    // 下一轮心跳拿同一份 offer 会重新进替换流程。
    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(harness.engine.launcher.tried.as_deref(), Some(sum.as_str()));
}
/// 替换成功之后，引擎对「盘上那份」的事实必须跟着换：否则下一份 offer 会拿旧版本
/// 比新旧，把设备换回更旧的启动器（F6）。
#[test]
fn a_replaced_launcher_updates_the_facts_and_keeps_the_version_monotonic() {
    let mut harness = harness("launcher-facts");
    let api = api(&mut harness);
    harness.engine.launcher.sum = digest(0xdd);
    harness.engine.launcher.version = Some("2026.11.26-5".to_string());

    // 换到 -7：盘上那份现在是它。
    harness
        .engine
        .on_launcher_result("2026.11.26-7".to_string(), digest(0xd1), true, false);
    assert_eq!(
        harness.engine.launcher.version.as_deref(),
        Some("2026.11.26-7")
    );
    assert_eq!(harness.engine.launcher.sum, digest(0xd1));

    // 之后下发的 -6 比盘上那份旧：不能换。
    let mut info = remote::LauncherInfo {
        version: "2026.11.26-6".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: digest(0xd2),
        signature: sign_release(
            remote::signing::SLOT_LAUNCHER,
            "2026.11.26-6",
            &digest(0xd2),
        ),
    };
    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(
        harness.engine.launcher.tried.as_deref(),
        Some(digest(0xd2).as_str())
    );
    assert!(
        !harness.engine.config.root.join("update").exists(),
        "更旧的启动器不该走到下载"
    );

    // 一次进程里第二次发布：-8 比现有的新，照常接受。
    info.version = "2026.11.26-8".to_string();
    info.sha256 = digest(0xd3);
    info.signature = sign_release(
        remote::signing::SLOT_LAUNCHER,
        "2026.11.26-8",
        &digest(0xd3),
    );
    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(
        harness.engine.launcher.tried.as_deref(),
        Some(digest(0xd3).as_str())
    );
}

/// 替换失败：盘上那份没变，事实也不变；下一份 offer 仍然按盘上的版本判断新旧。
#[test]
fn a_failed_launcher_replacement_keeps_the_facts() {
    let mut harness = harness("launcher-failed");
    harness.engine.launcher.version = Some("2026.11.26-5".to_string());
    harness.engine.launcher.sum = digest(0xdd);

    harness
        .engine
        .on_launcher_result("2026.11.26-9".to_string(), digest(0xd4), false, false);

    assert_eq!(
        harness.engine.launcher.version.as_deref(),
        Some("2026.11.26-5")
    );
    assert_eq!(harness.engine.launcher.sum, digest(0xdd));
}
#[test]
fn a_failed_download_is_retried_on_a_slim_offer() {
    let mut harness = harness("slim-retry");
    api(&mut harness);
    // 下载失败过：客户端照旧把这份 offer 的摘要报给服务端，服务端于是回精简形状。
    // 重试必须在这里发生，否则最常见的失败路径永远没有第二次机会。
    let mut offer = ready_offer("2026.11.26-2", &digest(0xab));
    offer.phase = Phase::Failed;
    offer.attempts = 1;
    offer.retry_at = Some(Instant::now());
    harness.engine.update = offer;

    harness
        .engine
        .offer_update(Some(&slim_update_info("2026.11.26-2", false)));
    assert_eq!(harness.engine.update.phase, Phase::Downloading);
}

#[test]
fn a_ready_offer_is_left_alone_by_a_slim_offer() {
    let mut harness = harness("slim-ready");
    api(&mut harness);
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));
    harness
        .engine
        .offer_update(Some(&slim_update_info("2026.11.26-2", false)));
    // 已就绪的那份不该因为「服务端说你已经知道」被重下或重置。
    assert_eq!(harness.engine.update.phase, Phase::Ready);
}

#[test]
fn a_republished_version_replaces_the_offer() {
    let mut harness = harness("known-republish");
    api(&mut harness);
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xaa));
    // 同一版本号重新发布：内容变了，摘要不同就是新的一份，必须换掉而不是按“同版本”跳过。
    let mut info = update_info("2026.11.26-2");
    info.sha256 = digest(0xbb);
    info.url = "http://127.0.0.1:1/new.tar.zst".to_string();
    resign(&mut info);
    harness.engine.offer_update(Some(&info));
    assert_eq!(harness.engine.update.sha256, digest(0xbb));
    assert_eq!(harness.engine.update.url, "http://127.0.0.1:1/new.tar.zst");
    // 同一版本、同一摘要：还按“手上就是它”跳过，不反复重下。
    let before = harness.engine.update.phase;
    harness.engine.offer_update(Some(&info));
    assert_eq!(harness.engine.update.phase, before);
    assert_eq!(harness.engine.update.sha256, digest(0xbb));
}

#[test]
fn the_switch_is_taken_as_given_with_a_local_veto() {
    let mut harness = harness("known-switch");
    api(&mut harness);
    // 服务端说“别自动装”：照单全收，不再自己叠一层判断。
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert!(!harness.engine.update.auto);
    // 服务端说“可以自动装”：照单全收。
    let mut info = update_info("2026.11.26-2");
    info.auto_update = true;
    harness.engine.offer_update(Some(&info));
    assert!(harness.engine.update.auto);
    // 本地一票否决仍然有效（--disable-auto-update）。
    harness.engine.config.auto_update_disabled = true;
    harness
        .engine
        .offer_update(Some(&slim_update_info("2026.11.26-2", true)));
    assert!(!harness.engine.update.auto);
}

/// 下载中撤回：offer 清掉，旧任务的完成消息作废，而且不许动固定路径上的包——
/// 那是「请安装」的交接信号，只有消耗它的安装事务能清（F3）。
#[test]
fn withdrawing_an_offer_during_download_discards_the_task_result() {
    let mut harness = harness("update-withdraw");
    let sha = digest(0xab);
    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-2".to_string(),
        sha256: sha.clone(),
    };
    harness.engine.update = ready_offer("2026.11.26-2", &sha);
    harness.engine.update.phase = Phase::Downloading;
    harness.engine.update.task = Some(task.clone());
    harness.engine.update.apply_after_download = true;
    // 早先已经交接出去的一份包：文件名本身就是信号。
    let pending = harness.dir.join("update.tar.zst");
    std::fs::write(&pending, b"pending-package").unwrap();

    // 服务端不再提供更新。
    harness.engine.clear_stale_update();
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
    assert!(
        harness.engine.update.task.is_some(),
        "线程还在跑：不许放第二个进来写同一个 .part"
    );

    // 旧任务的结果回来：它已经不代表任何被授权的 offer。
    let part = harness.dir.join("update.tar.zst.part");
    std::fs::write(&part, b"stale-package").unwrap();
    harness.engine.on_download(task, Ok(part.clone()));
    assert_eq!(
        harness.engine.update.phase,
        Phase::Idle,
        "旧结果不能把 offer 复活"
    );
    assert!(harness.engine.update.task.is_none());
    assert!(!part.exists(), "作废的下载只该清掉自己的临时文件");
    assert_eq!(
        std::fs::read(&pending).unwrap(),
        b"pending-package",
        "固定路径上的包不归客户端清（F3）"
    );
}

/// 同版本号重发：内容变了就是另一份 offer。旧任务的完成消息不能把旧内容
/// 认成新那份，更不能让它变成「请安装」。
#[test]
fn a_republished_version_invalidates_the_download_in_flight() {
    let mut harness = harness("update-republish-download");
    api(&mut harness);
    // 旧任务由引擎分配过代次（1），所以下一个是 2。
    harness.engine.next_download = 1;
    let old_sha = digest(0xab);
    let old = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-2".to_string(),
        sha256: old_sha.clone(),
    };
    harness.engine.update = ready_offer("2026.11.26-2", &old_sha);
    harness.engine.update.phase = Phase::Downloading;
    harness.engine.update.task = Some(old.clone());

    let mut info = update_info("2026.11.26-2");
    info.sha256 = digest(0xbb);
    info.url = "http://127.0.0.1:1/new.tar.zst".to_string();
    resign(&mut info);
    harness.engine.offer_update(Some(&info));

    assert_eq!(harness.engine.update.sha256, digest(0xbb), "换成新那一份");
    assert_eq!(
        harness
            .engine
            .update
            .task
            .as_ref()
            .map(|task| task.sha256.clone()),
        Some(old_sha.clone()),
        "旧任务还在跑，不许起第二个"
    );

    let part = harness.dir.join("update.tar.zst.part");
    std::fs::write(&part, b"old-bytes").unwrap();
    harness.engine.on_download(old.clone(), Ok(part.clone()));
    assert!(!part.exists(), "作废的结果只该清掉临时文件");
    assert!(
        !harness.dir.join("update.tar.zst").exists(),
        "旧内容不许变成「请安装」"
    );
    assert_eq!(harness.engine.update.sha256, digest(0xbb));
    match harness.engine.update.task.as_ref() {
        // Windows 预取：旧任务让路之后立刻重下新那份。
        Some(task) => {
            assert_eq!(task.sha256, digest(0xbb));
            assert_ne!(task.gen, old.gen);
        }
        // Unix：等用户点安装再下。
        None => assert_eq!(harness.engine.update.phase, Phase::Ready),
    }
}

/// 自动安装资格属于 offer 本身：没有授权不装，同一份 offer 只试一次；
/// 同版本重发是另一份（摘要不同），该给它一次机会。
#[test]
fn auto_apply_is_a_property_of_the_offer() {
    let mut harness = harness("auto-offer");
    api(&mut harness);
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));
    harness.engine.update.auto = false;
    harness.engine.auto_apply();
    assert!(
        harness.engine.auto_tried.is_none(),
        "没有授权不该记「试过」"
    );

    harness.engine.update.auto = true;
    harness.engine.auto_tried = Some(digest(0xab));
    harness.engine.auto_apply();
    assert_eq!(
        harness.engine.update.phase,
        Phase::Ready,
        "同一份 offer 只试一次"
    );

    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xbb));
    harness.engine.update.auto = true;
    harness.engine.auto_apply();
    assert_eq!(
        harness.engine.auto_tried.as_deref(),
        Some(digest(0xbb).as_str()),
        "换了内容就是另一份 offer"
    );
}
// ------------------------------------------------------------ 设置与注册

/// 假服务端记下的请求：(请求行, 请求体)。
type Requests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Stub {
    url: String,
    requests: Requests,
}

impl Stub {
    fn new(responses: Vec<(u16, Vec<u8>)>) -> Stub {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                sink.lock().unwrap().push(request);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Stub { url, requests }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> (String, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body).unwrap();
    }
    (request_line, body)
}

#[test]
fn registering_persists_the_identity_and_leaves_activation_to_the_loop() {
    let reply = r#"{"deviceId":"d-server","configToml":"hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n","configHash":"h1"}"#;
    let stub = Stub::new(vec![(200, reply.as_bytes().to_vec())]);
    let mut harness = harness("register");
    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: stub.url.clone(),
            token: "token-1".to_string(),
            hostname: "laptop".to_string(),
        })
        .unwrap();
    // 已注册：身份立刻落盘，token 已经用掉，后面的失败不能再要求一张新的。
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "d-server");
    assert_eq!(client.server_url, stub.url);
    assert!(client.created_at > 0);
    assert_eq!(harness.engine.snapshot().status, "connecting");
    // 待激活：配置不在这里写，写 config.toml 与换 core 只由 apply_locked 一处
    // 完成；下一步由连接循环用心跳把这一代的配置要下来。
    assert!(harness.engine.store.read_config().unwrap().is_none());
    assert!(harness.engine.store.load_credential().unwrap().is_none());
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[0].0, "POST /api/v1/register HTTP/1.1");
    let body = String::from_utf8_lossy(&requests[0].1);
    assert!(body.contains("\"token\":\"token-1\""), "body = {body}");
    assert!(body.contains("\"hostname\":\"laptop\""), "body = {body}");
    assert!(body.contains("\"pubkey\":"), "body = {body}");
}

/// 已注册、待激活的状态不需要令牌：退出再启动之后，一次普通心跳就能把这一代的
/// 配置补下来（服务端发现 configHash 对不上就随响应下发整份配置）。
#[test]
fn a_registered_identity_is_activated_by_a_plain_heartbeat() {
    let reply = r#"{"deviceId":"d-server","configToml":"","configHash":"h1"}"#;
    let toml = "hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n";
    // 上面那份 TOML 的 base64（engine 不依赖 base64，测试里直接写死）。
    let config_b64 = "aG9zdG5hbWUgPSAibGFwdG9wIgppcHY0ID0gIjEwLjAuMC4xLzI0Igo=";
    let beat = format!(r#"{{"configHash":"h1","configB64":"{config_b64}"}}"#);
    let stub = Stub::new(vec![
        (200, reply.as_bytes().to_vec()),
        (200, beat.as_bytes().to_vec()),
    ]);
    let mut harness = harness("register-activate");
    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: stub.url.clone(),
            token: "token-1".to_string(),
            hostname: "laptop".to_string(),
        })
        .unwrap();
    assert!(harness.engine.store.read_config().unwrap().is_none());

    // 模拟「进程重启」：进程里没有任何内存状态，只有盘上那份身份。
    let config = harness.engine.store.load_client().unwrap().unwrap();
    harness
        .engine
        .connect(&config)
        .map_err(|err| match err {
            crate::sync::ConnectError::Failed(err) => err,
            crate::sync::ConnectError::IdentityInvalid => "身份失效".to_string(),
        })
        .unwrap();

    // 配置由心跳补下来了；不需要再消费一次注册令牌。
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "第二次是心跳，不是重放注册");
    assert_eq!(requests[1].0, "POST /api/v1/heartbeat HTTP/1.1");
}

/// 换代次（这里只换控制面地址、设备 id 不变）必须把上一代的凭据与配置一起作废：
/// 凭据属于某个控制面上的某台设备，配置属于那个控制面上的某个网络。
#[test]
fn switching_the_control_plane_drops_the_previous_generation() {
    let reply = r#"{"deviceId":"same-id","configToml":"","configHash":"h1"}"#;
    let stub = Stub::new(vec![(200, reply.as_bytes().to_vec())]);
    let mut harness = harness("register-switch");
    harness
        .engine
        .store
        .save_client(&ClientConfig {
            server_url: "http://127.0.0.1:9".to_string(),
            device_id: "same-id".to_string(),
            hostname: String::new(),
            created_at: 1,
        })
        .unwrap();
    harness
        .engine
        .store
        .write_config("hostname = \"old\"\n")
        .unwrap();
    save_cred(&harness, "cred-old");
    // 上一代控制面下发的 offer：换代次之后不该还留着它的授权。
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));

    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: stub.url.clone(),
            token: "token-1".to_string(),
            hostname: String::new(),
        })
        .unwrap();

    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.server_url, stub.url);
    assert_eq!(client.device_id, "same-id", "设备 id 没变，代次却变了");
    assert!(
        harness.engine.store.load_credential().unwrap().is_none(),
        "上一代控制面的凭据必须作废"
    );
    assert!(
        harness.engine.store.read_config().unwrap().is_none(),
        "上一代控制面的配置必须作废"
    );
    assert!(
        harness.engine.update.version.is_empty(),
        "上一代控制面的更新 offer 必须作废"
    );
}

/// 同一控制面上重新注册（设备 id 也没变）不是换代次：本地配置与凭据都还有效。
#[test]
fn re_registering_on_the_same_control_plane_keeps_the_local_state() {
    let reply = r#"{"deviceId":"same-id","configToml":"","configHash":"h1"}"#;
    let stub = Stub::new(vec![(200, reply.as_bytes().to_vec())]);
    let mut harness = harness("register-same");
    harness
        .engine
        .store
        .save_client(&ClientConfig {
            server_url: stub.url.clone(),
            device_id: "same-id".to_string(),
            hostname: String::new(),
            created_at: 1,
        })
        .unwrap();
    harness
        .engine
        .store
        .write_config("hostname = \"kept\"\n")
        .unwrap();
    save_cred(&harness, "cred-kept");

    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: stub.url.clone(),
            token: "token-2".to_string(),
            hostname: String::new(),
        })
        .unwrap();

    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some("hostname = \"kept\"\n")
    );
    assert_eq!(
        harness.engine.store.load_credential().unwrap().unwrap().id,
        "cred-kept"
    );
}

/// 设备名是 core 的启动参数：改了就得换一个进程，没改就不该停掉正在跑的那个。
#[test]
fn changing_the_device_name_replaces_the_core() {
    let mut harness = harness("settings-hostname");
    let config = saved_client_named(&harness, "old");
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    harness.engine.refresh_identity();
    harness.engine.client = Some(config);
    harness.engine.core = Some(running_core());

    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: "http://127.0.0.1:1".to_string(),
            token: String::new(),
            hostname: "new".to_string(),
        })
        .unwrap();
    assert!(
        matches!(harness.next_core(), Recorded::Stop),
        "换名字要换进程"
    );

    // 名字没变：正在跑的 core 不动（它已经是这个身份了）。
    harness.engine.core = Some(running_core());
    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: "http://127.0.0.1:1".to_string(),
            token: String::new(),
            hostname: "new".to_string(),
        })
        .unwrap();
    harness.expect_no_core();
}

#[test]
fn saving_without_a_token_needs_an_existing_identity() {
    let mut harness = harness("settings-no-token");
    let err = harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: "http://10.0.0.1:8088".to_string(),
            token: String::new(),
            hostname: String::new(),
        })
        .unwrap_err();
    assert!(err.contains("尚未注册"), "err = {err}");
}

// ------------------------------------------------------------ 下载交接

#[test]
fn the_package_becomes_the_fixed_name_only_when_it_is_accepted() {
    let payload = b"package-bytes".to_vec();
    let stub = Stub::new(vec![(200, payload.clone())]);
    let harness = harness("download");
    let key = harness.engine.store.signing_key().unwrap();
    let api = remote::Client::new(&stub.url, "dev-1", key);
    let want = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&payload))
    };
    // 下载只写 .part：这一刻它还不是「请安装」的信号。
    let part = crate::update::download_package(&api, &harness.dir, &stub.url, &want).unwrap();
    assert_eq!(part.file_name().unwrap(), "update.tar.zst.part");
    assert_eq!(std::fs::read(&part).unwrap(), payload);
    assert!(!harness.dir.join("update.tar.zst").exists());
    // 结果被接受之后才改名成固定路径。
    let path = crate::update::commit_package(&part, &harness.dir).unwrap();
    assert_eq!(path.file_name().unwrap(), "update.tar.zst");
    assert_eq!(std::fs::read(&path).unwrap(), payload);
    assert!(
        !harness.dir.join("update.tar.zst.part").exists(),
        ".part 必须改名掉，不能留下半截文件"
    );
}

#[test]
fn a_missing_digest_is_refused() {
    let stub = Stub::new(vec![(200, b"package-bytes".to_vec())]);
    let harness = harness("download-no-digest");
    let key = harness.engine.store.signing_key().unwrap();
    let api = remote::Client::new(&stub.url, "dev-1", key);
    let err = crate::update::download_package(&api, &harness.dir, &stub.url, "").unwrap_err();
    assert!(err.contains("没有给出包摘要"), "err = {err}");
    assert!(!harness.dir.join("update.tar.zst.part").exists());
    assert!(!harness.dir.join("update.tar.zst").exists());
}

#[test]
fn a_mismatched_digest_is_refused() {
    let stub = Stub::new(vec![(200, b"package-bytes".to_vec())]);
    let harness = harness("download-mismatch");
    let key = harness.engine.store.signing_key().unwrap();
    let api = remote::Client::new(&stub.url, "dev-1", key);
    let err = crate::update::download_package(&api, &harness.dir, &stub.url, &"0".repeat(64))
        .unwrap_err();
    assert!(err.contains("包摘要不符"), "err = {err}");
    assert!(!harness.dir.join("update.tar.zst").exists());
}
/// 摘要必填：没有完整摘要的 offer 不下载、不提示，也不覆盖手上已有的那份。
#[test]
fn an_offer_without_a_full_digest_is_ignored() {
    let mut harness = harness("update-bad-digest");
    api(&mut harness);
    for bad in ["", "ab", &digest(0xab)[..63]] {
        let mut info = update_info("2026.11.26-9");
        info.sha256 = bad.to_string();
        harness.engine.offer_update(Some(&info));
        assert_eq!(harness.engine.update.phase, Phase::Idle, "digest = {bad:?}");
        assert!(harness.engine.update.version.is_empty(), "digest = {bad:?}");
    }
}
