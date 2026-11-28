//! 面板的单元测试：直接构造 StateCell / 引擎替身，不监听端口、不碰托盘，
//! 因此在没有桌面、没有管理员权限的机器上也能跑。
//!
//! Host 与同源判据逐条搬自 Go 的 internal/uits/server_test.go。

use super::router::{Request, Router};
use super::{read_url, write_url};
use mesh_engine::{Cmd, Published, Snapshot, StateCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

fn published() -> Published {
    Published {
        snapshot: Snapshot {
            status: "online".into(),
            server_url: "http://10.126.126.1:8088".into(),
            device_id: "abc123".into(),
            // 管理面板只在设置页显示它：这是「把本机建进控制台」的凭据。
            pubkey: "ZGV2aWNlLXB1YmxpYy1rZXk=".into(),
            hostname: "laptop".into(),
            config_hash: "deadbeef".into(),
            core_path_used: "/opt/mesh/bin/easytier-core".into(),
            core_running: true,
            core_pid: 42,
            core_version: "2.7.1-abc".into(),
            core_started_at: 1,
            local_ip: "10.126.126.100/24".into(),
            public_ips: vec!["203.0.113.7".into()],
            nat_type_udp: "PortRestricted".into(),
            ..Snapshot::default()
        },
        ..Published::default()
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mesh-panel-test-{}-{}-{name}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        TempDir(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn router_with(
    published: Published,
    logs: PathBuf,
    quit: Arc<dyn Fn() + Send + Sync>,
) -> (Router, Receiver<Cmd>) {
    let cell = Arc::new(StateCell::new(published));
    let (tx, rx) = channel();
    (Router::new(cell, tx, logs, quit), rx)
}

fn router() -> (Router, Receiver<Cmd>) {
    router_with(published(), PathBuf::from("unused.log"), Arc::new(|| {}))
}

fn request(method: &str, path: &str) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        host: "127.0.0.1:11020".to_string(),
        headers: Vec::new(),
        body: Vec::new(),
        too_large: false,
    }
}

fn with_header(mut request: Request, name: &str, value: &str) -> Request {
    request.headers.push((name.to_string(), value.to_string()));
    request
}

fn body_json(response: &super::router::Response) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("响应体应当是 JSON")
}

// ---- Host 与同源 ----

#[test]
fn loopback_host_accepts_the_three_loopback_forms() {
    for host in ["127.0.0.1:11020", "localhost:11020", "[::1]:11020"] {
        assert!(
            crate::guard::loopback_host(host),
            "loopback_host({host:?}) 应该是 true"
        );
    }
}

#[test]
fn loopback_host_rejects_lan_addresses_and_rebinding_names() {
    for host in [
        "10.126.126.100:11020",
        "evil.example:11020",
        "127.0.0.1.evil.example",
    ] {
        assert!(
            !crate::guard::loopback_host(host),
            "loopback_host({host:?}) 应该是 false"
        );
    }
}

#[test]
fn same_origin_matches_the_go_table() {
    let host = "127.0.0.1:11020";
    let cases: [(&str, Option<&str>, Option<&str>, bool); 7] = [
        ("面板自己的请求", Some("same-origin"), Some(host), true),
        ("地址栏打开", Some("none"), None, true),
        (
            "别的网页发来",
            Some("cross-site"),
            Some("https://evil.example"),
            false,
        ),
        (
            "同站不同源",
            Some("same-site"),
            Some("http://localhost:3000"),
            false,
        ),
        ("无 Sec-Fetch-Site 时比 Origin", None, Some(host), true),
        (
            "无 Sec-Fetch-Site 时 Origin 不符",
            None,
            Some("https://evil.example"),
            false,
        ),
        ("两者都缺按非浏览器放行", None, None, true),
    ];
    for (name, site, origin, want) in cases {
        let mut headers = Vec::new();
        if let Some(site) = site {
            headers.push(("sec-fetch-site".to_string(), site.to_string()));
        }
        if let Some(origin) = origin {
            let origin = if origin.starts_with("http") {
                origin.to_string()
            } else {
                format!("http://{origin}")
            };
            headers.push(("origin".to_string(), origin));
        }
        assert_eq!(
            crate::guard::same_origin(host, &headers),
            want,
            "用例: {name}"
        );
    }
}

#[test]
fn api_rejects_non_loopback_host() {
    let (router, _rx) = router();
    let mut req = request("GET", "/api/snapshot");
    req.host = "10.126.126.100:11020".to_string();
    let response = router.handle(req);
    assert_eq!(response.status, 400);
    assert_eq!(body_json(&response)["error"], "bad_host");
}

#[test]
fn api_rejects_cross_origin_requests() {
    let (router, _rx) = router();
    let req = with_header(request("POST", "/api/quit"), "sec-fetch-site", "cross-site");
    let response = router.handle(req);
    assert_eq!(response.status, 403);
    assert_eq!(body_json(&response)["error"], "cross_origin");
}

#[test]
fn unknown_method_is_405_with_allow_header() {
    let (router, _rx) = router();
    let response = router.handle(request("POST", "/api/snapshot"));
    assert_eq!(response.status, 405);
    assert_eq!(
        response
            .extra_headers
            .iter()
            .find(|(name, _)| *name == "Allow")
            .map(|(_, value)| value.as_str()),
        Some("GET, HEAD")
    );
}

// ---- 静态资源与 SPA 兜底 ----

#[test]
fn root_serves_the_built_page_and_refuses_framing() {
    let (router, _rx) = router();
    let response = router.handle(request("GET", "/"));
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "text/html; charset=utf-8");
    assert!(
        response
            .extra_headers
            .iter()
            .any(|(name, value)| *name == "X-Frame-Options" && value == "DENY"),
        "静态页面必须带 X-Frame-Options: DENY"
    );
    let body = String::from_utf8_lossy(&response.body).to_string();
    assert!(body.contains("Mesh Client"), "页面里应当有标题");
}

#[test]
fn unknown_paths_fall_back_to_the_spa() {
    let (router, _rx) = router();
    let response = router.handle(request("GET", "/data/some/deep/link"));
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "text/html; charset=utf-8");
    assert!(String::from_utf8_lossy(&response.body).contains("<!doctype html>"));
}

#[test]
fn path_traversal_is_not_served_as_a_file() {
    let (router, _rx) = router();
    let response = router.handle(request("GET", "/../../Cargo.toml"));
    assert_eq!(response.status, 200);
    assert!(String::from_utf8_lossy(&response.body).contains("<!doctype html>"));
}

#[test]
fn static_is_not_guarded_like_go() {
    // Go 只把 guard 加在 /api 上：静态页面没有副作用。
    let (router, _rx) = router();
    let mut req = request("GET", "/");
    req.host = "evil.example".to_string();
    assert_eq!(router.handle(req).status, 200);
}

#[test]
fn content_types_follow_the_extension() {
    assert_eq!(
        crate::assets::content_type(Path::new("assets/index.js")),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        crate::assets::content_type(Path::new("index.html")),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        crate::assets::content_type(Path::new("icon.svg")),
        "image/svg+xml"
    );
    assert_eq!(
        crate::assets::content_type(Path::new("unknown.bin")),
        "application/octet-stream"
    );
}

// ---- 只读接口 ----

#[test]
fn snapshot_uses_the_frontend_field_names() {
    let (router, _rx) = router();
    let response = router.handle(request("GET", "/api/snapshot"));
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "application/json; charset=utf-8");
    let body = body_json(&response);
    assert_eq!(body["status"], "online");
    assert_eq!(body["serverUrl"], "http://10.126.126.1:8088");
    assert_eq!(body["deviceId"], "abc123");
    // 公钥是认领的唯一凭据：字段名与前端 Snapshot 一致，设置页要拿得到。
    assert_eq!(body["pubkey"], "ZGV2aWNlLXB1YmxpYy1rZXk=");
    assert_eq!(body["corePid"], 42);
    assert_eq!(body["localIp"], "10.126.126.100/24");
    // publicIps 必须是数组而不是 null（前端类型是 string[]）。
    assert_eq!(body["publicIps"][0], "203.0.113.7");
    assert!(body["publicIps"].is_array());
    assert_eq!(body["autostartSupported"], false);
}

#[test]
fn dashboard_without_a_running_core_has_an_empty_node_list() {
    let (router, _rx) = router();
    let response = router.handle(request("GET", "/api/dashboard"));
    assert_eq!(response.status, 200);
    let body = body_json(&response);
    assert_eq!(body["snapshot"]["status"], "online");
    assert!(
        body["nodes"].as_array().is_some_and(|rows| rows.is_empty()),
        "core 没在跑时节点表是 []，不是 null"
    );
}

#[test]
fn dashboard_with_an_unreachable_core_reports_an_unknown_topology() {
    // core 报的是「在跑」，但 rpc 连接不上：这是「拓扑未知」，不是「一台节点都没有」。
    // 从前这里回一行本机，把查询失败伪装成空拓扑（F11）。
    let mut published = published();
    published.rpc = Some("127.0.0.1:1".parse().unwrap());
    published.core_gen = 7;
    let (router, _rx) = router_with(published, PathBuf::from("unused.log"), Arc::new(|| {}));
    let response = router.handle(request("GET", "/api/dashboard"));
    assert_eq!(response.status, 200);
    let body = body_json(&response);
    assert!(
        body["nodes"].is_null(),
        "查询失败时节点表是 null（未知），不是 []: {body}"
    );
    // 快照照旧可用：失败只影响节点表。
    assert_eq!(body["snapshot"]["status"], "online");
}

#[test]
fn logs_rejects_invalid_line_counts() {
    let (router, _rx) = router();
    for query in ["lines=abc", "lines=0", "lines=-1"] {
        let mut req = request("GET", "/api/logs");
        req.query = query.to_string();
        let response = router.handle(req);
        assert_eq!(response.status, 400, "{query}");
        assert_eq!(body_json(&response)["error"], "bad_lines");
    }
}

#[test]
fn logs_returns_the_file_tail_and_events_filter_app_lines() {
    let dir = TempDir::new("logs");
    let path = dir.join("client.log");
    fs::write(
        &path,
        "10:00:00 [app] 客户端启动 2026.11.26-21
10:00:01 [core] INFO hello
10:00:02 [app] 已连接服务端
",
    )
    .expect("写日志");
    let (router, _rx) = router_with(published(), path, Arc::new(|| {}));

    let mut req = request("GET", "/api/logs");
    req.query = "lines=2".to_string();
    let response = router.handle(req);
    let lines = body_json(&response);
    assert_eq!(lines.as_array().map(|rows| rows.len()), Some(2));
    assert_eq!(lines[1]["source"], "app");

    let response = router.handle(request("GET", "/api/events"));
    let events = body_json(&response);
    let rows = events.as_array().expect("events 是数组");
    assert!(rows.iter().all(|row| row["source"] == "app"));
    assert!(!rows.is_empty());
}

// ---- 修改状态的接口：面板只负责转成 Cmd 并把结果翻译成 HTTP ----

#[test]
fn settings_are_forwarded_to_the_engine() {
    let (router, rx) = router();
    let engine = std::thread::spawn(move || match rx.recv().expect("收到命令") {
        Cmd::Settings(input, reply) => {
            assert_eq!(input.server_url, "http://10.126.126.1:8088");
            assert_eq!(input.hostname, "laptop");
            let _ = reply.send(Ok(Snapshot {
                device_id: "new-id".into(),
                ..Snapshot::default()
            }));
        }
        _ => panic!("应当是 Cmd::Settings"),
    });
    let mut req = request("PUT", "/api/settings");
    req.body = br#"{"serverUrl":"http://10.126.126.1:8088","hostname":"laptop"}"#.to_vec();
    let response = router.handle(req);
    assert_eq!(response.status, 200);
    assert_eq!(body_json(&response)["deviceId"], "new-id");
    engine.join().expect("引擎替身");
}

#[test]
fn settings_rejects_bad_json_and_too_large_bodies() {
    let (router, _rx) = router();
    let mut req = request("PUT", "/api/settings");
    req.body = b"not json".to_vec();
    let response = router.handle(req);
    assert_eq!(response.status, 400);
    assert_eq!(body_json(&response)["error"], "bad_request");

    let mut req = request("PUT", "/api/settings");
    req.too_large = true;
    let response = router.handle(req);
    assert_eq!(response.status, 400);
    assert_eq!(body_json(&response)["error"], "bad_request");
}

#[test]
fn settings_report_the_engine_error_as_400() {
    let (router, rx) = router();
    let engine = std::thread::spawn(move || match rx.recv().expect("收到命令") {
        Cmd::Settings(_, reply) => {
            let _ = reply.send(Err("控制台还不认这把公钥".to_string()));
        }
        _ => panic!("应当是 Cmd::Settings"),
    });
    let mut req = request("PUT", "/api/settings");
    req.body = br#"{"serverUrl":"http://x","hostname":"laptop"}"#.to_vec();
    let response = router.handle(req);
    assert_eq!(response.status, 400);
    assert_eq!(body_json(&response)["error"], "控制台还不认这把公钥");
    engine.join().expect("引擎替身");
}

#[test]
fn autostart_is_forwarded_and_errors_are_400() {
    let (router, rx) = router();
    let engine = std::thread::spawn(move || match rx.recv().expect("收到命令") {
        Cmd::SetAutostart(enabled, reply) => {
            assert!(enabled);
            let _ = reply.send(Err("开机自启在本平台尚未接入（阶段 2）".to_string()));
        }
        _ => panic!("应当是 Cmd::SetAutostart"),
    });
    let mut req = request("PUT", "/api/autostart");
    req.body = br#"{"enabled":true}"#.to_vec();
    let response = router.handle(req);
    assert_eq!(response.status, 400);
    assert!(body_json(&response)["error"]
        .as_str()
        .unwrap_or_default()
        .contains("阶段 2"));
    engine.join().expect("引擎替身");
}

#[test]
fn reset_maps_engine_failure_to_500() {
    let (router, rx) = router();
    let engine = std::thread::spawn(move || match rx.recv().expect("收到命令") {
        Cmd::Reset(reply) => {
            let _ = reply.send(Err("清空设备身份失败".to_string()));
        }
        _ => panic!("应当是 Cmd::Reset"),
    });
    let response = router.handle(request("POST", "/api/reset"));
    assert_eq!(response.status, 500);
    assert_eq!(body_json(&response)["error"], "清空设备身份失败");
    engine.join().expect("引擎替身");
}

#[test]
fn update_returns_the_snapshot_when_the_engine_exits_before_replying() {
    let (router, rx) = router();
    // Windows 的交接路径会先结束进程再回复：替身只把回复端攥住不放。
    let engine = std::thread::spawn(move || match rx.recv().expect("收到命令") {
        Cmd::ApplyUpdate(reply) => {
            std::thread::sleep(std::time::Duration::from_secs(2));
            drop(reply);
        }
        _ => panic!("应当是 Cmd::ApplyUpdate"),
    });
    let response = router.handle(request("POST", "/api/update"));
    assert_eq!(response.status, 200);
    assert_eq!(body_json(&response)["status"], "online");
    let _ = engine.join();
}

#[test]
fn update_reports_an_immediate_engine_error() {
    let (router, rx) = router();
    let engine = std::thread::spawn(move || match rx.recv().expect("收到命令") {
        Cmd::ApplyUpdate(reply) => {
            let _ = reply.send(Err("没有可安装的更新".to_string()));
        }
        _ => panic!("应当是 Cmd::ApplyUpdate"),
    });
    let response = router.handle(request("POST", "/api/update"));
    assert_eq!(response.status, 400);
    assert_eq!(body_json(&response)["error"], "没有可安装的更新");
    engine.join().expect("引擎替身");
}

#[test]
fn reconnect_is_204_and_sends_the_command() {
    let (router, rx) = router();
    let response = router.handle(request("POST", "/api/reconnect"));
    assert_eq!(response.status, 204);
    assert!(response.body.is_empty());
    match rx.recv().expect("收到命令") {
        Cmd::Reconnect => {}
        _ => panic!("应当是 Cmd::Reconnect"),
    }
}

#[test]
fn quit_answers_204_then_runs_the_callback() {
    let fired = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    let (router, _rx) = router_with(
        published(),
        PathBuf::from("unused.log"),
        Arc::new(move || flag.store(true, Ordering::SeqCst)),
    );
    let mut response = router.handle(request("POST", "/api/quit"));
    assert_eq!(response.status, 204);
    // 响应发出去之前不能已经退出：Go 是回完 204 再 go quit()。
    assert!(!fired.load(Ordering::SeqCst));
    let after = response.after_send.take().expect("退出动作应当在响应之后");
    after();
    assert!(fired.load(Ordering::SeqCst));
}

// ---- config/ui.json ----

#[test]
fn ui_record_roundtrips_and_ignores_missing_or_empty_values() {
    let dir = TempDir::new("ui");
    let path = dir.join("ui.json");
    assert_eq!(read_url(&path), None);
    write_url(&path, "http://127.0.0.1:11021/").expect("写 ui.json");
    assert_eq!(read_url(&path), Some("http://127.0.0.1:11021/".to_string()));
    let raw = fs::read_to_string(&path).expect("读回原文");
    assert!(
        raw.contains("\n  \"url\": "),
        "与 Go 的 MarshalIndent 同形: {raw}"
    );
    fs::write(&path, "{\"url\":\"\"}").expect("写空地址");
    assert_eq!(read_url(&path), None);
    fs::write(&path, "not json").expect("写坏内容");
    assert_eq!(read_url(&path), None);
}
