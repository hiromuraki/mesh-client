//! `ListRoute` 的应答解码。

use crate::pb;
use crate::wire;
use crate::Error;
use std::time::Duration;

/// 路由表的一项：本机到某个网段怎么走。
///
/// 直连（`cost` 为 1）时 `next_hop` 就是目标自己；中转时 `next_hop` 是下一跳的
/// `peer_id`，靠它把整条链路逐级串起来。
///
/// 注意 `Route.ipv4_addr` 是 `Ipv4Inet`（message），和 `NodeInfo.ipv4_addr` 的
/// 字符串类型不同，同名同号不同型是这套协议最容易踩的坑。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Route {
    pub peer_id: u32,
    /// 带掩码，形如 `10.126.126.1/24`。
    pub ipv4: String,
    pub hostname: String,
    pub cost: i32,
    pub next_hop: u32,
    pub path_latency: Duration,
    pub nat_type: String,
    pub proxy_cidrs: Vec<String>,
    pub version: String,
}

pub(crate) fn decode_list_route(raw: &[u8]) -> Result<Vec<Route>, Error> {
    let fields = pb::parse(raw, "ListRouteResponse")?;
    Ok(fields
        .msgs(wire::LIST_ROUTE_FIELD_ROUTES)
        .iter()
        .map(decode_route)
        .collect())
}

fn decode_route(fields: &pb::Fields) -> Route {
    let mut route = Route {
        peer_id: fields.uint(wire::ROUTE_FIELD_PEER_ID) as u32,
        hostname: fields.str(wire::ROUTE_FIELD_HOSTNAME),
        cost: fields.uint(wire::ROUTE_FIELD_COST) as i32,
        next_hop: fields.uint(wire::ROUTE_FIELD_NEXT_HOP) as u32,
        proxy_cidrs: fields.strs(wire::ROUTE_FIELD_PROXY_CIDRS),
        version: fields.str(wire::ROUTE_FIELD_VERSION),
        ..Route::default()
    };
    if let Some(inet) = fields.msg(wire::ROUTE_FIELD_IPV4) {
        route.ipv4 = decode_ipv4_inet(&inet);
    }
    if let Some(stun) = fields.msg(wire::ROUTE_FIELD_STUN_INFO) {
        route.nat_type = wire::nat_type_name(stun.uint(wire::STUN_FIELD_UDP_NAT_TYPE));
    }
    // 延迟优先的字段只在有多条路径时出现；缺省时退回普通路径的毫秒数。
    let millis = if fields.has(wire::ROUTE_FIELD_PATH_LATENCY_LATENCY_FIRST) {
        fields.uint(wire::ROUTE_FIELD_PATH_LATENCY_LATENCY_FIRST) as i32
    } else {
        fields.uint(wire::ROUTE_FIELD_PATH_LATENCY) as i32
    };
    route.path_latency = Duration::from_millis(millis.max(0) as u64);
    route
}

/// 把 `Ipv4Inet{address, network_length}` 拼成 `ip/prefix`。
fn decode_ipv4_inet(fields: &pb::Fields) -> String {
    let raw = match fields.msg(wire::INET_FIELD_ADDRESS) {
        Some(address) => address.uint(wire::IPV4_FIELD_ADDR) as u32,
        None => 0,
    };
    let [a, b, c, d] = raw.to_be_bytes();
    format!(
        "{a}.{b}.{c}.{d}/{}",
        fields.uint(wire::INET_FIELD_PREFIX_LEN)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/listroute.bin"
    ));

    fn by_hostname(routes: &[Route], hostname: &str) -> Route {
        routes
            .iter()
            .find(|route| route.hostname == hostname)
            .unwrap_or_else(|| panic!("缺少 {hostname} 的路由: {routes:?}"))
            .clone()
    }

    #[test]
    fn decodes_a_real_list_route_response() {
        let routes = decode_list_route(FIXTURE).unwrap();
        assert_eq!(routes.len(), 5, "路由条数");

        // 直连：下一跳就是自己，延迟走普通路径字段。
        let direct = by_hostname(&routes, "ali-seed");
        assert_eq!(direct.ipv4, "10.126.126.1/24");
        assert_eq!(direct.cost, 1);
        assert_eq!(direct.next_hop, direct.peer_id);
        assert_eq!(direct.path_latency, Duration::from_millis(6));

        // 中继：下一跳是 tencent-seed，延迟用「延迟优先」字段。
        let relay = by_hostname(&routes, "DESKTOP-A3EKN7A");
        assert_eq!(relay.ipv4, "10.126.126.5/24");
        assert_eq!(relay.cost, 2);
        assert_eq!(relay.next_hop, 1_849_544_238);
        assert_eq!(relay.path_latency, Duration::from_millis(18));
        assert_eq!(relay.nat_type, "SymmetricEasyInc");
    }

    #[test]
    fn empty_response_is_an_empty_table() {
        assert!(decode_list_route(&[]).unwrap().is_empty());
    }
}
