//! 无界面形态：把日志抄到控制台，等 SIGTERM/SIGINT；身份失效就退出。
//!
//! 对应 Go 的 runHeadless。退出只有一条路（Shutdown）：这里收到信号只负责让
//! 主流程返回，停 core 与停面板由 Shutdown 完成。

use crate::follow_log;
use crate::platform;
use crate::APP_TITLE;
use mesh_engine::StateCell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 检查身份是否失效的间隔（Go 的 5 秒 ticker）。
const LOST_CHECK: Duration = Duration::from_secs(5);
/// 等信号的切片长度：够短，identity 状态变化不会被拖住。
const SIGNAL_SLICE: Duration = Duration::from_millis(250);

/// 身份失效时的退出原因（Go 的同一句话）。
const IDENTITY_LOST: &str = "设备身份已失效（服务端拒绝签名请求），请先确认本机时间准确，再用 --server 与 --token 重新注册后启动";

pub(crate) fn run(
    cell: Arc<StateCell>,
    log_path: &Path,
    ready: Box<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let offset = follow_log::file_size(log_path);
    let follower = {
        let stop = Arc::clone(&stop);
        let path = log_path.to_path_buf();
        std::thread::Builder::new()
            .name("mesh-follow-log".to_string())
            .spawn(move || follow_log::follow(&path, offset, &stop))
            .map_err(|err| format!("启动日志跟随失败: {err}"))?
    };

    let (signal_tx, signal_rx) = channel();
    platform::spawn_signal_thread(signal_tx)?;
    platform::say(&format!("{APP_TITLE} 以无界面模式运行，Ctrl+C 退出"));

    let started = Instant::now();
    let mut next_check = started + LOST_CHECK;
    let outcome = loop {
        match signal_rx.recv_timeout(SIGNAL_SLICE) {
            Ok(()) => {
                platform::say("正在退出…");
                break Ok(());
            }
            // 信号线程不在了（例如控制台事件注册失败）也不该挂着。
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if now < next_check {
            continue;
        }
        next_check = now + LOST_CHECK;
        // 启动头几秒引擎还在读本地配置，unconfigured 是瞬态；等满一轮再看。
        if started.elapsed() >= LOST_CHECK && cell.get().snapshot.status == "unconfigured" {
            break Err(IDENTITY_LOST.to_string());
        }
        // 过了这一关说明进程会留下来：此时才让安装器认为这一版启动成功。
        ready();
    };

    stop.store(true, Ordering::SeqCst);
    let _ = follower.join();
    outcome
}
