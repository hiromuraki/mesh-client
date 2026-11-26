//! `mesh-core` 的托管行为，用 `examples/fake_core.rs` 当替身。
//!
//! 覆盖：启动与就绪探测、输出按行捕获、崩溃可发现、停止时整个进程组一起没了、
//! 就绪探测超时明确失败（并且把那个进程停掉，不留孤儿）。

use mesh_core::{Cmd, Event, Expect, Identity, Options, StartSpec};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn fake_core() -> PathBuf {
    // 示例二进制由 `cargo test` 一并构建，落在 <target>/<profile>/examples/ 下。
    // 测试可执行文件本身可能被搬到别处运行（本机就是 target/debug/build/<crate>/.../out/），
    // 所以沿着它的祖先目录往上找，而不是假定它是 target/<profile>/deps/<name>。
    let name = if cfg!(windows) {
        "fake_core.exe"
    } else {
        "fake_core"
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        roots.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    roots.push(manifest.join("../../target/debug"));
    roots.push(manifest.join("../../target/release"));
    for root in &roots {
        let candidate = root.join("examples").join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "找不到假 core {name}：查过 {:?}（cargo test 会构建 examples；单独跑的话先 `cargo build --examples -p mesh-core`）",
        roots
    );
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mesh-core-it-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 给假 core 挑一个端口。
///
/// 不能简单地“bind :0 拿到端口就立刻释放”：从释放到假 core 真正 bind 之间有窗口，
/// 并行跑的其它用例（它们各自也在挑端口）可能正好拿走它，表现为随机失败。这里按
/// 进程号 + 递增序号挑一个固定端口，并当场验证它空着。
fn free_addr() -> SocketAddr {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = 20_000 + (std::process::id() % 200) as u32 * 100;
    for _ in 0..64 {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = (base + offset) % 60_000;
        if port < 1024 {
            continue;
        }
        let candidate = SocketAddr::from(([127, 0, 0, 1], port as u16));
        if TcpListener::bind(candidate).is_ok() {
            return candidate;
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap()
}

struct Harness {
    core: Sender<Cmd>,
    events: Receiver<Event>,
    lines: Arc<Mutex<Vec<String>>>,
}

fn harness(options: Options) -> Harness {
    let (event_tx, event_rx) = channel();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let core = mesh_core::spawn_with(
        move |event| {
            let _ = event_tx.send(event);
        },
        move |line| sink.lock().unwrap().push(line.to_string()),
        options,
    );
    Harness {
        core,
        events: event_rx,
        lines,
    }
}

impl Harness {
    fn start(&self, spec: StartSpec) {
        self.core.send(Cmd::Start(spec)).unwrap();
    }

    fn stop(&self) -> Result<(), String> {
        let (tx, rx) = channel();
        self.core.send(Cmd::Stop { reply: tx }).unwrap();
        rx.recv_timeout(Duration::from_secs(15))
            .expect("Stop 没有回复")
    }

    fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    fn wait_line(&self, wanted: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(line) = self
                .lines
                .lock()
                .unwrap()
                .iter()
                .find(|line| line.contains(wanted))
            {
                return Some(line.clone());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

/// 写一份假 core 的“配置”：`mode=` 等 key=value 行由示例自己解析。
fn write_config(dir: &Path, extra: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("config.toml");
    let mut text = String::from("ipv4 = \"10.126.126.100/24\"\n");
    for (key, value) in extra {
        text.push_str(&format!("{key}={value}\n"));
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn test_options() -> Options {
    Options {
        ready_timeout: Duration::from_secs(10),
        ready_interval: Duration::from_millis(50),
        probe_timeout: Duration::from_millis(500),
    }
}

fn spec(gen: u64, config: PathBuf, hostname: &str) -> StartSpec {
    StartSpec {
        gen,
        core_path: fake_core(),
        config_path: config,
        rpc_addr: free_addr(),
        hostname: hostname.to_string(),
        rpc_whitelist: None,
        identity: Identity::Credential {
            id: "cred-1".to_string(),
            secret: "secret-1".to_string(),
        },
        expect: Expect {
            hostname: hostname.to_string(),
            ipv4: String::new(),
        },
    }
}

#[test]
fn starts_probes_and_stops() {
    let dir = temp_dir("start");
    let config = write_config(&dir, &[]);
    let harness = harness(test_options());
    harness.start(spec(7, config.clone(), "laptop"));

    match harness.next_event(Duration::from_secs(15)) {
        Some(Event::Started { gen, pid, node }) => {
            assert_eq!(gen, 7);
            assert_ne!(pid, 0);
            assert_eq!(node.hostname, "laptop");
            assert_eq!(node.ipv4, "10.126.126.100/24");
            assert!(
                node.version.starts_with("2.7.1"),
                "version = {}",
                node.version
            );
        }
        other => panic!("应收到 Started，得到 {other:?}"),
    }

    // 参数与身份确实传给了子进程：配置路径在命令行，凭据在环境变量。
    let config_line = harness
        .wait_line("config=", Duration::from_secs(5))
        .expect("假 core 应打印收到的参数");
    assert!(config_line.contains("config.toml"), "line = {config_line}");
    assert!(
        harness
            .wait_line("credential_env=true", Duration::from_secs(5))
            .is_some(),
        "凭据应通过 ET_CREDENTIAL 传进去"
    );
    assert!(
        harness
            .wait_line("private_key_env=false", Duration::from_secs(5))
            .is_some(),
        "凭据节点不该把 secure key 塞进环境"
    );

    assert!(harness.stop().is_ok(), "停止应当成功");
    match harness.next_event(Duration::from_secs(5)) {
        Some(Event::Exited { gen, .. }) => assert_eq!(gen, 7),
        other => panic!("应收到 Exited，得到 {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn captures_output_lines_from_both_pipes() {
    let dir = temp_dir("lines");
    let config = write_config(&dir, &[("mode", "lines")]);
    let harness = harness(test_options());
    harness.start(spec(1, config, "laptop"));
    assert!(matches!(
        harness.next_event(Duration::from_secs(15)),
        Some(Event::Started { .. })
    ));

    assert!(
        harness
            .wait_line("line one", Duration::from_secs(5))
            .is_some(),
        "stdout 的行应当被捕获"
    );
    assert!(
        harness
            .wait_line("line two", Duration::from_secs(5))
            .is_some(),
        "stderr 的行应当被捕获"
    );
    // 行尾空白被去掉，空行不转交。
    assert!(
        harness
            .lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line == "trailing spaces"),
        "行尾空白应被去掉: {:?}",
        harness.lines.lock().unwrap()
    );
    assert!(!harness
        .lines
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.trim().is_empty()));
    harness.stop().ok();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_core_that_dies_before_ready_reports_start_failed() {
    let dir = temp_dir("crash");
    let config = write_config(&dir, &[("mode", "crash")]);
    let harness = harness(test_options());
    harness.start(spec(3, config, "laptop"));
    match harness.next_event(Duration::from_secs(15)) {
        Some(Event::StartFailed { gen, reason }) => {
            assert_eq!(gen, 3);
            assert!(reason.contains("已退出"), "reason = {reason}");
        }
        other => panic!("应收到 StartFailed，得到 {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_core_that_dies_after_ready_reports_exited() {
    let dir = temp_dir("exit");
    let config = write_config(&dir, &[("mode", "exit-after-start")]);
    let harness = harness(test_options());
    harness.start(spec(9, config, "laptop"));
    assert!(matches!(
        harness.next_event(Duration::from_secs(15)),
        Some(Event::Started { gen: 9, .. })
    ));
    match harness.next_event(Duration::from_secs(10)) {
        Some(Event::Exited { gen, err }) => {
            assert_eq!(gen, 9);
            let err = err.expect("非零退出应带上原因");
            assert!(err.contains('7'), "err = {err}");
        }
        other => panic!("应收到 Exited，得到 {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stopping_takes_the_whole_process_group_with_it() {
    let dir = temp_dir("group");
    let config = write_config(&dir, &[("mode", "child")]);
    let harness = harness(test_options());
    harness.start(spec(11, config, "laptop"));
    assert!(matches!(
        harness.next_event(Duration::from_secs(15)),
        Some(Event::Started { .. })
    ));
    let line = harness
        .wait_line("child ", Duration::from_secs(10))
        .expect("假 core 应报告子进程 pid");
    let child_pid: u32 = line
        .rsplit(' ')
        .next()
        .and_then(|pid| pid.trim().parse().ok())
        .expect("解析子进程 pid");
    assert!(process_alive(child_pid), "子进程 {} 应还在", child_pid);

    assert!(harness.stop().is_ok());
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(child_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !process_alive(child_pid),
        "停止 core 必须连它派生的子进程一起结束，否则会留下孤儿抢同一个 TUN"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_readiness_probe_times_out_and_cleans_up() {
    let dir = temp_dir("timeout");
    let marker = dir.join("pid");
    let config = write_config(
        &dir,
        &[("mode", "no-portal"), ("marker", marker.to_str().unwrap())],
    );
    let harness = harness(Options {
        ready_timeout: Duration::from_millis(700),
        ready_interval: Duration::from_millis(100),
        probe_timeout: Duration::from_millis(200),
    });
    harness.start(spec(5, config, "laptop"));
    match harness.next_event(Duration::from_secs(10)) {
        Some(Event::StartFailed { gen, reason }) => {
            assert_eq!(gen, 5);
            assert!(reason.contains("未确认 core 就绪"), "reason = {reason}");
        }
        other => panic!("应收到 StartFailed，得到 {other:?}"),
    }
    // 超时之后必须把那个 core 停掉，不能留一个没人管的进程。
    let pid: u32 = std::fs::read_to_string(&marker)
        .expect("假 core 应写下自己的 pid")
        .trim()
        .parse()
        .unwrap();
    assert!(!process_alive(pid), "超时后 core 进程 {pid} 应已被清理");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // 0 号信号只做存在性检查；pid 已被别人复用时可能误判，测试窗口里可以接受。
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
