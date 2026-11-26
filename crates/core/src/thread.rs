//! core 代管线程：命令队列、进程句柄、代次、就绪探测都在这里。
//!
//! 这个线程是 `Child`、pid 与 Job 句柄的唯一所有者。别的线程只能发 [`Cmd`]、
//! 收 [`Event`]，所以“两个线程同时动 core”这件事在类型上就不存在。

use crate::probe::{self, Outcome};
use crate::process;
use crate::pump::{self, LineSink};
use crate::{Cmd, Event, Identity, Options, StartSpec};
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 软停止之后等多久再强杀（Go 是 5s）。
const STOP_GRACE: Duration = Duration::from_secs(5);
/// 强杀之后再等多久（Go 是 3s）。
const KILL_GRACE: Duration = Duration::from_secs(3);
/// 命令循环的轮询间隔：没有命令时也每 100ms 看一眼子进程是不是自己退了。
const TICK: Duration = Duration::from_millis(100);

/// 事件回调只有 core 线程调用，所以是独占的 `Box`，不需要 `Sync`。
type EventSink = Box<dyn Fn(Event) + Send>;

pub(crate) fn spawn(
    events: impl Fn(Event) + Send + 'static,
    on_line: impl Fn(&str) + Send + 'static,
    options: Options,
) -> Sender<Cmd> {
    let (tx, rx) = channel();
    let events: EventSink = Box::new(events);
    let sink: LineSink = Arc::new(Mutex::new(Box::new(on_line)));
    thread::spawn(move || {
        CoreThread {
            cmds: rx,
            events,
            sink,
            options,
            running: None,
        }
        .run();
    });
    tx
}

struct Running {
    gen: u64,
    pid: u32,
    child: Child,
    /// 两个输出泵的句柄：退出时先等它们把管道读空再报 Exited（Go 的顺序）。
    pumps: Vec<thread::JoinHandle<()>>,
}

struct CoreThread {
    cmds: Receiver<Cmd>,
    events: EventSink,
    sink: LineSink,
    options: Options,
    running: Option<Running>,
}

impl CoreThread {
    fn run(mut self) {
        loop {
            self.reap();
            match self.cmds.recv_timeout(TICK) {
                Ok(Cmd::Start(spec)) => self.start(spec),
                Ok(Cmd::Stop { reply }) => {
                    let result = self.stop(true);
                    let _ = reply.send(result);
                }
                Err(RecvTimeoutError::Timeout) => {}
                // 命令端全部断开：进程要退了，别把 core 留成孤儿。
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = self.stop(false);
                    return;
                }
            }
        }
    }

    fn emit(&self, event: Event) {
        (self.events)(event);
    }

    /// 子进程自己退出时在这里收尸。
    fn reap(&mut self) {
        let exited = match self.running.as_mut() {
            None => return,
            Some(running) => match running.child.try_wait() {
                Ok(Some(status)) => Ok(Some(status)),
                Ok(None) => return,
                Err(err) => Err(err),
            },
        };
        let Some(running) = self.running.take() else {
            return;
        };
        for pump in running.pumps {
            let _ = pump.join();
        }
        match exited {
            Ok(status) => self.emit(Event::Exited {
                gen: running.gen,
                err: status.and_then(describe_status),
            }),
            Err(err) => self.emit(Event::Exited {
                gen: running.gen,
                err: Some(format!("等待 core 退出失败: {err}")),
            }),
        }
    }

    fn start(&mut self, spec: StartSpec) {
        if self.running.is_some() {
            if let Err(reason) = self.stop(false) {
                self.emit(Event::StartFailed {
                    gen: spec.gen,
                    reason,
                });
                return;
            }
        }
        let running = match self.launch(&spec) {
            Ok(running) => running,
            Err(reason) => {
                self.emit(Event::StartFailed {
                    gen: spec.gen,
                    reason,
                });
                return;
            }
        };
        let mut running = running;
        match probe::wait_ready(
            &mut running.child,
            spec.rpc_addr,
            &spec.expect,
            &self.options,
        ) {
            Outcome::Ready(node) => {
                let (gen, pid) = (running.gen, running.pid);
                self.running = Some(running);
                self.emit(Event::Started { gen, pid, node });
            }
            Outcome::Exited(status) => {
                let gen = running.gen;
                for pump in running.pumps {
                    let _ = pump.join();
                }
                self.emit(Event::StartFailed {
                    gen,
                    reason: format!("core 进程已退出（{}）", describe_status_opt(status)),
                });
            }
            Outcome::Timeout(reason) => {
                let gen = running.gen;
                self.running = Some(running);
                // 停不掉也要报：继续下去会起来第二个 core 抢同一个虚拟 IP。
                let stop_note = match self.stop(false) {
                    Ok(()) => String::new(),
                    Err(err) => format!("；停止 core 失败: {err}"),
                };
                self.emit(Event::StartFailed {
                    gen,
                    reason: format!(
                        "{}{}",
                        probe::timeout_message(self.options.ready_timeout, &reason),
                        stop_note
                    ),
                });
            }
        }
    }

    fn launch(&self, spec: &StartSpec) -> Result<Running, String> {
        let mut cmd = Command::new(&spec.core_path);
        cmd.arg("-c")
            .arg(&spec.config_path)
            .arg("--rpc-portal")
            .arg(spec.rpc_addr.to_string())
            .arg("--hostname")
            .arg(&spec.hostname);
        // core 默认按 info 刷屏：打洞重试约占七成行数、直连探测一成半、RPC 断开半成，
        // 另外每 15 秒还有一行 4.5 KB 的全局拓扑（字节数接近一半）。只把这四个 target
        // 压到 warn，其余仍是 info——「连上了」「配置变了」这类排障要看的不受影响。
        // RUST_LOG 是 EasyTier 原生的级别入口，最长前缀匹配，不用改内核参数解析。
        // 外部已经设了 RUST_LOG 就用外部的：现场排查打洞问题时可以直接放宽级别。
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,\
                 easytier_core::connectivity::hole_punch=warn,\
                 easytier_core::connectivity::direct=warn,\
                 easytier_core::rpc::bidirect=warn,\
                 easytier_core::peers::peer_center=warn"
                    .to_string()
            }),
        );
        // 钉住可达地址时同时收窄来源：这个端口能签发凭据，等价于 network_secret。
        if let Some(whitelist) = spec
            .rpc_whitelist
            .as_ref()
            .filter(|value| !value.is_empty())
        {
            cmd.arg("--rpc-portal-whitelist").arg(whitelist);
        }
        match &spec.identity {
            Identity::Formal { key } => {
                cmd.arg("--secure-mode");
                // 私钥走环境变量：命令行对同机其它用户可读，而它等价于节点身份。
                cmd.env("ET_LOCAL_PRIVATE_KEY", key);
            }
            Identity::Credential { secret, .. } => {
                cmd.env("ET_CREDENTIAL", secret);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process::configure(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|err| format!("启动 core 失败: {err}"))?;
        let pid = child.id();
        if let Err(err) = process::assign_to_job(pid) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("接管 core 进程生命周期失败: {err}"));
        }
        let mut pumps = Vec::with_capacity(2);
        for pipe in [
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let sink = Arc::clone(&self.sink);
            pumps.push(thread::spawn(move || pump::pump(pipe, &sink)));
        }
        Ok(Running {
            gen: spec.gen,
            pid,
            child,
            pumps,
        })
    }

    /// 停止 core。`report_exit` 为真时发 `Exited`（对应 Go 里关闭 Done）：
    /// 引擎按代号忽略自己主动停掉的那一代，所以这里照发不误。
    fn stop(&mut self, report_exit: bool) -> Result<(), String> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        let gen = running.gen;
        let pid = running.pid;
        let mut running = running;
        // 先礼：软停止命令本身失败（Windows 上无窗口的 core 必然如此）就立刻改强杀，
        // 不再空等满 grace——旧实现每次停 core 固定多等 5 秒。
        let status = match process::terminate(pid, false) {
            Ok(()) => wait_exit(&mut running.child, STOP_GRACE),
            Err(_) => None,
        };
        let status = match status {
            Some(status) => Some(status),
            None => {
                let _ = process::terminate(pid, true);
                match wait_exit(&mut running.child, KILL_GRACE) {
                    Some(status) => Some(status),
                    None => return Err(format!("core 进程 {pid} 未能退出")),
                }
            }
        };
        for pump in running.pumps {
            let _ = pump.join();
        }
        if report_exit {
            self.emit(Event::Exited {
                gen,
                err: status.and_then(describe_status),
            });
        }
        Ok(())
    }
}

/// 等子进程退出，最多 `grace`。退出状态由 `Child` 缓存，之后不会再被回收一次。
fn wait_exit(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            // 取不到状态（句柄被系统收走）：就当它退了，别把 Stop 卡死。
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn describe_status_opt(status: Option<ExitStatus>) -> String {
    match status.and_then(describe_status) {
        Some(err) => err,
        None => "进程已退出".to_string(),
    }
}

/// 正常退出（0）返回 None，其余折成一句能进日志的话。
fn describe_status(status: ExitStatus) -> Option<String> {
    if status.success() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(format!("被信号 {signal} 终止"));
        }
    }
    match status.code() {
        Some(code) => Some(format!("退出码 {code}")),
        None => Some("非正常退出".to_string()),
    }
}
