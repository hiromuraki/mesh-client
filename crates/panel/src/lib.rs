//! 本地面板：回环监听、Host/同源 guard、静态资源与 SPA 兜底、10 个 /api 路由。
//!
//! 对应 Go 的 internal/uits 与 web.go。HTTP 层与引擎解耦：路由只依赖 StateCell、
//! Sender<Cmd> 与日志路径，单测直接构造这三样，不需要托盘、管理员权限或真实端口。
//!
//! 端口从 11020 起顺延 20 个（固定端口便于书签与 ssh -L 转发，被占也不至于让客户端
//! 起不来）；面板地址由调用方写进 config/ui.json。

mod assets;
mod guard;
mod http;
mod router;
mod topology;
mod ui;

#[cfg(test)]
mod tests;

pub use ui::{read_url, write_url};

use mesh_engine::{Cmd, StateCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// 面板端口的首选值。
pub const DEFAULT_PORT: u16 = 11020;
/// 端口被别的进程占着时向后顺延的次数。
pub const PORT_ATTEMPTS: u16 = 20;

/// 面板的退出动作。/api/quit 回完 204 之后调用它（Go 的 Server.quit）。
pub type Quit = Box<dyn Fn() + Send + Sync>;

/// 起面板并返回它的地址。不持有 join handle：关闭就是关监听。
///
/// 与 MODULES.md 3.7 的签名差异：多一个 quit。面板的 /api/quit 必须真的能结束
/// 客户端，而 Shutdown 归 client 所有；把回调塞进 Panel 再设置会让「先起监听、
/// 后装动作」之间有一个不可用的 /api/quit，不如让它在构造时就完整。
pub fn start(
    cell: Arc<StateCell>,
    tx: Sender<Cmd>,
    logs: PathBuf,
    quit: Quit,
) -> Result<Panel, String> {
    let router = Arc::new(router::Router::new(cell, tx, logs, Arc::from(quit)));
    let (server, port) = http::listen(DEFAULT_PORT, PORT_ATTEMPTS)?;
    let server = Arc::new(server);
    let running = Arc::new(AtomicBool::new(true));
    http::spawn_workers(Arc::clone(&server), router, Arc::clone(&running));
    Ok(Panel {
        url: format!("http://127.0.0.1:{port}/"),
        server,
        running,
    })
}

/// 起好的面板。可以 clone 一份交给退出流程去关。
#[derive(Clone)]
pub struct Panel {
    pub url: String,
    server: Arc<tiny_http::Server>,
    running: Arc<AtomicBool>,
}

impl Panel {
    /// 停止监听。幂等；已经在处理的请求会自己跑完。
    pub fn close(&self) {
        if self.running.swap(false, Ordering::SeqCst) {
            // 每个工作线程都阻塞在自己的 recv_timeout 上，unblock 一次只放一个。
            for _ in 0..http::WORKERS {
                self.server.unblock();
            }
        }
    }
}
