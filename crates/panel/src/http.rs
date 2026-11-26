//! HTTP 适配层：tiny_http 的请求/响应与 router 的纯描述之间的转换，以及监听。
//!
//! 线程模型是固定的 4 个工作线程（MODULES.md 5.1）：面板的请求都是毫秒级，
//! 真正会阻塞的只有仪表盘的 core 查询（4s 超时），4 个线程足够覆盖浏览器 2 秒
//! 一次的轮询。

use crate::router::{Request, Response, Router};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Response as HttpResponse, Server};

/// 面板的 HTTP 工作线程数。
pub(crate) const WORKERS: usize = 4;

/// 工作线程检查退出标志的间隔；unblock 之外再留一条兜底路径。
const POLL: Duration = Duration::from_millis(100);

/// 请求体读取的硬上限。路由自己按 64KiB / 4KiB 限，这里只是防止一个畸形请求
/// 把内存吃光。
const MAX_READ: usize = 1 << 20;

/// 从 first 开始顺延 attempts 个端口，全部被占才失败（Go 的 listenPanel）。
pub(crate) fn listen(first: u16, attempts: u16) -> Result<(Server, u16), String> {
    let mut last = String::new();
    for port in first..first + attempts {
        match Server::http(("127.0.0.1", port)) {
            Ok(server) => return Ok((server, port)),
            Err(err) => last = err.to_string(),
        }
    }
    Err(format!(
        "127.0.0.1:{first}..{} 都被占用: {last}",
        first + attempts - 1
    ))
}

pub(crate) fn spawn_workers(server: Arc<Server>, router: Arc<Router>, running: Arc<AtomicBool>) {
    for index in 0..WORKERS {
        let server = Arc::clone(&server);
        let router = Arc::clone(&router);
        let running = Arc::clone(&running);
        let _ = thread::Builder::new()
            .name(format!("mesh-panel-{index}"))
            .spawn(move || worker(&server, &router, &running));
    }
}

fn worker(server: &Server, router: &Router, running: &AtomicBool) {
    while running.load(Ordering::SeqCst) {
        match server.recv_timeout(POLL) {
            Ok(Some(request)) => serve(router, request),
            // 超时：回去看一眼退出标志。
            Ok(None) => {}
            // unblock 或监听 socket 出错：线程退出，close 之后剩下的线程也会走。
            Err(_) => break,
        }
    }
}

fn serve(router: &Router, mut request: tiny_http::Request) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.as_str().as_str().to_ascii_lowercase(),
                header.value.as_str().to_string(),
            )
        })
        .collect();
    let host = headers
        .iter()
        .find(|(field, _)| field == "host")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (url, String::new()),
    };
    let mut body = Vec::new();
    let too_large = match request
        .as_reader()
        .take(MAX_READ as u64 + 1)
        .read_to_end(&mut body)
    {
        Ok(_) => body.len() > MAX_READ,
        Err(_) => true,
    };

    let response = router.handle(Request {
        method: method.clone(),
        path,
        query,
        host,
        headers,
        body,
        too_large,
    });
    send(request, &method, response);
}

fn send(request: tiny_http::Request, method: &str, response: Response) {
    let Response {
        status,
        content_type,
        cache_control,
        extra_headers,
        body,
        after_send,
    } = response;
    // HEAD 只要头：body 丢掉，Content-Length 归零。面板没有客户端依赖 HEAD。
    let body = if method == "HEAD" { Vec::new() } else { body };
    let mut outgoing = HttpResponse::from_data(body).with_status_code(status);
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
        outgoing.add_header(header);
    }
    if let Some(cache_control) = cache_control {
        if let Ok(header) = Header::from_bytes(&b"Cache-Control"[..], cache_control.as_bytes()) {
            outgoing.add_header(header);
        }
    }
    for (name, value) in extra_headers {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            outgoing.add_header(header);
        }
    }
    // 客户端断开（浏览器关页面）不是错误：响应发不出去就把这个请求丢掉。
    let _ = request.respond(outgoing);
    if let Some(after) = after_send {
        after();
    }
}
