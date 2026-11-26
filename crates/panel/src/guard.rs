//! Host 与同源检查，判据逐条照 Go 的 internal/uits/server.go 与 server_test.go。
//!
//! 本地 HTTP 既不是安全边界也不是身份：同机任何进程都能连 127.0.0.1。要挡的只有
//! 借用户浏览器驱动 /api/quit、/api/update 这类副作用接口的跨站请求。浏览器一定
//! 带 Sec-Fetch-Site 或 Origin，所以校验 Host 是回环地址（挡 DNS rebinding）+ 请求
//! 来自本服务自己的页面就够了，不需要令牌。

/// 只接受 127.0.0.1、localhost 与 [::1]（可带端口）。
///
/// 与 Go 一致地按最后一个冒号截断：浏览器访问本面板一定带端口，没端口的主机名
/// 本来就到不了这里。
pub(crate) fn loopback_host(host: &str) -> bool {
    let name = match host.rfind(':') {
        Some(index) => &host[..index],
        None => host,
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// 判断请求是否来自本服务自己的页面。
///
/// Sec-Fetch-Site 是主判据；缺了再比 Origin；两个都没有说明不是浏览器（curl、
/// 脚本、托盘打开的新标签页），放行。
pub(crate) fn same_origin(host: &str, headers: &[(String, String)]) -> bool {
    if let Some(site) = header_value(headers, "sec-fetch-site") {
        if !site.is_empty() {
            return site == "same-origin" || site == "none";
        }
    }
    let Some(origin) = header_value(headers, "origin") else {
        return true;
    };
    // Go 的 strings.TrimSuffix(origin, "/")：只去掉一个结尾斜杠。
    let origin = origin.strip_suffix('/').unwrap_or(origin);
    origin == format!("http://{host}")
}

/// 取一个请求头（名字大小写不敏感）。
pub(crate) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(field, _)| field.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}
