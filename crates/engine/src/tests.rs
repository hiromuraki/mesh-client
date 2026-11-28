//! 引擎单测：用假 core（命令通道）与假服务端（std TcpListener）驱动状态机。
//!
//! 不启动真的 easytier-core、不真的注册设备：这台机器上客户端要 UAC，而且测试应当
//! 只验证“判断”本身。

use crate::log::Log;
use crate::run::{AuthMode, Cmd, Config, CoreRun, CoreSpec, Engine, Notice};
use crate::update::{Artifact, DownloadTask, Intent, Offer, UpdateState, MAX_DOWNLOAD_ATTEMPTS};
use crate::views::{Published, StateCell};
use crate::Shutdown;
use mesh_core::Cmd as CoreCmd;
use mesh_remote as remote;
use mesh_state::{ClientConfig, Credential, Store};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// 假 core 收到的东西。
///
/// 只是测试里的记录通道，`Start` 带着整份 spec 不值得再套 Box。
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
    /// 假 core 的 Stop 是否成功：模拟「确认不了旧数据面停下」。
    stop_ok: Arc<AtomicBool>,
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
    let stop_ok = Arc::new(AtomicBool::new(true));
    let stop_flag = Arc::clone(&stop_ok);
    std::thread::spawn(move || loop {
        match commands.recv() {
            Ok(CoreCmd::Start(spec)) => {
                if record_tx.send(Recorded::Start(spec)).is_err() {
                    return;
                }
            }
            Ok(CoreCmd::Stop { reply }) => {
                let _ = record_tx.send(Recorded::Stop);
                let result = if stop_flag.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("core 停不下来".to_string())
                };
                let _ = reply.send(result);
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
        client: None,
        api: None,
        control: Default::default(),
        last_hb: 0,
        last_error: String::new(),
        fault: String::new(),
        core: None,
        last_core_started_at: None,
        revive_attempt: 0,
        next_gen: 0,
        revive: None,
        auth_fails: 0,
        update: Default::default(),
        next_task: 0,
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
        stun_stop: None,
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
        stop_ok,
    }
}

impl Harness {
    fn next_core(&self) -> Recorded {
        self.core
            .recv_timeout(Duration::from_secs(5))
            .expect("假 core 没有收到命令")
    }

    /// 让假 core 的 Stop 失败或恢复成功。
    fn set_stop_ok(&self, ok: bool) {
        self.stop_ok.store(ok, Ordering::SeqCst);
    }

    fn expect_no_core(&self) {
        match self.core.recv_timeout(Duration::from_millis(300)) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(_) => panic!("不该给 core 发命令"),
            Err(err) => panic!("core 通道异常: {err}"),
        }
    }

    /// 发布一次快照并返回它：快照是推导值，先经过 `publish` 的收口。
    fn published(&mut self) -> crate::Snapshot {
        self.engine.publish();
        self.engine.cell.get().snapshot.clone()
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

/// 造一个能过启动闸门权限校验的槽：root/bin 与两个可执行文件。
///
/// Windows 的闸门只收紧 ACL，Unix 的闸门还看属主与 mode；这个测试直接调用
/// 生产入口 `spawn`，所以两边都要满足。
fn install_for_spawn(dir: &Path) -> PathBuf {
    let slot = dir.join("bin");
    std::fs::create_dir_all(&slot).unwrap();
    for name in [
        mesh_install::platform::client_exe(),
        mesh_install::platform::core_name(),
    ] {
        let path = slot.join(name);
        std::fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    slot
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
fn api(harness: &mut Harness) -> Arc<remote::Client> {
    let key = harness.engine.store.signing_key().unwrap();
    let api = Arc::new(remote::Client::new("http://127.0.0.1:1", "dev", key));
    harness.engine.api = Some(Arc::clone(&api));
    api
}

fn idle_spec() -> CoreSpec {
    CoreSpec {
        generation: "g".to_string(),
        config_hash: "h".to_string(),
        hostname: "test-host".to_string(),
        auth: AuthMode::Credential("cred-1".to_string()),
    }
}

fn running_core() -> CoreRun {
    CoreRun {
        gen: 1,
        pid: 4242,
        spec: idle_spec(),
        rpc_addr: "127.0.0.1:15888".parse::<SocketAddr>().unwrap(),
        started_unix: 1_700_000_000,
    }
}

/// 一个“已经按当前期望跑起来”的实例：规格就是引擎此刻算出来的那份。
fn applied_core(harness: &Harness) -> CoreRun {
    CoreRun {
        spec: harness.engine.desired_spec().unwrap().unwrap(),
        ..running_core()
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
    let snapshot = harness.published();
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
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness.engine.control.failures = 1;
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
    let snapshot = harness.published();
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
    for _ in 0..2 {
        assert!(!harness.engine.handle_auth_rejected(&plain));
    }
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1");
}

/// 5xx 是服务端自己的临时状况，不是身份失效：按控制面失败退避重试，既不累加
/// 认证失败次数，也不清本地身份（nonce 缓存饱和也走这一支）。
#[test]
fn a_server_error_is_retried_without_touching_the_identity() {
    let mut harness = harness("server-error");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    api(&mut harness);
    for attempt in 1..=5u64 {
        harness.engine.control.gen = attempt;
        harness.engine.on_heartbeat(
            attempt,
            Err(remote::Error::Api {
                status: 503,
                code: "unavailable".to_string(),
                message: "nonce cache full".to_string(),
            }),
        );
    }
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1", "5xx 不能清掉身份");
    assert_eq!(harness.engine.auth_fails, 0, "5xx 不计认证失败");
    assert_eq!(harness.engine.control.failures, 5, "5xx 按控制面失败退避");
    assert!(
        harness.engine.control.due.is_some(),
        "5xx 要继续重试，不能停在无期限"
    );
    assert_eq!(harness.published().status, "degraded");

    // 服务端恢复（204）就回到正常节拍，身份与计数都还是那份。
    harness.engine.control.gen = 6;
    harness.engine.on_heartbeat(6, Ok(None));
    assert_eq!(harness.engine.control.failures, 0);
    assert!(harness.engine.last_hb > 0);
}

// ------------------------------------------------------------ 凭据上报（F8）

#[test]
fn credential_id_is_only_the_one_the_core_confirmed() {
    let mut harness = harness("cred");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-2");
    harness.engine.refresh_identity();
    // 磁盘上有、core 还没起来：持有但未确认。
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Held("cred-2".to_string())
    );
    assert_eq!(harness.engine.credential_id(), "");
    // core 起来了，加载的是 cred-2：这才是「设备在用这张」。
    harness.engine.core = Some(CoreRun {
        spec: CoreSpec {
            auth: AuthMode::Credential("cred-2".to_string()),
            ..idle_spec()
        },
        ..running_core()
    });
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Loaded("cred-2".to_string())
    );
    assert_eq!(harness.engine.credential_id(), "cred-2");
    // 磁盘换成 cred-1，core 还在用 cred-2：报 core 那张。
    save_cred(&harness, "cred-1");
    assert_eq!(harness.engine.credential_id(), "cred-2");
    // 规格随之变成另一份：下一次 ensure_core 会换进程。
    assert_ne!(
        harness.engine.desired_spec().unwrap().unwrap(),
        harness.engine.core.as_ref().unwrap().spec
    );
    // core 停下后凭据随之作废：又回到「持有但未确认」。
    harness.engine.core = None;
    assert_eq!(harness.engine.credential_id(), "");
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
    // 正式节点就算磁盘上还留着一张凭据，也不该把它算成自己的身份。
    save_cred(&harness, "left-over");
    assert_eq!(
        harness.engine.credential_state(),
        crate::sync::CredentialState::Formal
    );
}

/// 面板上的「凭证过期」就是落盘那份凭据的到期时间：心跳下发的凭据带着它，
/// 引擎落盘后重算身份摘要，快照跟着它走（前端据此显示剩余天数）。
#[test]
fn the_panel_shows_the_credential_expiry_from_the_heartbeat() {
    let mut harness = harness("cred-expiry");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    api(&mut harness);
    harness.engine.control.gen = 1;

    harness.engine.on_heartbeat(
        1,
        Ok(Some(remote::Heartbeat {
            credential: "Y3JlZC0x".to_string(),
            credential_id: "cred-9".to_string(),
            credential_expiry: 1_798_000_000,
            ..remote::Heartbeat::default()
        })),
    );

    let stored = harness.engine.store.load_credential().unwrap().unwrap();
    assert_eq!(
        stored.expires_at, 1_798_000_000,
        "到期时间要跟着凭据一起落盘"
    );
    let snapshot = harness.published();
    assert_eq!(snapshot.auth_mode, "credential");
    assert_eq!(
        snapshot.credential_expiry, 1_798_000_000,
        "面板显示的到期时间必须来自落盘的那份凭据"
    );
}

/// 封装坏了就这一轮作废：不写盘，也不回落到响应里的明文字段——能改响应字节的
/// 一方正是这条封装要防的人，回落等于把封装变成可选的装饰。
#[test]
fn a_broken_sealed_credential_is_not_taken_from_the_plaintext() {
    let mut harness = harness("sealed-broken");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    api(&mut harness);
    harness.engine.control.gen = 1;

    harness.engine.on_heartbeat(
        1,
        Ok(Some(remote::Heartbeat {
            credential: "Y3JlZC0x".to_string(),
            credential_sealed: "AAAA".to_string(),
            credential_id: "cred-9".to_string(),
            credential_expiry: 1_798_000_000,
            ..remote::Heartbeat::default()
        })),
    );

    assert!(
        harness.engine.store.load_credential().unwrap().is_none(),
        "坏掉的封装回落到了明文凭据"
    );
    assert!(
        harness.engine.last_error.contains("解封"),
        "错误提示没有说明是解封失败：{}",
        harness.engine.last_error
    );
}

// ------------------------------------------------------------ 配置下发与 core 启停

#[test]
fn a_new_config_is_written_and_the_core_started() {
    let mut harness = harness("apply");
    let config = saved_client_named(&harness, "laptop");
    save_cred(&harness, "cred-1");
    let toml = "hostname = \"laptop\"\nipv4 = \"10.126.126.100/24\"\n";
    harness.engine.apply_locked(&config, toml).unwrap();
    // 写配置只写事实，换不换进程由规格入口决定（唯一收敛点）。
    harness.engine.ensure_core_or_report();
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
    match harness.next_core() {
        Recorded::Start(spec) => {
            assert_eq!(spec.core_path, harness.dir.join("bin").join(CORE_FILE));
            assert_eq!(spec.config_path, harness.engine.store.config_path());
            assert_eq!(spec.hostname, "laptop");
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
    harness.engine.client = Some(config.clone());
    harness.engine.core = Some(applied_core(&harness));
    harness.engine.apply_locked(&config, toml).unwrap();
    harness.expect_no_core();
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
}

/// 正常无变化的心跳没有配置正文：那不是「配置变化却没带正文」的异常，也不该
/// 在这里多算一遍规格。
#[test]
fn a_heartbeat_without_a_config_body_is_not_a_config_change() {
    let mut harness = harness("beat-no-body");
    let config = saved_client(&harness);
    let toml = "hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n";
    harness.engine.store.write_config(toml).unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.client = Some(config);
    harness.engine.core = Some(applied_core(&harness));
    api(&mut harness);
    harness.engine.control.gen = 1;

    // 只带发布信息、没有 configB64：与 204 一样，没有配置要应用。
    harness.engine.on_heartbeat(
        1,
        Ok(Some(remote::Heartbeat {
            config_hash: "stale".to_string(),
            ..remote::Heartbeat::default()
        })),
    );

    harness.expect_no_core();
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml),
        "空正文不该覆盖磁盘上的配置"
    );
    let log = std::fs::read_to_string(harness.dir.join("log").join("client.log")).unwrap();
    let without_body = mesh_text::get(mesh_text::id::ENGINE_LOG_CONFIG_CHANGED_WITHOUT_BODY);
    assert!(!log.contains(without_body), "正常空响应不该记异常：{log}");
}

/// 带正文的心跳：配置落盘，并由最后那一个规格入口换进程。
#[test]
fn a_heartbeat_with_a_config_body_is_applied() {
    let mut harness = harness("beat-body");
    let config = saved_client_named(&harness, "laptop");
    harness.engine.client = Some(config);
    save_cred(&harness, "cred-1");
    api(&mut harness);
    harness.engine.control.gen = 1;
    let body = "aG9zdG5hbWUgPSAibGFwdG9wIgppcHY0ID0gIjEwLjAuMC4xLzI0Igo=";
    harness.engine.on_heartbeat(
        1,
        Ok(Some(remote::Heartbeat {
            config_hash: "h1".to_string(),
            config_b64: body.to_string(),
            ..remote::Heartbeat::default()
        })),
    );
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some("hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n")
    );
    match harness.next_core() {
        Recorded::Start(spec) => assert_eq!(spec.hostname, "laptop"),
        _ => panic!("配置到齐就该起 core"),
    }
    harness.expect_no_core();
}

#[test]
fn the_same_spec_is_never_started_twice() {
    let mut harness = harness("same-spec");
    let config = saved_client(&harness);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.client = Some(config);
    harness.engine.ensure_core().unwrap();
    assert!(matches!(harness.next_core(), Recorded::Start(_)));
    // 规格相同：不重复拉起（第一次的 core 还没就绪，但已经在跑）。
    harness.engine.ensure_core().unwrap();
    harness.expect_no_core();
}

#[test]
fn a_credential_rotation_replaces_the_core() {
    let mut harness = harness("cred-rotate");
    let config = saved_client(&harness);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.client = Some(config);
    harness.engine.core = Some(applied_core(&harness));
    save_cred(&harness, "cred-2");
    harness.engine.ensure_core().unwrap();
    assert!(
        matches!(harness.next_core(), Recorded::Stop),
        "旧凭据的进程要停"
    );
    match harness.next_core() {
        Recorded::Start(spec) => match spec.identity {
            mesh_core::Identity::Credential { id, .. } => assert_eq!(id, "cred-2"),
            _ => panic!("凭据节点应当用 ET_CREDENTIAL"),
        },
        _ => panic!("再起新的"),
    }
}

#[test]
fn an_unchanged_config_still_starts_a_core_that_is_not_running() {
    let mut harness = harness("restart-after-crash");
    let config = saved_client(&harness);
    let toml = "hostname = \"laptop\"\nipv4 = \"10.126.126.100/24\"\n";
    harness.engine.store.write_config(toml).unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.apply_locked(&config, toml).unwrap();
    harness.engine.ensure_core_or_report();
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
    harness.engine.apply_locked(&config, toml).unwrap();
    harness.engine.ensure_core_or_report();
    assert!(matches!(harness.next_core(), Recorded::Stop), "先停旧的");
    match harness.next_core() {
        Recorded::Start(spec) => assert_eq!(spec.hostname, "new"),
        _ => panic!("再起新的"),
    }
}

#[test]
fn starting_without_a_credential_keeps_the_control_plane_alive() {
    let mut harness = harness("no-credential");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    harness.engine.refresh_identity();
    harness.engine.ensure_core_or_report();
    let snapshot = harness.published();
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
    assert_eq!(harness.engine.next_revive_attempt(), 1);
    harness.engine.last_core_started_at = Some(Instant::now());
    assert_eq!(harness.engine.next_revive_attempt(), 2);
    assert_eq!(harness.engine.next_revive_attempt(), 3);
    harness.engine.last_core_started_at = Some(Instant::now() - Duration::from_secs(60));
    assert_eq!(harness.engine.next_revive_attempt(), 1);
    harness.engine.revive_attempt = 0;
    assert_eq!(harness.engine.on_start_failed_attempt(), 1);
    assert_eq!(harness.engine.on_start_failed_attempt(), 2);
}

#[test]
fn a_started_core_marks_online_when_the_heartbeat_is_live() {
    let mut harness = harness("started");
    let config = saved_client(&harness);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.client = Some(config);
    harness.engine.last_hb = 1;
    harness.engine.core = Some(CoreRun {
        pid: 0,
        spec: harness.engine.desired_spec().unwrap().unwrap(),
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
    let snapshot = harness.published();
    assert_eq!(snapshot.status, "online");
    assert!(snapshot.core_running);
    assert_eq!(snapshot.core_pid, 4242);
    assert_eq!(snapshot.local_ip, "10.126.126.100");
    assert!(snapshot.core_started_at > 1_700_000_000, "就绪时刻应被刷新");
}

#[test]
fn a_stale_core_event_is_ignored() {
    let mut harness = harness("stale");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness.engine.core = Some(running_core());
    harness.engine.on_core_event(mesh_core::Event::Exited {
        gen: 99,
        err: Some("退出码 1".to_string()),
    });
    assert!(harness.engine.core.is_some());
    assert!(harness.engine.revive.is_none());
    harness.engine.on_core_event(mesh_core::Event::Exited {
        gen: 1,
        err: Some("退出码 1".to_string()),
    });
    assert!(harness.engine.core.is_none());
    assert!(harness.engine.revive.is_some());
    assert_eq!(harness.published().status, "degraded");
}

#[test]
fn a_stale_stun_result_is_ignored() {
    let mut harness = harness("stun-stale");
    harness.engine.core = Some(running_core());
    let fresh = mesh_etrpc::StunInfo {
        public_ips: vec!["1.2.3.4".to_string()],
        udp_nat_type: "FullCone".to_string(),
        tcp_nat_type: String::new(),
    };
    harness.engine.handle(Cmd::Stun {
        gen: 99,
        info: fresh.clone(),
    });
    assert!(
        harness.engine.stun.public_ips.is_empty(),
        "旧 core 的结果不算数"
    );
    harness.engine.handle(Cmd::Stun {
        gen: 1,
        info: fresh.clone(),
    });
    assert_eq!(harness.engine.stun.public_ips, fresh.public_ips);
}

// ------------------------------------------------------------ 线程入口

#[test]
fn the_engine_thread_starts_publishes_and_exits() {
    let dir = temp_dir("spawn");
    let program_dir = install_for_spawn(&dir);
    let cell = Arc::new(StateCell::new(Published::default()));
    let (notice_tx, _notice_rx) = channel();
    let shutdown = Shutdown::new();
    let (tx, handle) = crate::spawn(
        Config {
            root: dir.clone(),
            program_dir,
            self_version: "2026.11.26-9".to_string(),
            hostname: "test-host".to_string(),
            auto_update_disabled: false,
        },
        Arc::clone(&cell),
        notice_tx,
        Arc::clone(&shutdown),
    )
    .expect("合法的安装目录应放行");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let published = cell.get();
        if published.snapshot.app_version == "2026.11.26-9" {
            assert_eq!(published.snapshot.status, "unconfigured");
            assert!(published.rpc.is_none(), "没有 core 就不该有 rpc 地址");
            shutdown.run();
            // 没有期限时引擎阻塞在 inbox 上：用一条命令把它叫醒，它就会看到收尾已开始。
            let _ = tx.send(Cmd::Reconnect);
            drop(tx);
            handle.join().expect("引擎线程应干净退出");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("引擎没有发布首份快照");
}

// ------------------------------------------------------------ 用户动作

#[cfg(not(windows))]
#[test]
fn autostart_reports_a_missing_capability() {
    let mut harness = harness("autostart");
    let (reply_tx, reply_rx) = channel();
    harness.engine.handle(Cmd::SetAutostart(true, reply_tx));
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

// ------------------------------------------------------------ 心跳与事件循环

#[test]
fn a_heartbeat_without_a_running_core_starts_it() {
    let mut harness = harness("hb-restart");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    api(&mut harness);
    harness.engine.control.gen = 1;
    harness.engine.on_heartbeat(1, Ok(None));
    assert!(matches!(harness.next_core(), Recorded::Start(_)));
}

#[test]
fn a_successful_heartbeat_recovers_the_status() {
    let mut harness = harness("hb-recover");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    api(&mut harness);
    harness.engine.core = Some(applied_core(&harness));
    harness.engine.fault = "上一次失败".to_string();
    harness.engine.control.gen = 1;
    harness.engine.on_heartbeat(1, Ok(None));
    let snapshot = harness.published();
    assert_eq!(snapshot.status, "online");
    assert_eq!(snapshot.last_error, "");
    assert!(snapshot.last_heartbeat > 0);
}

#[test]
fn a_heartbeat_401_does_not_clear_the_identity_on_the_first_strike() {
    let mut harness = harness("hb-401");
    api(&mut harness);
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness.engine.control.gen = 1;
    harness.engine.on_heartbeat(
        1,
        Err(remote::Error::Unauthorized {
            code: "unauthorized".to_string(),
            server_time: None,
        }),
    );
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "dev-1");
    assert_eq!(harness.published().status, "degraded");
}

/// 上一代心跳线程带回来的结果属于旧连接：重连之后它不能再改状态、不能再下发。
#[test]
fn a_stale_heartbeat_result_is_ignored() {
    let mut harness = harness("hb-stale");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    api(&mut harness);
    harness.engine.core = Some(applied_core(&harness));
    harness.engine.fault = "上一次失败".to_string();
    harness.engine.control.failures = 1;
    // 重连过一次：当前代次是 2，代次 1 的结果已经过期。
    harness.engine.control.gen = 2;

    harness.engine.on_heartbeat(1, Ok(None));

    let snapshot = harness.published();
    assert_eq!(snapshot.status, "error", "旧连接的心跳不能把状态改回去");
    assert_eq!(
        snapshot.last_heartbeat, 0,
        "旧连接的心跳不该刷新 last_heartbeat"
    );

    // 当前代次照常生效。
    harness.engine.on_heartbeat(2, Ok(None));
    assert_eq!(harness.published().status, "online");
}

/// 控制面退避期间，core 的重拉期限照常推进——这是 F1 的核心验收。
#[test]
fn a_control_outage_does_not_stop_the_core_revival() {
    let mut harness = harness("outage-revive");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    // 控制面持续不可达：退避排到 60 秒之后。
    harness.engine.control.failures = 4;
    harness.engine.control.due = Some(Instant::now() + Duration::from_secs(60));
    // core 的期限已经到点。
    harness.engine.schedule_revive(1);
    harness.engine.revive.as_mut().unwrap().due = Instant::now();

    let deadline = harness.engine.next_deadline().unwrap();
    assert!(
        deadline < Duration::from_secs(1),
        "下一次唤醒应取 core 的期限：{deadline:?}"
    );
    harness.engine.on_deadline();
    assert!(matches!(harness.next_core(), Recorded::Start(_)));
    assert!(
        harness.engine.control.due.unwrap() > Instant::now() + Duration::from_secs(30),
        "控制面的退避不该被 core 的重拉改掉"
    );
}

/// 首次请求与周期心跳走同一段响应处理：第一次带配置与凭据，第二次没有任何变化。
#[test]
fn the_first_and_periodic_heartbeats_share_one_response_handler() {
    let toml = "hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n";
    let config_b64 = "aG9zdG5hbWUgPSAibGFwdG9wIgppcHY0ID0gIjEwLjAuMC4xLzI0Igo=";
    let first = format!(
        r#"{{"configHash":"h1","configB64":"{config_b64}","credentialId":"cred-1","credential":"Y3JlZC0x"}}"#
    );
    let stub = Stub::new(vec![(200, first.into_bytes()), (204, Vec::new())]);
    let mut harness = harness("beat-unified");
    harness
        .engine
        .store
        .save_client(&ClientConfig {
            server_url: stub.url.clone(),
            device_id: "dev-1".to_string(),
            hostname: "laptop".to_string(),
            created_at: 1,
        })
        .unwrap();
    harness.engine.client = harness.engine.store.load_client().unwrap();

    harness.engine.beat_now();
    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
    assert_eq!(
        harness.engine.store.load_credential().unwrap().unwrap().id,
        "cred-1"
    );
    assert!(matches!(harness.next_core(), Recorded::Start(_)));

    // 第二轮：服务端回 204。状态与规格都不变，不重写、不重启。
    harness.engine.beat_now();
    harness.expect_no_core();
    assert_eq!(harness.engine.snapshot().status, "degraded");
    assert!(harness.engine.last_hb > 0);
}

// ------------------------------------------------------------ 更新

fn digest(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

fn release_signing_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
}

fn release_key_hex() -> String {
    hex::encode(release_signing_key().verifying_key().to_bytes())
}

fn sign_release(slot: &str, version: &str, sha256: &str) -> String {
    use ed25519_dalek::Signer;
    let message =
        remote::signing::signing_string(slot, mesh_install::platform::id(), version, sha256);
    hex::encode(release_signing_key().sign(message.as_bytes()).to_bytes())
}

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
    }
}

/// 一条已经到手、可安装的 offer：内容齐、工件已校验暂存。
fn staged_offer(harness: &Harness, version: &str, sha: &str) -> (UpdateState, PathBuf) {
    let part = harness.dir.join("update.tar.zst.part");
    std::fs::write(&part, b"package-bytes").unwrap();
    let state = UpdateState {
        offer: Some(Offer {
            version: version.to_string(),
            url: format!("http://127.0.0.1:1/{version}.tar.zst"),
            notes: "旧说明".to_string(),
            sha256: sha.to_string(),
            auto: false,
            auto_tried: false,
        }),
        task: None,
        artifact: Artifact::Staged(part.clone()),
        attempts: 0,
        intent: None,
    };
    (state, part)
}

/// 让当前 offer 停在「已经连续失败 `attempts` 次、下一次重试在 `retry_in` 秒之后」。
fn fail_update(harness: &mut Harness, attempts: u32, retry_in: i64) {
    harness.engine.update.attempts = attempts;
    harness.engine.update.artifact = Artifact::Failed {
        error: "下载中断".to_string(),
        retry_at: if retry_in >= 0 {
            Instant::now() + Duration::from_secs(retry_in as u64)
        } else {
            Instant::now() - Duration::from_secs((-retry_in) as u64)
        },
    };
}

#[test]
fn an_invalid_version_is_ignored_before_downloading() {
    let mut harness = harness("update-invalid");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("../../evil")));
    assert_eq!(harness.engine.update.phase(), crate::update::Phase::Idle);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert_eq!(
        harness.engine.update.offer.as_ref().unwrap().version,
        "2026.11.26-2"
    );
    assert_ne!(harness.engine.update.phase(), crate::update::Phase::Idle);
}

#[test]
fn a_failure_is_kept_until_the_backoff_expires() {
    let mut harness = harness("update-failure");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    // 有意图才在两种平台上都走「准备工件」这条路：Unix 无意图时不预取。
    harness.engine.update.intent = Some(Intent::Manual);
    harness.engine.update.task = None;
    fail_update(&mut harness, 1, 30);
    // 退避还没到：心跳既不能把失败态抹掉，也不能抢着重下。
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    harness.engine.advance_update();
    assert_eq!(harness.engine.update.phase(), crate::update::Phase::Failed);
    assert_eq!(harness.engine.update.error(), "下载中断");
    assert!(harness.engine.update.task.is_none());
}

#[test]
fn a_failed_download_is_retried_once_the_backoff_expires() {
    let mut harness = harness("update-retry");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    harness.engine.update.task = None;
    fail_update(&mut harness, 1, -1);
    // 走「有意图 → try_apply → 工件失败 → prepare 重下」这条两种平台都有的路径：
    // 没有意图时 Unix 不预取（见 prepare 的平台判据），只靠心跳推不动重下。
    harness.engine.update.intent = Some(Intent::Manual);
    harness.engine.advance_update();
    assert_eq!(
        harness.engine.update.phase(),
        crate::update::Phase::Downloading
    );
    assert!(
        harness.engine.update.error().is_empty(),
        "重试时错误行要清掉"
    );
}

/// Windows 收到 offer 就自己预取，不需要安装意图；Unix 等用户点安装才下载。
/// 这条差异只能由平台专属的断言表达。
#[cfg(windows)]
#[test]
fn a_windows_engine_prefetches_without_an_intent() {
    let mut harness = harness("update-prefetch");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert!(harness.engine.update.intent.is_none());
    harness.engine.advance_update();
    assert_eq!(
        harness.engine.update.phase(),
        crate::update::Phase::Downloading
    );
}

#[test]
fn downloading_gives_up_after_the_attempt_limit() {
    let mut harness = harness("update-give-up");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    harness.engine.update.task = None;
    fail_update(&mut harness, MAX_DOWNLOAD_ATTEMPTS, -1);
    // 意图摆上，判的才是次数上限本身，而不是「Unix 无意图不预取」。
    harness.engine.update.intent = Some(Intent::Manual);
    harness.engine.advance_update();
    assert_eq!(
        harness.engine.update.phase(),
        crate::update::Phase::Failed,
        "次数用完就该停下来等人处理，不能一直刷发布文件服务"
    );
    assert!(harness.engine.update.task.is_none());
}

#[test]
fn the_engine_never_clears_the_pending_package() {
    let mut harness = harness("pending-package");
    let pending = harness.dir.join("update.tar.zst");
    std::fs::write(&pending, b"pending").unwrap();
    let temp = harness.dir.join("update");
    std::fs::create_dir_all(&temp).unwrap();
    std::fs::write(temp.join("launcher.new"), b"half-download").unwrap();
    harness.engine.clean_update_dir();
    assert!(pending.is_file(), "启动清理删掉了待安装的包");
    assert!(!temp.join("launcher.new").exists());
    let (state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    harness.engine.update = state;
    harness.engine.clear_stale_update();
    assert!(pending.is_file(), "撤回不能替安装事务清包");
}

#[test]
fn a_version_with_two_install_failures_is_skipped() {
    let mut harness = harness("update-exhausted");
    api(&mut harness);
    let version = "2026.11.26-2";
    let mut failures = mesh_install::failures::Failures::load(&harness.dir);
    failures.record(version);
    failures.record(version);
    harness.engine.offer_update(Some(&update_info(version)));
    assert!(
        harness.engine.update.offer.is_none(),
        "连续两次装不上的版本不再自动接"
    );
}

#[test]
fn a_new_version_resets_the_failure() {
    let mut harness = harness("update-new-version");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    fail_update(&mut harness, 3, 60);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-3")));
    assert_eq!(
        harness.engine.update.offer.as_ref().unwrap().version,
        "2026.11.26-3"
    );
    assert!(
        matches!(harness.engine.update.artifact, Artifact::None),
        "换版本要清掉上一份的失败"
    );
    assert_eq!(
        harness.engine.update.attempts, 0,
        "换版本要清掉上一份的失败次数"
    );
}

/// 服务端每轮心跳都带完整的一份，「这份我已经验过」由版本与摘要判：同一工件不再
/// 下载，但地址、说明与授权都按最新下发值刷新——发布方搬迁了地址，客户端还按旧
/// 地址重试就永远下不到。已暂存件与在途任务不受影响。
#[test]
fn the_same_artifact_refreshes_the_offer_without_dropping_the_local_package() {
    let mut harness = harness("same-offer");
    api(&mut harness);
    let (mut state, part) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    // 这一份已经自动试过一次：同工件刷新不该让它再自动装一遍。
    state.offer.as_mut().unwrap().auto_tried = true;
    harness.engine.update = state;
    harness.engine.update.attempts = 2;
    let mut info = update_info("2026.11.26-2");
    info.url = "http://10.0.0.9/moved.tar.zst".to_string();
    info.notes = "新说明".to_string();
    info.auto_update = true;
    harness.engine.offer_update(Some(&info));
    harness.engine.advance_update();

    let update = &harness.engine.update;
    assert_eq!(update.phase(), crate::update::Phase::Ready);
    let offer = update.offer.as_ref().unwrap();
    assert_eq!(offer.version, "2026.11.26-2");
    assert_eq!(offer.url, "http://10.0.0.9/moved.tar.zst", "地址要跟着换");
    assert_eq!(offer.notes, "新说明");
    assert!(offer.auto);
    assert!(
        matches!(&update.artifact, Artifact::Staged(staged) if staged == &part),
        "已暂存件不动"
    );
    assert!(part.is_file(), "已校验的字节还在");
    assert!(update.task.is_none(), "同一份不再下载");
    assert_eq!(update.attempts, 2, "同一份的失败次数不该被心跳清零");

    // 在途任务同样不受影响：它按版本与摘要认这一份，与地址无关。
    let in_flight = DownloadTask {
        gen: 7,
        version: "2026.11.26-2".to_string(),
        sha256: digest(0xab),
    };
    harness.engine.update.task = Some(in_flight.clone());
    harness.engine.offer_update(Some(&info));
    assert_eq!(harness.engine.update.task.as_ref(), Some(&in_flight));
}

/// 连续下载失败：次数跨「起任务 → 失败 → 重试」累加，到上限就停下来等人处理。
#[test]
fn consecutive_download_failures_accumulate_until_the_limit() {
    let mut harness = harness("update-accumulate");
    // 不装 api：任务由测试直接投递完成消息，不起后台线程。
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    // Unix 无意图不预取：把意图摆上，两种平台才走同一条准备路径。
    harness.engine.update.intent = Some(Intent::Manual);
    for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
        let task = DownloadTask {
            gen: attempt as u64,
            version: "2026.11.26-2".to_string(),
            sha256: digest(0xab),
        };
        harness.engine.update.task = Some(task.clone());
        harness
            .engine
            .on_download(task, Err("连接被拒绝".to_string()));
        assert_eq!(
            harness.engine.update.attempts, attempt,
            "失败次数要累加，不能每次回到 1"
        );
        assert_eq!(
            harness.engine.update.phase(),
            crate::update::Phase::Failed,
            "第 {attempt} 次失败之后停在失败态"
        );
    }

    // 退避已过（那是另一条轴）：次数用完就不再起任务。
    if let Artifact::Failed { retry_at, .. } = &mut harness.engine.update.artifact {
        *retry_at = Instant::now() - Duration::from_secs(1);
    }
    api(&mut harness);
    harness.engine.advance_update();
    assert!(
        harness.engine.update.task.is_none(),
        "次数用完就该停下来等人处理"
    );
    assert_eq!(harness.engine.update.phase(), crate::update::Phase::Failed);
}

#[test]
fn clearing_the_offer_withdraws_it() {
    let mut harness = harness("known-clear");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert!(harness.engine.update.offer.is_some());
    harness.engine.clear_stale_update();
    assert!(harness.engine.update.offer.is_none());
}

#[test]
fn an_unsigned_offer_is_ignored() {
    let mut harness = harness("update-unsigned");
    api(&mut harness);
    let mut info = update_info("2026.11.26-2");
    info.signature.clear();
    harness.engine.offer_update(Some(&info));
    assert!(harness.engine.update.offer.is_none());
}

#[test]
fn a_signature_for_another_slot_is_ignored() {
    let mut harness = harness("update-wrong-slot");
    api(&mut harness);
    let mut info = update_info("2026.11.26-2");
    info.signature = sign_release(remote::signing::SLOT_LAUNCHER, &info.version, &info.sha256);
    harness.engine.offer_update(Some(&info));
    assert!(harness.engine.update.offer.is_none());
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
    assert!(harness.engine.launcher.tried.is_none());
    assert!(harness.engine.launcher.task.is_none());
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
    assert!(harness.engine.launcher.task.is_some(), "任务本身是单飞判据");
}

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
    harness.engine.launcher.installed_version = Some("2026.11.26-5".to_string());
    harness.engine.apply_launcher(&api, Some(&info));
    assert!(
        !harness.engine.config.root.join("update").exists(),
        "更旧的启动器不该走到建暂存目录那一步"
    );
    assert_eq!(harness.engine.launcher.tried.as_deref(), Some(sum.as_str()));
    assert!(harness.engine.launcher.task.is_none());
}

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
    harness.engine.launcher.installed_version = Some("2026.11.26-5".to_string());
    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(harness.engine.launcher.tried.as_deref(), Some(sum.as_str()));
    assert!(harness.engine.launcher.task.is_some());
}

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
    assert_eq!(facts.installed_version.as_deref(), Some("2026.11.26-7"));
    assert_eq!(
        facts.installed_sum,
        mesh_install::artifact::digest_file(&launcher).unwrap(),
        "摘要描述的就是安装根里那份文件"
    );
}

/// 安装根被占着：已校验的暂存件留着，等下一轮再提交——绝不重下。
#[test]
fn a_verified_artifact_waits_for_the_install_root_without_downloading() {
    let mut harness = harness("download-lock");
    api(&mut harness);
    let sha = digest(0xab);
    let (mut state, part) = staged_offer(&harness, "2026.11.26-2", &sha);
    state.intent = Some(Intent::Manual);
    harness.engine.update = state;
    // 别人（启动器）正握着安装根。
    let owner =
        mesh_install::lock::acquire(&mesh_install::lock::Lock::owner_path(&harness.dir)).unwrap();

    harness.engine.advance_update();

    // 拿不到写权：包留在暂存位置，固定路径没出现，也没有起新下载。
    assert!(matches!(
        harness.engine.update.artifact,
        Artifact::Staged(_)
    ));
    assert!(harness.engine.update.task.is_none());
    assert!(part.is_file(), "包不能丢：下一轮还要提交它");
    assert!(!harness.dir.join("update.tar.zst").exists());
    assert!(harness.engine.update.error().is_empty());
    assert_eq!(
        harness.engine.update.intent,
        Some(Intent::Manual),
        "延期不是失败，意图留着"
    );
    drop(owner);
}

/// 提交是「请安装」信号的唯一写入口：没有安装意图就只暂存，不改固定路径。
#[test]
fn a_downloaded_package_is_not_committed_without_an_intent() {
    let mut harness = harness("download-staged");
    api(&mut harness);
    let sha = digest(0xab);
    let (mut state, _) = staged_offer(&harness, "2026.11.26-2", &sha);
    state.artifact = Artifact::None;
    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-2".to_string(),
        sha256: sha.clone(),
    };
    state.task = Some(task.clone());
    harness.engine.next_task = 1;
    harness.engine.update = state;
    let part = harness.dir.join("update.tar.zst.part");
    std::fs::write(&part, b"package-bytes").unwrap();

    harness.engine.on_download(task, Ok(part.clone()));

    assert!(matches!(
        harness.engine.update.artifact,
        Artifact::Staged(_)
    ));
    assert!(part.is_file());
    assert!(
        !harness.dir.join("update.tar.zst").exists(),
        "没有授权就不该出现「请安装」信号"
    );
}

/// 交接没被接收：已就绪的包与授权都留着，失败可见；下一轮只重试交接——不重下，
/// 也不消耗自动尝试标记。
#[cfg(windows)]
#[test]
fn a_failed_handoff_keeps_the_package_and_the_authorization() {
    let mut harness = harness("handoff-fails");
    let sha = digest(0xab);
    let (mut state, part) = staged_offer(&harness, "2026.11.26-2", &sha);
    state.intent = Some(Intent::Auto);
    let offer = state.offer.as_mut().unwrap();
    offer.auto = true;
    offer.auto_tried = true;
    harness.engine.update = state;

    // 安装根里没有启动器：安装事务无法被接收。
    harness.engine.advance_update();

    let pending = harness.dir.join("update.tar.zst");
    {
        let update = &harness.engine.update;
        assert!(
            matches!(update.artifact, Artifact::Committed { .. }),
            "工件已就绪是另一件事"
        );
        assert!(!part.exists(), "包已提交到固定路径");
        assert!(pending.is_file(), "已就绪的包要留着，不能丢");
        assert_eq!(update.intent, Some(Intent::Auto), "授权不因交接失败而丢");
        assert!(
            !update.offer.as_ref().unwrap().auto_tried,
            "自动尝试标记不被消耗"
        );
        assert!(
            update.error().contains("启动器"),
            "失败要可见：{}",
            update.error()
        );
        assert_eq!(update.phase(), crate::update::Phase::Failed);
    }

    // 下一轮心跳只重试交接：不重下，包还在。
    harness.engine.advance_update();
    assert!(matches!(
        harness.engine.update.artifact,
        Artifact::Committed { .. }
    ));
    assert!(harness.engine.update.task.is_none(), "不重新下载");
    assert!(pending.is_file());
}

/// 启动器替换还没结束：客户端包交接延期——这一轮要拉起的正是那个正在被换掉的文件。
#[test]
fn a_client_handoff_waits_for_the_launcher_replacement_in_flight() {
    let mut harness = harness("handoff-deferred");
    let sha = digest(0xab);
    let (mut state, part) = staged_offer(&harness, "2026.11.26-2", &sha);
    state.intent = Some(Intent::Manual);
    harness.engine.update = state;
    harness.engine.launcher.task = Some(DownloadTask {
        gen: 9,
        version: "2026.11.26-3".to_string(),
        sha256: digest(0xcd),
    });

    harness.engine.advance_update();

    assert!(matches!(
        &harness.engine.update.artifact,
        Artifact::Staged(staged) if staged == &part
    ));
    assert!(part.is_file(), "延期不是失败：暂存件留着");
    assert!(
        !harness.dir.join("update.tar.zst").exists(),
        "交接没被接收就不该有安装信号"
    );
    assert_eq!(harness.engine.update.intent, Some(Intent::Manual));
}

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
    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-9".to_string(),
        sha256: sum.clone(),
    };
    harness.engine.launcher.task = Some(task.clone());
    harness.engine.launcher.tried = Some(sum.clone());

    harness
        .engine
        .on_launcher_result(task, crate::update::LauncherResult::Deferred);
    assert!(harness.engine.launcher.tried.is_none(), "没拿到锁不算结论");

    harness.engine.apply_launcher(&api, Some(&info));
    assert_eq!(harness.engine.launcher.tried.as_deref(), Some(sum.as_str()));
    assert!(harness.engine.launcher.task.is_some());
}

/// 已校验的启动器工件不因写权暂不可用而重下：暂存区里那份摘要对得上就直接提交。
#[test]
fn a_verified_launcher_artifact_is_reused_without_downloading() {
    let mut harness = harness("launcher-reuse");
    let api = api(&mut harness);
    let payload = b"launcher-bytes".to_vec();
    let sum = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&payload))
    };
    let dir = harness.engine.config.root.join("update");
    std::fs::create_dir_all(&dir).unwrap();
    let temp = dir.join("launcher.new");
    std::fs::write(&temp, &payload).unwrap();
    // 地址不可达：只有“复用暂存件、不下载”这一条路能换成功。
    let info = remote::LauncherInfo {
        version: "2026.11.26-9".to_string(),
        url: "http://127.0.0.1:1/mesh.exe".to_string(),
        sha256: sum.clone(),
        signature: sign_release(remote::signing::SLOT_LAUNCHER, "2026.11.26-9", &sum),
    };
    harness.engine.apply_launcher(&api, Some(&info));
    let dst = harness
        .engine
        .config
        .root
        .join(mesh_install::platform::launcher_name());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if dst.is_file() {
            assert_eq!(std::fs::read(&dst).unwrap(), payload);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("已校验的暂存件没有被复用");
}

#[test]
fn a_replaced_launcher_updates_the_facts_and_keeps_the_version_monotonic() {
    let mut harness = harness("launcher-facts");
    let api = api(&mut harness);
    harness.engine.launcher.installed_sum = digest(0xdd);
    harness.engine.launcher.installed_version = Some("2026.11.26-5".to_string());

    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-7".to_string(),
        sha256: digest(0xd1),
    };
    harness.engine.launcher.task = Some(task.clone());
    harness
        .engine
        .on_launcher_result(task, crate::update::LauncherResult::Replaced);
    assert_eq!(
        harness.engine.launcher.installed_version.as_deref(),
        Some("2026.11.26-7")
    );
    assert_eq!(harness.engine.launcher.installed_sum, digest(0xd1));

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

#[test]
fn a_failed_launcher_replacement_keeps_the_facts() {
    let mut harness = harness("launcher-failed");
    harness.engine.launcher.installed_version = Some("2026.11.26-5".to_string());
    harness.engine.launcher.installed_sum = digest(0xdd);
    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-9".to_string(),
        sha256: digest(0xd4),
    };
    harness.engine.launcher.task = Some(task.clone());
    harness
        .engine
        .on_launcher_result(task, crate::update::LauncherResult::Failed);
    assert_eq!(
        harness.engine.launcher.installed_version.as_deref(),
        Some("2026.11.26-5")
    );
    assert_eq!(harness.engine.launcher.installed_sum, digest(0xdd));
}

/// 同一份 offer 重来一遍不会掐死下载重试：判重只更新策略，重试仍归状态机。
#[test]
fn a_failed_download_is_retried_when_the_same_offer_comes_back() {
    let mut harness = harness("same-retry");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    fail_update(&mut harness, 1, -1);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    // 判重不动意图：把意图摆上，才走两种平台都有的「有意图 → prepare 重下」路径。
    harness.engine.update.intent = Some(Intent::Manual);
    harness.engine.advance_update();
    assert_eq!(
        harness.engine.update.phase(),
        crate::update::Phase::Downloading
    );
}

#[test]
fn a_republished_version_replaces_the_offer() {
    let mut harness = harness("known-republish");
    api(&mut harness);
    let (state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xaa));
    harness.engine.update = state;
    let mut info = update_info("2026.11.26-2");
    info.sha256 = digest(0xbb);
    info.url = "http://127.0.0.1:1/new.tar.zst".to_string();
    resign(&mut info);
    harness.engine.offer_update(Some(&info));
    assert_eq!(
        harness.engine.update.offer.as_ref().unwrap().sha256,
        digest(0xbb)
    );
    assert_eq!(
        harness.engine.update.offer.as_ref().unwrap().url,
        "http://127.0.0.1:1/new.tar.zst"
    );
    let before = harness.engine.update.offer.as_ref().unwrap().sha256.clone();
    harness.engine.offer_update(Some(&info));
    assert_eq!(harness.engine.update.offer.as_ref().unwrap().sha256, before);
}

#[test]
fn the_switch_is_taken_as_given_with_a_local_veto() {
    let mut harness = harness("known-switch");
    api(&mut harness);
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    assert!(!harness.engine.update.offer.as_ref().unwrap().auto);
    let mut info = update_info("2026.11.26-2");
    info.auto_update = true;
    harness.engine.offer_update(Some(&info));
    assert!(harness.engine.update.offer.as_ref().unwrap().auto);
    harness.engine.config.auto_update_disabled = true;
    harness.engine.offer_update(Some(&info));
    assert!(!harness.engine.update.offer.as_ref().unwrap().auto);
}

/// 下载中撤回：offer 与意图清掉，暂存件删掉，旧任务的完成消息作废；
/// 固定路径上的包不归客户端清。
#[test]
fn withdrawing_an_offer_during_download_discards_the_task_result() {
    let mut harness = harness("update-withdraw");
    let sha = digest(0xab);
    let task = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-2".to_string(),
        sha256: sha.clone(),
    };
    let (mut state, part) = staged_offer(&harness, "2026.11.26-2", &sha);
    state.task = Some(task.clone());
    state.intent = Some(Intent::Auto);
    harness.engine.update = state;
    let pending = harness.dir.join("update.tar.zst");
    std::fs::write(&pending, b"pending-package").unwrap();

    harness.engine.clear_stale_update();
    assert!(harness.engine.update.offer.is_none());
    assert!(harness.engine.update.intent.is_none());
    assert!(!part.exists(), "未授权的暂存件要清掉");
    assert!(
        harness.engine.update.task.is_some(),
        "线程还在跑：不许放第二个进来写同一个 .part"
    );

    let stale = harness.dir.join("update.tar.zst.part");
    std::fs::write(&stale, b"stale-package").unwrap();
    harness.engine.on_download(task, Ok(stale.clone()));
    assert!(
        harness.engine.update.offer.is_none(),
        "旧结果不能把 offer 复活"
    );
    assert!(harness.engine.update.task.is_none());
    assert!(!stale.exists(), "作废的下载只该清掉自己的临时文件");
    assert_eq!(
        std::fs::read(&pending).unwrap(),
        b"pending-package",
        "固定路径上的包不归客户端清"
    );
}

#[test]
fn a_republished_version_invalidates_the_download_in_flight() {
    let mut harness = harness("update-republish-download");
    api(&mut harness);
    harness.engine.next_task = 1;
    let old_sha = digest(0xab);
    let old = crate::update::DownloadTask {
        gen: 1,
        version: "2026.11.26-2".to_string(),
        sha256: old_sha.clone(),
    };
    let (mut state, _) = staged_offer(&harness, "2026.11.26-2", &old_sha);
    state.artifact = Artifact::None;
    state.task = Some(old.clone());
    harness.engine.update = state;

    let mut info = update_info("2026.11.26-2");
    info.sha256 = digest(0xbb);
    info.url = "http://127.0.0.1:1/new.tar.zst".to_string();
    resign(&mut info);
    harness.engine.offer_update(Some(&info));

    assert_eq!(
        harness.engine.update.offer.as_ref().unwrap().sha256,
        digest(0xbb),
        "换成新那一份"
    );
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
    assert_eq!(
        harness.engine.update.offer.as_ref().unwrap().sha256,
        digest(0xbb)
    );
    match harness.engine.update.task.as_ref() {
        Some(task) => {
            assert_eq!(task.sha256, digest(0xbb));
            assert_ne!(task.gen, old.gen);
        }
        None => assert_eq!(harness.engine.update.phase(), crate::update::Phase::Ready),
    }
}

#[test]
fn auto_apply_is_a_property_of_the_offer() {
    let mut harness = harness("auto-offer");
    api(&mut harness);
    let (state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    harness.engine.update = state;
    harness.engine.auto_apply();
    assert!(harness.engine.update.intent.is_none(), "没有授权不该记意图");

    let offer = harness.engine.update.offer.as_mut().unwrap();
    offer.auto = true;
    harness.engine.auto_apply();
    assert_eq!(harness.engine.update.intent, Some(Intent::Auto));
    assert!(harness.engine.update.offer.as_ref().unwrap().auto_tried);
    // 已经把意图交给交接路径之后，同一份 offer 不再重复记。
    harness.engine.update.intent = None;
    harness.engine.auto_apply();
    assert!(
        harness.engine.update.intent.is_none(),
        "同一份 offer 只试一次"
    );

    let (state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xbb));
    harness.engine.update = state;
    harness.engine.update.offer.as_mut().unwrap().auto = true;
    harness.engine.auto_apply();
    assert_eq!(
        harness.engine.update.intent,
        Some(Intent::Auto),
        "换了内容就是另一份 offer"
    );
}

/// 策略在等待期间被关掉：自动意图在交接前作废，手动意图不受影响。
#[test]
fn an_auto_intent_is_dropped_when_the_policy_turns_off() {
    let mut harness = harness("auto-revoked");
    api(&mut harness);
    let (mut state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    state.offer.as_mut().unwrap().auto = true;
    state.intent = Some(Intent::Auto);
    harness.engine.update = state;
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    harness.engine.advance_update();
    assert!(harness.engine.update.intent.is_none());
    assert!(matches!(
        harness.engine.update.artifact,
        Artifact::Staged(_)
    ));
}

#[test]
fn a_manual_intent_survives_a_policy_change() {
    let mut harness = harness("manual-kept");
    api(&mut harness);
    let (mut state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    state.intent = Some(Intent::Manual);
    harness.engine.update = state;
    // 手动安装与自动策略无关：关掉开关也不该取消。
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));
    let owner =
        mesh_install::lock::acquire(&mesh_install::lock::Lock::owner_path(&harness.dir)).unwrap();
    harness.engine.advance_update();
    assert_eq!(harness.engine.update.intent, Some(Intent::Manual));
    drop(owner);
}

/// 新的 offer 让旧的授权与暂存件一起作废。
#[test]
fn a_new_offer_drops_the_intent_and_the_staged_artifact() {
    let mut harness = harness("new-offer-clears");
    api(&mut harness);
    let (mut state, part) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    state.intent = Some(Intent::Manual);
    harness.engine.update = state;
    let mut info = update_info("2026.11.26-3");
    info.sha256 = digest(0xbc);
    resign(&mut info);
    harness.engine.offer_update(Some(&info));
    assert!(harness.engine.update.intent.is_none());
    assert!(!part.exists(), "旧暂存件不再被授权");
    assert!(matches!(harness.engine.update.artifact, Artifact::None));
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
    fail_update(&mut harness, 1, 30);
    let result = harness.engine.apply_update();
    assert!(result.is_ok(), "点重试要重新下一份，而不是报“包不在本地”");
    assert_eq!(
        harness.engine.update.phase(),
        crate::update::Phase::Downloading
    );
    match &harness.engine.update.artifact {
        Artifact::None => {}
        _ => panic!("用户主动重试要把失败退避还回来"),
    }
}

// ------------------------------------------------------------ 设置与注册

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
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.device_id, "d-server");
    assert_eq!(client.server_url, stub.url);
    assert!(client.created_at > 0);
    assert_eq!(harness.engine.snapshot().status, "connecting");
    assert!(harness.engine.store.read_config().unwrap().is_none());
    assert!(harness.engine.store.load_credential().unwrap().is_none());
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[0].0, "POST /api/v1/register HTTP/1.1");
    let body = String::from_utf8_lossy(&requests[0].1);
    assert!(body.contains("\"token\":\"token-1\""), "body = {body}");
    assert!(body.contains("\"hostname\":\"laptop\""), "body = {body}");
    assert!(body.contains("\"pubkey\":"), "body = {body}");
}

#[test]
fn a_registered_identity_is_activated_by_a_plain_heartbeat() {
    let reply = r#"{"deviceId":"d-server","configToml":"","configHash":"h1"}"#;
    let toml = "hostname = \"laptop\"\nipv4 = \"10.0.0.1/24\"\n";
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
    harness.engine.reload_identity();
    harness.engine.beat_now();

    assert_eq!(
        harness.engine.store.read_config().unwrap().as_deref(),
        Some(toml)
    );
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "第二次是心跳，不是重放注册");
    assert_eq!(requests[1].0, "POST /api/v1/heartbeat HTTP/1.1");
}

/// 换代次：先把旧一代的配置与凭据从可读位置删掉，再写新身份。
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
    harness
        .engine
        .offer_update(Some(&update_info("2026.11.26-2")));

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
        harness.engine.update.offer.is_none(),
        "上一代控制面的更新 offer 必须作废"
    );
}

/// 清理失败就是这次保存失败：不留下「新身份配旧配置」的一代。
#[test]
fn a_failed_generation_cleanup_keeps_the_old_identity() {
    let mut harness = harness("switch-cleanup-fails");
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
    // config.toml 是目录：remove_file 会失败（Windows 与 Unix 都如此）。
    std::fs::create_dir_all(harness.engine.store.config_path()).unwrap();

    let err = harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: "http://127.0.0.1:8".to_string(),
            token: String::new(),
            hostname: String::new(),
        })
        .unwrap_err();
    assert!(err.contains("config.toml"), "err = {err}");
    let client = harness.engine.store.load_client().unwrap().unwrap();
    assert_eq!(client.server_url, "http://127.0.0.1:9", "旧身份不能被换掉");
}

/// 换绑的中途失败：旧数据面先停下了，清理却没做成。这是一次明确失败，不是半换绑：
/// 旧一代的身份与配置原样留着，下一轮心跳仍按同一份身份收敛。
#[test]
fn a_failed_generation_cleanup_stops_the_old_data_plane_first() {
    let mut harness = harness("switch-cleanup-fails-core");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-old");
    harness.engine.core = Some(applied_core(&harness));
    // config.toml 换成目录：remove_file 会失败（Windows 与 Unix 都如此）。
    std::fs::remove_file(harness.engine.store.config_path()).unwrap();
    std::fs::create_dir_all(harness.engine.store.config_path()).unwrap();

    let err = harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: "http://127.0.0.1:8".to_string(),
            token: String::new(),
            hostname: String::new(),
        })
        .unwrap_err();
    assert!(err.contains("config.toml"), "err = {err}");
    assert!(
        matches!(harness.next_core(), Recorded::Stop),
        "清理之前先停旧数据面"
    );
    assert!(harness.engine.core.is_none());
    assert_eq!(
        harness
            .engine
            .store
            .load_client()
            .unwrap()
            .unwrap()
            .server_url,
        "http://127.0.0.1:1",
        "旧身份不能被换掉"
    );
}

/// 换代次的中断点：旧一代的配置与凭据已经删掉、新身份还没写入。
///
/// 这时本地只该得到「旧身份 + 没有配置」这一个确定状态：不拿旧文件起 core，
/// 已经在跑的那一代也必须停下——删文件只决定下一次读哪份配置，不决定那个进程会退。
#[test]
fn an_interrupted_generation_switch_stops_the_running_core() {
    let mut harness = harness("switch-interrupted");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.core = Some(applied_core(&harness));

    // 模拟「旧文件已删、新 client.json 还没写」这一刻。
    harness.engine.store.remove_config().unwrap();
    harness.engine.store.remove_credential().unwrap();

    assert!(
        harness.engine.desired_spec().unwrap().is_none(),
        "没有配置就没有可应用的规格"
    );
    harness.engine.ensure_core().unwrap();
    assert!(
        matches!(harness.next_core(), Recorded::Stop),
        "没有期望规格就不该有运行实例"
    );
    assert!(harness.engine.core.is_none());
    harness.expect_no_core();
}

/// 换绑：控制面地址变了就是另一代。旧数据面必须先停下，新控制面还没下发配置时
/// 什么都不启动，旧一代的配置、凭据与认证失败计数一起作废。
#[test]
fn rebinding_to_another_control_plane_stops_the_running_core() {
    let mut harness = harness("rebind-stop");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-old");
    harness.engine.core = Some(applied_core(&harness));
    harness.engine.auth_fails = 2;

    harness
        .engine
        .save_settings(crate::SettingsInput {
            server_url: "http://127.0.0.1:2".to_string(),
            token: String::new(),
            hostname: String::new(),
        })
        .unwrap();

    assert!(
        matches!(harness.next_core(), Recorded::Stop),
        "换绑必须停掉旧数据面"
    );
    // 新控制面还没下发配置：不能用旧配置的规格再起一个进程。
    harness.expect_no_core();
    assert!(harness.engine.core.is_none());
    assert!(
        harness.engine.store.read_config().unwrap().is_none(),
        "上一代控制面的配置必须作废"
    );
    assert!(
        harness.engine.store.load_credential().unwrap().is_none(),
        "上一代控制面的凭据必须作废"
    );
    assert_eq!(
        harness
            .engine
            .store
            .load_client()
            .unwrap()
            .unwrap()
            .server_url,
        "http://127.0.0.1:2"
    );
    assert_eq!(harness.engine.auth_fails, 0, "认证失败计数属于上一代");
}

/// 旧数据面停不掉：失败必须可见，并且按重拉档位有界重试；停下之后实例才消失。
#[test]
fn a_core_that_cannot_be_stopped_stays_visible_and_is_retried() {
    let mut harness = harness("stop-fails");
    let config = saved_client(&harness);
    harness.engine.client = Some(config);
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.core = Some(applied_core(&harness));
    harness.engine.store.remove_config().unwrap();
    harness.engine.store.remove_credential().unwrap();
    harness.set_stop_ok(false);

    harness.engine.ensure_core_or_report();
    assert!(matches!(harness.next_core(), Recorded::Stop));
    assert!(harness.engine.core.is_some(), "停不掉就不能当它停了");
    assert!(harness.engine.core_running(), "core 还在跑，界面不能谎报");
    assert!(harness.engine.revive.is_some(), "停不掉要有界重试");
    assert_eq!(harness.published().status, "error", "失败要可见");

    // 到点重试：这次停得掉，实例消失，重试期限不再排。
    harness.set_stop_ok(true);
    harness.engine.revive = Some(crate::run::Revive {
        attempt: 1,
        due: Instant::now(),
    });
    harness.engine.on_deadline();
    assert!(matches!(harness.next_core(), Recorded::Stop));
    assert!(harness.engine.core.is_none());
    assert!(harness.engine.revive.is_none(), "收敛之后不该再排重试");
}

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

/// 设备名是 core 的启动参数：规格变了就换进程，没变就不动。
#[test]
fn changing_the_device_name_replaces_the_core() {
    let mut harness = harness("settings-hostname");
    let config = saved_client_named(&harness, "old");
    harness
        .engine
        .store
        .write_config("ipv4 = \"10.0.0.1/24\"\n")
        .unwrap();
    save_cred(&harness, "cred-1");
    harness.engine.client = Some(config);
    harness.engine.core = Some(applied_core(&harness));

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
    match harness.next_core() {
        Recorded::Start(spec) => assert_eq!(spec.hostname, "new"),
        _ => panic!("再起新的"),
    }

    // 名字没变：已经按这个身份跑着，不动。
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
    let part = crate::update::download_package(&api, &harness.dir, &stub.url, &want).unwrap();
    assert_eq!(part.file_name().unwrap(), "update.tar.zst.part");
    assert_eq!(std::fs::read(&part).unwrap(), payload);
    assert!(!harness.dir.join("update.tar.zst").exists());
    let path = crate::update::commit_package(&part, &harness.dir, &want).unwrap();
    assert_eq!(path.file_name().unwrap(), "update.tar.zst");
    assert_eq!(std::fs::read(&path).unwrap(), payload);
    assert!(!harness.dir.join("update.tar.zst.part").exists());
    // 摘要与包一起落盘：启动器靠它确认盘上的字节还是被授权的那一份。
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_to_string(harness.dir.join(mesh_install::layout::UPDATE_DIGEST))
            .unwrap()
            .trim(),
        want
    );
}

/// 撤回或换 offer 时，引擎自己放上去的包与摘要一起撤掉。
#[cfg(unix)]
#[test]
fn dropping_the_committed_package_drops_its_digest() {
    let mut harness = harness("update-withdraw");
    let (mut state, _) = staged_offer(&harness, "2026.11.26-2", &digest(0xab));
    let pending = harness.dir.join("update.tar.zst");
    let sum = harness.dir.join(mesh_install::layout::UPDATE_DIGEST);
    std::fs::write(&pending, b"package-bytes").unwrap();
    std::fs::write(&sum, digest(0xab)).unwrap();
    state.artifact = Artifact::Committed {
        error: String::new(),
    };
    harness.engine.update = state;

    harness.engine.clear_stale_update();

    assert!(!pending.exists(), "不再被授权的包不能留成安装信号");
    assert!(!sum.exists(), "摘要跟着包一起走");
}

/// 交接的命令行带的是客户端验过的那份整包摘要，启动器用它比对盘上的字节。
#[cfg(windows)]
#[test]
fn the_handoff_carries_the_package_digest() {
    let command =
        crate::update::handoff_command(Path::new("mesh.exe"), Path::new("update.tar.zst"), "ab12");
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let at = args
        .iter()
        .position(|arg| arg == "--sha256")
        .expect("交接必须带摘要");
    assert_eq!(args[at + 1], "ab12");
    assert!(args.iter().any(|arg| arg == "--file"));
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

#[test]
fn an_offer_without_a_full_digest_is_ignored() {
    let mut harness = harness("update-bad-digest");
    api(&mut harness);
    for bad in ["", "ab", &digest(0xab)[..63]] {
        let mut info = update_info("2026.11.26-9");
        info.sha256 = bad.to_string();
        harness.engine.offer_update(Some(&info));
        assert!(harness.engine.update.offer.is_none(), "digest = {bad:?}");
    }
}
