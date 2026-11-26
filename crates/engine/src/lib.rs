//! 状态机：唯一可变状态的拥有者。
//!
//! 对应 Go 的 `internal/engine`（除 core 进程部分）、`internal/logbook`。
//!
//! 线程模型与 MODULES.md 5.x 一致：没有 async runtime、没有线程池。引擎线程阻塞在
//! 自己的 inbox 上；心跳、STUN 刷新、下载各自是独立线程，只把结果投回 inbox。
//! “等待退避”不做成 `sleep`，而是把 inbox 的 `recv_timeout` 设成到期时间——这样
//! 退避期间面板的设置请求、配置下发、core 事件照常处理。
//!
//! 阶段 1 只做主干：连接循环、注册、心跳、配置下发与写盘、core 重启判定、401 三支、
//! 更新下载与交接。开机自启（Windows schtasks）、面板、托盘在后续阶段接入。

mod autostart;
mod clock;
mod coreline;
mod log;
mod run;
mod settings;
mod sync;
mod update;
mod views;

#[cfg(test)]
mod tests;

pub use log::{events, logs, Line, Log};
pub use run::{spawn, Cmd, Config, Notice, SettingsInput, Status};
pub use views::{build_nodes, BeatInput, Hop, LocalNode, NodeRow, Published, Snapshot, StateCell};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 一次性的回复通道。会改状态的动作（设置、退出、应用更新）用它同步拿到结果。
pub type Reply<T> = std::sync::mpsc::Sender<T>;

/// 唯一的退出路径（MODULES.md 5.6）：停面板 → 停 core → 关日志 → `process::exit`。
///
/// 各步骤由拥有资源的线程注册（面板由 client 注册，core 由引擎注册）；`run_and_exit`
/// 幂等，托盘退出、`/api/quit`、SIGTERM、更新交接都走它。更新交接必须先拉起启动器
/// 再退出，否则新版本装不上（F7）。
///
/// 归属说明：MODULES.md 3.6 的 `spawn` 把 `Arc<Shutdown>` 注入引擎，3.8 又把 `Shutdown`
/// 算在 `mesh-client` 名下。这里放在 engine：引擎要注册“停 core”那一步，client 只需要
/// 往里再加一步“停面板”。
pub struct Shutdown {
    steps: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    done: AtomicBool,
}

impl Shutdown {
    pub fn new() -> std::sync::Arc<Shutdown> {
        std::sync::Arc::new(Shutdown {
            steps: Mutex::new(Vec::new()),
            done: AtomicBool::new(false),
        })
    }

    /// 注册一步收尾。注册顺序就是执行顺序（先注册先跑）。
    pub fn register(&self, step: impl FnOnce() + Send + 'static) {
        if let Ok(mut steps) = self.steps.lock() {
            steps.push(Box::new(step));
        }
    }

    /// 收尾是否已经开始。引擎线程据此停止工作：`process::exit` 之后就没人读它了，
    /// 而且测试里可以靠它让引擎线程干净退出（不必结束进程）。
    pub fn triggered(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// 跑到退出为止。第二次调用直接退出，不会重跑收尾步骤。
    pub fn run_and_exit(&self, code: i32) -> ! {
        self.run();
        std::process::exit(code)
    }

    /// 只跑收尾、不结束进程。client 在启动失败时要带走 core（Unix 上 core 在
    /// 自己的会话里，不主动停就会变成孤儿），但还得先把错误写给人看。
    pub fn run(&self) {
        if !self.done.swap(true, Ordering::SeqCst) {
            let steps = self
                .steps
                .lock()
                .map(|mut steps| std::mem::take(&mut *steps))
                .unwrap_or_default();
            for step in steps {
                step();
            }
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    #[test]
    fn shutdown_runs_each_step_once() {
        let shutdown = Shutdown::new();
        let counter = Arc::new(AtomicU32::new(0));
        let first = Arc::clone(&counter);
        shutdown.register(move || {
            first.fetch_add(1, Ordering::SeqCst);
        });
        let second = Arc::clone(&counter);
        shutdown.register(move || {
            second.fetch_add(10, Ordering::SeqCst);
        });
        shutdown.run();
        shutdown.run();
        assert_eq!(counter.load(Ordering::SeqCst), 11, "收尾步骤只能跑一次");
    }
}
