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
        auto_update: false,
        auto_tried: None,
        launcher_sum: String::new(),
        launcher_tried: None,
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
        stun_stop: None,
        closed: false,
    };
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

fn saved_client(harness: &Harness, core_path: &Path) -> ClientConfig {
    saved_client_named(harness, core_path, "")
}

fn saved_client_named(harness: &Harness, core_path: &Path, hostname: &str) -> ClientConfig {
    let config = ClientConfig {
        server_url: "http://127.0.0.1:1".to_string(),
        device_id: "dev-1".to_string(),
        hostname: hostname.to_string(),
        created_at: 1_700_000_000,
        core_path: core_path.display().to_string(),
    };
    harness.engine.store.save_client(&config).unwrap();
    config
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
    saved_client(&harness, Path::new(""));
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
    saved_client(&harness, Path::new(""));
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
    saved_client(&harness, Path::new(""));
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
    saved_client(&harness, Path::new(""));
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

// ------------------------------------------------------------ 凭据上报（F1）

#[test]
fn credential_id_is_the_one_the_core_loaded() {
    let mut harness = harness("cred");
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-2");
    harness.engine.refresh_identity();
    // 还没有 core 认领任何一张：报磁盘上的，服务端据此确认与续期。
    assert_eq!(harness.engine.credential_id(), "cred-2");
    // core 还在用 cred-1，磁盘已经换成 cred-2：必须报 core 那张。
    harness.engine.core = Some(running_core());
    assert_eq!(harness.engine.credential_id(), "cred-1");
    // core 停下后凭据随之作废，回到磁盘上那张。
    harness.engine.core = None;
    assert_eq!(harness.engine.credential_id(), "cred-2");
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
    assert_eq!(harness.engine.credential_id(), "");
    assert!(!harness.engine.credential_stale());
}

// ------------------------------------------------------------ 配置下发与 core 启停

#[test]
fn a_new_config_is_written_and_the_core_started() {
    let mut harness = harness("apply");
    let config = saved_client_named(&harness, &fake_core(), "laptop");
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
            assert_eq!(spec.core_path, fake_core());
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
    let config = saved_client(&harness, &fake_core());
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
    let config = saved_client(&harness, &fake_core());
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
    let config = saved_client_named(&harness, &fake_core(), "new");
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
    saved_client(&harness, &fake_core());
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
    harness.engine.on_heartbeat(Ok(None));
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
    harness.engine.on_heartbeat(Ok(None));
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
    saved_client(&harness, Path::new(""));
    harness
        .engine
        .on_heartbeat(Err(remote::Error::Unauthorized {
            code: "unauthorized".to_string(),
            server_time: None,
        }));
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1");
    assert_eq!(harness.engine.snapshot().status, "degraded");
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
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("../../evil")));
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
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
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 1, 30);
    // 退避还没到：心跳既不能把失败态抹成“已就绪”，也不能抢着重下。
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    assert_eq!(harness.engine.update.phase, Phase::Failed);
    assert_eq!(harness.engine.update.error, "下载中断");
}

#[test]
fn a_failed_download_is_retried_once_the_backoff_expires() {
    let mut harness = harness("update-retry");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 1, -1);
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    assert_eq!(harness.engine.update.phase, Phase::Downloading);
    assert!(harness.engine.update.error.is_empty(), "重试时错误行要清掉");
}

#[test]
fn downloading_gives_up_after_the_attempt_limit() {
    let mut harness = harness("update-give-up");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 5, -1);
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    assert_eq!(
        harness.engine.update.phase,
        Phase::Failed,
        "次数用完就该停下来等人处理，不能一直刷发布文件服务"
    );
}

#[cfg(windows)]
#[test]
fn a_missing_package_is_downloaded_again_when_the_user_asks_to_apply() {
    let mut harness = harness("update-redownload");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
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
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    failed_download(&mut harness, 3, 60);
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-3")));
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
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    harness.engine.update.busy = true;
    harness.engine.update.phase = Phase::Downloading;
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-3")));
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
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    // 完整响应把摘要记进内存：下一轮心跳据此告诉服务端“我手上是这一份”。
    assert_eq!(beat_known(&harness).as_deref(), Some(digest(0xab).as_str()));
}

#[test]
fn a_slim_offer_keeps_the_offer_and_does_not_download() {
    let mut harness = harness("known-slim");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));
    harness.engine.auto_update = false;
    harness
        .engine
        .offer_update(&api, Some(&slim_update_info("2026.11.26-2", true)));

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
    assert!(harness.engine.auto_update);
    // ④ 下一轮心跳仍然报同一份摘要：known 派生自 offer 的 sha，不受 phase 影响。
    assert_eq!(beat_known(&harness).as_deref(), Some(digest(0xab).as_str()));
}

#[test]
fn a_slim_offer_with_nothing_known_is_a_no_op() {
    let mut harness = harness("known-slim-empty");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&slim_update_info("2026.11.26-2", true)));
    // 手上没有 offer（理论上不该出现：重启后会带不上 known，服务端就该回完整内容）。
    // 这里只要求：不 panic、不凭空造出一份下载任务、开关照常更新。
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
    assert!(harness.engine.auto_update);
    assert!(beat_known(&harness).is_none());
}

#[test]
fn clearing_the_offer_stops_reporting_it() {
    let mut harness = harness("known-clear");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
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
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    let mut info = update_info("2026.11.26-2");
    info.signature.clear();
    harness.engine.offer_update(&api, Some(&info));
    // 摘要与版本都合法，只有签名缺席：那也不能记下来，否则下一轮心跳会把它
    // 当成“已就绪”交给启动器。
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
}

#[test]
fn a_signature_for_another_slot_is_ignored() {
    let mut harness = harness("update-wrong-slot");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    let mut info = update_info("2026.11.26-2");
    // 槽位是签名串的一行：同一份字节签成启动器槽位，客户端包这边就不该通过。
    info.signature = sign_release(remote::signing::SLOT_LAUNCHER, &info.version, &info.sha256);
    harness.engine.offer_update(&api, Some(&info));
    assert_eq!(harness.engine.update.phase, Phase::Idle);
    assert!(harness.engine.update.version.is_empty());
}

#[test]
fn a_launcher_with_an_invalid_signature_is_not_replaced() {
    let mut harness = harness("launcher-signature");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    let sum = digest(0xcd);
    let info = remote::LauncherInfo {
        version: "2026.11.26-2".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_CLIENT, "2026.11.26-2", &sum),
    };
    harness.engine.apply_launcher(&api, Some(&info));
    // 没进替换流程：tried 只在验签通过之后才占位。
    assert!(harness.engine.launcher_tried.is_none());
}

#[test]
fn a_signed_launcher_is_offered_for_replacement() {
    let mut harness = harness("launcher-signed");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    let sum = digest(0xce);
    let info = remote::LauncherInfo {
        version: "2026.11.26-2".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_LAUNCHER, "2026.11.26-2", &sum),
    };
    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(harness.engine.launcher_tried.as_deref(), Some(sum.as_str()));
}

#[test]
fn a_failed_download_is_retried_on_a_slim_offer() {
    let mut harness = harness("slim-retry");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    // 下载失败过：客户端照旧把这份 offer 的摘要报给服务端，服务端于是回精简形状。
    // 重试必须在这里发生，否则最常见的失败路径永远没有第二次机会。
    let mut offer = ready_offer("2026.11.26-2", &digest(0xab));
    offer.phase = Phase::Failed;
    offer.attempts = 1;
    offer.retry_at = Some(Instant::now());
    harness.engine.update = offer;

    harness
        .engine
        .offer_update(&api, Some(&slim_update_info("2026.11.26-2", false)));
    assert_eq!(harness.engine.update.phase, Phase::Downloading);
}

#[test]
fn a_ready_offer_is_left_alone_by_a_slim_offer() {
    let mut harness = harness("slim-ready");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xab));
    harness
        .engine
        .offer_update(&api, Some(&slim_update_info("2026.11.26-2", false)));
    // 已就绪的那份不该因为「服务端说你已经知道」被重下或重置。
    assert_eq!(harness.engine.update.phase, Phase::Ready);
}

#[test]
fn a_republished_version_replaces_the_offer() {
    let mut harness = harness("known-republish");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness.engine.update = ready_offer("2026.11.26-2", &digest(0xaa));
    // 同一版本号重新发布：内容变了，摘要不同就是新的一份，必须换掉而不是按“同版本”跳过。
    let mut info = update_info("2026.11.26-2");
    info.sha256 = digest(0xbb);
    info.url = "http://127.0.0.1:1/new.tar.zst".to_string();
    resign(&mut info);
    harness.engine.offer_update(&api, Some(&info));
    assert_eq!(harness.engine.update.sha256, digest(0xbb));
    assert_eq!(harness.engine.update.url, "http://127.0.0.1:1/new.tar.zst");
    // 同一版本、同一摘要：还按“手上就是它”跳过，不反复重下。
    let before = harness.engine.update.phase;
    harness.engine.offer_update(&api, Some(&info));
    assert_eq!(harness.engine.update.phase, before);
    assert_eq!(harness.engine.update.sha256, digest(0xbb));
}

#[test]
fn the_switch_is_taken_as_given_with_a_local_veto() {
    let mut harness = harness("known-switch");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    // 服务端说“别自动装”：照单全收，不再自己叠一层判断。
    harness
        .engine
        .offer_update(&api, Some(&update_info("2026.11.26-2")));
    assert!(!harness.engine.auto_update);
    // 服务端说“可以自动装”：照单全收。
    let mut info = update_info("2026.11.26-2");
    info.auto_update = true;
    harness.engine.offer_update(&api, Some(&info));
    assert!(harness.engine.auto_update);
    // 本地一票否决仍然有效（--disable-auto-update）。
    harness.engine.config.auto_update_disabled = true;
    harness
        .engine
        .offer_update(&api, Some(&slim_update_info("2026.11.26-2", true)));
    assert!(!harness.engine.auto_update);
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
fn registering_saves_the_identity_and_the_config() {
    let reply = r#"{"deviceId":"d-server","configToml":"hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n","configHash":"h1"}"#;
    let stub = Stub::new(vec![(200, reply.as_bytes().to_vec())]);
    let mut harness = harness("register");
    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: stub.url.clone(),
            token: "token-1".to_string(),
            hostname: "laptop".to_string(),
            core_path: String::new(),
        })
        .unwrap();
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "d-server");
    assert_eq!(client.server_url, stub.url);
    assert!(client.created_at > 0);
    let config = harness.engine.store.read_config().unwrap().unwrap();
    assert!(
        config.contains("hostname = \"laptop\""),
        "config = {config}"
    );
    assert!(harness.engine.store.load_credential().unwrap().is_none());
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[0].0, "POST /api/v1/register HTTP/1.1");
    let body = String::from_utf8_lossy(&requests[0].1);
    assert!(body.contains("\"token\":\"token-1\""), "body = {body}");
    assert!(body.contains("\"hostname\":\"laptop\""), "body = {body}");
    assert!(body.contains("\"pubkey\":"), "body = {body}");
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
            core_path: String::new(),
        })
        .unwrap_err();
    assert!(err.contains("尚未注册"), "err = {err}");
}

// ------------------------------------------------------------ 下载交接

#[test]
fn the_package_lands_at_the_fixed_name_with_its_digest() {
    let payload = b"package-bytes".to_vec();
    let stub = Stub::new(vec![(200, payload.clone())]);
    let harness = harness("download");
    let key = harness.engine.store.signing_key().unwrap();
    let api = remote::Client::new(&stub.url, "dev-1", key);
    let want = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&payload))
    };
    let path = crate::update::download_update(&api, &harness.dir, &stub.url, &want).unwrap();
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
    let err = crate::update::download_update(&api, &harness.dir, &stub.url, "").unwrap_err();
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
    let err =
        crate::update::download_update(&api, &harness.dir, &stub.url, &"0".repeat(64)).unwrap_err();
    assert!(err.contains("包摘要不符"), "err = {err}");
    assert!(!harness.dir.join("update.tar.zst").exists());
}
/// 摘要必填：没有完整摘要的 offer 不下载、不提示，也不覆盖手上已有的那份。
#[test]
fn an_offer_without_a_full_digest_is_ignored() {
    let mut harness = harness("update-bad-digest");
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    for bad in ["", "ab", &digest(0xab)[..63]] {
        let mut info = update_info("2026.11.26-9");
        info.sha256 = bad.to_string();
        harness.engine.offer_update(&api, Some(&info));
        assert_eq!(harness.engine.update.phase, Phase::Idle, "digest = {bad:?}");
        assert!(harness.engine.update.version.is_empty(), "digest = {bad:?}");
    }
}
