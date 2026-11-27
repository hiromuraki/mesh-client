//! 路由表与处理函数，对应 Go 的 uits.Server.routes 与各个 handleXxx。
//!
//! 这一层不碰 tiny_http：handle 收一个普通的请求描述、回一个普通的响应描述，单测
//! 因此不需要监听端口，也不需要托盘或管理员权限。HTTP 协议只活在 http.rs。

use crate::assets;
use crate::guard;
use mesh_engine::{build_nodes, Cmd, LocalNode, NodeRow, SettingsInput, Snapshot, StateCell};
use mesh_etrpc::{Peer, Route};
use mesh_text::id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 仪表盘查询 core 的超时（Go 的 4s）。
const QUERY_TIMEOUT: Duration = Duration::from_secs(4);
/// core 查询结果的缓存时长；浏览器 2 秒轮询一次，缓存正好覆盖一轮。
const CACHE_TTL: Duration = Duration::from_secs(2);
/// 设置页请求体上限（Go 的 64KiB）。
const SETTINGS_LIMIT: usize = 64 << 10;
/// 自启开关请求体上限（Go 的 4KiB）。
const AUTOSTART_LIMIT: usize = 4 << 10;
/// 等引擎回复「应用更新」的上限。Windows 交接会在回复之前结束进程，面板不能
/// 一直等下去：超时就按「已经在退出」处理，回当前快照。
const UPDATE_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// 一次 HTTP 请求里路由需要的部分。
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub host: String,
    /// 名字已转成小写。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 读取请求体时超限或出错。会读 body 的路由据此回 bad_request。
    pub too_large: bool,
}

/// 一次响应；after_send 在响应真正发出去之后才跑（/api/quit 要能先回 204）。
pub(crate) struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub cache_control: Option<&'static str>,
    pub extra_headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    pub after_send: Option<Box<dyn FnOnce()>>,
}

impl Response {
    fn json<T: Serialize + ?Sized>(value: &T) -> Response {
        match serde_json::to_vec(value) {
            Ok(body) => Response {
                status: 200,
                content_type: "application/json; charset=utf-8",
                cache_control: Some("no-store"),
                extra_headers: Vec::new(),
                body,
                after_send: None,
            },
            Err(err) => Response::error(
                500,
                &mesh_text::format(id::PANEL_ERROR_SERIALIZE_RESPONSE, &[&err]),
            ),
        }
    }

    pub(crate) fn error(status: u16, message: &str) -> Response {
        let body = serde_json::to_vec(&json!({ "error": message }))
            .unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec());
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            cache_control: None,
            extra_headers: Vec::new(),
            body,
            after_send: None,
        }
    }

    fn no_content() -> Response {
        Response {
            status: 204,
            content_type: "text/plain; charset=utf-8",
            cache_control: None,
            extra_headers: Vec::new(),
            body: Vec::new(),
            after_send: None,
        }
    }

    fn method_not_allowed(allow: &'static str) -> Response {
        Response {
            status: 405,
            content_type: "text/plain; charset=utf-8",
            cache_control: None,
            extra_headers: vec![("Allow", allow.to_string())],
            body: b"Method Not Allowed\n".to_vec(),
            after_send: None,
        }
    }

    /// 静态资源：不能被 iframe 嵌套，否则别的网页可以盖一层透明 iframe 骗用户点
    /// 「退出」「立即更新」。
    pub(crate) fn asset(content_type: &'static str, body: Vec<u8>) -> Response {
        Response {
            status: 200,
            content_type,
            cache_control: Some("no-store"),
            extra_headers: vec![("X-Frame-Options", "DENY".to_string())],
            body,
            after_send: None,
        }
    }
}

/// 十个 /api 路由。表格（Go 方法、Rust 路径、处理函数）见 README 的「面板路由」。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiRoute {
    Snapshot,
    Dashboard,
    Logs,
    Events,
    Settings,
    Autostart,
    Reset,
    Update,
    Reconnect,
    Quit,
}

impl ApiRoute {
    fn from_path(path: &str) -> Option<ApiRoute> {
        Some(match path {
            "/api/snapshot" => ApiRoute::Snapshot,
            "/api/dashboard" => ApiRoute::Dashboard,
            "/api/logs" => ApiRoute::Logs,
            "/api/events" => ApiRoute::Events,
            "/api/settings" => ApiRoute::Settings,
            "/api/autostart" => ApiRoute::Autostart,
            "/api/reset" => ApiRoute::Reset,
            "/api/update" => ApiRoute::Update,
            "/api/reconnect" => ApiRoute::Reconnect,
            "/api/quit" => ApiRoute::Quit,
            _ => return None,
        })
    }

    /// GET 的注册同时接受 HEAD（Go 1.22 的 ServeMux 语义），其余方法严格匹配。
    fn accepts(self, method: &str) -> bool {
        match self {
            ApiRoute::Snapshot | ApiRoute::Dashboard | ApiRoute::Logs | ApiRoute::Events => {
                method == "GET" || method == "HEAD"
            }
            ApiRoute::Settings | ApiRoute::Autostart => method == "PUT",
            ApiRoute::Reset | ApiRoute::Update | ApiRoute::Reconnect | ApiRoute::Quit => {
                method == "POST"
            }
        }
    }

    fn allow(self) -> &'static str {
        match self {
            ApiRoute::Snapshot | ApiRoute::Dashboard | ApiRoute::Logs | ApiRoute::Events => {
                "GET, HEAD"
            }
            ApiRoute::Settings | ApiRoute::Autostart => "PUT",
            _ => "POST",
        }
    }
}

/// 仪表盘和它最近一次 core 查询结果。
#[derive(Default)]
struct Cache {
    at: Option<Instant>,
    peers: Vec<Peer>,
    routes: Vec<Route>,
}

pub(crate) struct Router {
    cell: Arc<StateCell>,
    /// 只在会改状态的动作上用到；mpsc 的 Sender 不是 Sync，包一层 Mutex 才能让
    /// Router 被多个工作线程共享。
    tx: Mutex<Sender<Cmd>>,
    logs: PathBuf,
    quit: Arc<dyn Fn() + Send + Sync>,
    cache: Mutex<Cache>,
}

impl Router {
    pub(crate) fn new(
        cell: Arc<StateCell>,
        tx: Sender<Cmd>,
        logs: PathBuf,
        quit: Arc<dyn Fn() + Send + Sync>,
    ) -> Router {
        Router {
            cell,
            tx: Mutex::new(tx),
            logs,
            quit,
            cache: Mutex::new(Cache::default()),
        }
    }

    pub(crate) fn handle(&self, request: Request) -> Response {
        if let Some(route) = ApiRoute::from_path(&request.path) {
            if !route.accepts(&request.method) {
                return Response::method_not_allowed(route.allow());
            }
            // guard 只罩 /api：静态页面本身没有副作用，Go 也没给它加 guard。
            if !guard::loopback_host(&request.host) {
                return Response::error(400, "bad_host");
            }
            if !guard::same_origin(&request.host, &request.headers) {
                return Response::error(403, "cross_origin");
            }
            return self.dispatch(route, request);
        }
        if request.method != "GET" && request.method != "HEAD" {
            return Response::method_not_allowed("GET, HEAD");
        }
        assets::serve(&request.path)
    }

    fn dispatch(&self, route: ApiRoute, request: Request) -> Response {
        match route {
            ApiRoute::Snapshot => self.snapshot(),
            ApiRoute::Dashboard => self.dashboard(),
            ApiRoute::Logs => self.tail(&request, false),
            ApiRoute::Events => self.tail(&request, true),
            ApiRoute::Settings => self.settings(request),
            ApiRoute::Autostart => self.autostart(request),
            ApiRoute::Reset => self.reset(),
            ApiRoute::Update => self.update(),
            ApiRoute::Reconnect => self.reconnect(),
            ApiRoute::Quit => self.quit(),
        }
    }

    fn snapshot(&self) -> Response {
        let published = self.cell.get();
        Response::json(&published.snapshot)
    }

    /// 数据页内容：快照 + 节点表。core 没在跑（rpc 是 None）时节点表是空数组，
    /// 与 Go 的 Dashboard 在 core 未运行时直接返回一致。
    fn dashboard(&self) -> Response {
        let published = self.cell.get();
        let snapshot = &published.snapshot;
        let mut nodes: Vec<NodeRow> = Vec::new();
        if let Some(addr) = published.rpc {
            let (peers, routes) = self.query_core(addr);
            nodes = build_nodes(
                &peers,
                &routes,
                LocalNode {
                    ip: strip_cidr(&snapshot.local_ip).to_string(),
                    hostname: snapshot.hostname.clone(),
                    version: snapshot.core_version.clone(),
                    nat: snapshot.nat_type_udp.clone(),
                },
            );
        }
        Response::json(&Dashboard {
            snapshot,
            nodes: &nodes,
        })
    }

    /// 查对等节点与路由表，结果缓存 2 秒（Go 的 queryCore）。两次查询各开一个
    /// 连接、并行跑，总耗时取最慢的那个。
    fn query_core(&self, addr: SocketAddr) -> (Vec<Peer>, Vec<Route>) {
        if let Ok(cache) = self.cache.lock() {
            if let Some(at) = cache.at {
                if at.elapsed() < CACHE_TTL {
                    return (cache.peers.clone(), cache.routes.clone());
                }
            }
        }
        let (peers, routes) = std::thread::scope(|scope| {
            let peers = scope.spawn(move || mesh_etrpc::list_peer(addr, QUERY_TIMEOUT));
            let routes = scope.spawn(move || mesh_etrpc::list_route(addr, QUERY_TIMEOUT));
            let peers = peers.join().ok().and_then(Result::ok).unwrap_or_default();
            let routes = routes.join().ok().and_then(Result::ok).unwrap_or_default();
            (peers, routes)
        });
        if let Ok(mut cache) = self.cache.lock() {
            cache.at = Some(Instant::now());
            cache.peers = peers.clone();
            cache.routes = routes.clone();
        }
        (peers, routes)
    }

    /// 日志页与最近事件：lines 缺省或为空用引擎的默认值；给了就必须是正整数
    /// （Go 的 bad_lines）。
    fn tail(&self, request: &Request, events: bool) -> Response {
        let n = match query_value(&request.query, "lines") {
            Some(raw) if !raw.is_empty() => match raw.parse::<usize>() {
                Ok(n) if n > 0 => n,
                _ => return Response::error(400, "bad_lines"),
            },
            _ => 0,
        };
        let lines = if events {
            mesh_engine::events(&self.logs, n)
        } else {
            mesh_engine::logs(&self.logs, n)
        };
        Response::json(&lines)
    }

    fn settings(&self, request: Request) -> Response {
        if request.too_large || request.body.len() > SETTINGS_LIMIT {
            return Response::error(400, "bad_request");
        }
        let Ok(body) = serde_json::from_slice::<SettingsBody>(&request.body) else {
            return Response::error(400, "bad_request");
        };
        let input = SettingsInput {
            server_url: body.server_url,
            token: body.token,
            hostname: body.hostname,
            core_path: body.core_path,
        };
        let (reply_tx, reply_rx) = channel();
        if let Err(err) = self.send(Cmd::Settings(input, reply_tx)) {
            return Response::error(500, &err);
        }
        match reply_rx.recv() {
            Ok(Ok(snapshot)) => Response::json(&snapshot),
            Ok(Err(message)) => Response::error(400, &message),
            Err(_) => Response::error(500, mesh_text::get(id::PANEL_TEXT_ENGINE_NOT_RUNNING)),
        }
    }

    fn autostart(&self, request: Request) -> Response {
        if request.too_large || request.body.len() > AUTOSTART_LIMIT {
            return Response::error(400, "bad_request");
        }
        let Ok(body) = serde_json::from_slice::<AutostartBody>(&request.body) else {
            return Response::error(400, "bad_request");
        };
        let (reply_tx, reply_rx) = channel();
        if let Err(err) = self.send(Cmd::SetAutostart(body.enabled, reply_tx)) {
            return Response::error(500, &err);
        }
        match reply_rx.recv() {
            Ok(Ok(snapshot)) => Response::json(&snapshot),
            Ok(Err(message)) => Response::error(400, &message),
            Err(_) => Response::error(500, mesh_text::get(id::PANEL_TEXT_ENGINE_NOT_RUNNING)),
        }
    }

    fn reset(&self) -> Response {
        let (reply_tx, reply_rx) = channel();
        if let Err(err) = self.send(Cmd::Reset(reply_tx)) {
            return Response::error(500, &err);
        }
        match reply_rx.recv() {
            Ok(Ok(snapshot)) => Response::json(&snapshot),
            Ok(Err(message)) => Response::error(500, &message),
            Err(_) => Response::error(500, mesh_text::get(id::PANEL_TEXT_ENGINE_NOT_RUNNING)),
        }
    }

    /// 应用更新。引擎的交接路径会在回复之前结束进程（Windows），所以回复等不到
    /// 就按「已经在退出」处理：回当前快照，让浏览器拿到一个完整响应。
    fn update(&self) -> Response {
        let (reply_tx, reply_rx) = channel();
        if let Err(err) = self.send(Cmd::ApplyUpdate(reply_tx)) {
            return Response::error(500, &err);
        }
        let snapshot = self.cell.get().snapshot.clone();
        match reply_rx.recv_timeout(UPDATE_REPLY_TIMEOUT) {
            Ok(Ok(())) | Err(RecvTimeoutError::Timeout) => Response::json(&snapshot),
            Ok(Err(message)) => Response::error(400, &message),
            Err(RecvTimeoutError::Disconnected) => {
                Response::error(500, mesh_text::get(id::PANEL_TEXT_ENGINE_NOT_RUNNING))
            }
        }
    }

    fn reconnect(&self) -> Response {
        if let Err(err) = self.send(Cmd::Reconnect) {
            return Response::error(500, &err);
        }
        Response::no_content()
    }

    fn quit(&self) -> Response {
        let quit = Arc::clone(&self.quit);
        Response {
            after_send: Some(Box::new(move || quit())),
            ..Response::no_content()
        }
    }

    fn send(&self, cmd: Cmd) -> Result<(), String> {
        let tx = self
            .tx
            .lock()
            .map_err(|_| mesh_text::get(id::PANEL_TEXT_ENGINE_NOT_RUNNING).to_string())?;
        tx.send(cmd)
            .map_err(|_| mesh_text::get(id::PANEL_TEXT_ENGINE_NOT_RUNNING).to_string())
    }
}

#[derive(Serialize)]
struct Dashboard<'a> {
    snapshot: &'a Snapshot,
    nodes: &'a [NodeRow],
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct SettingsBody {
    server_url: String,
    token: String,
    hostname: String,
    core_path: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AutostartBody {
    enabled: bool,
}

/// 取一个查询参数；返回原始（未解码）值。lines 只可能是十进制数，不值得为它引
/// 一套 URL 解码。
fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if name == key {
            return Some(value);
        }
    }
    None
}

fn strip_cidr(ip: &str) -> &str {
    match ip.find('/') {
        Some(index) => &ip[..index],
        None => ip,
    }
}
