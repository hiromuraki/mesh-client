//! 拓扑展示：把 `ListPeer` 与 `ListRoute` 拼成界面要的节点行。
//!
//! 生产调用者只有面板：它自己查 core 的 ET RPC、自己按 core 实例缓存结果，拼行是
//! 纯展示层的事，与引擎的状态推进无关；协议结构仍归 `mesh-etrpc`。

use mesh_etrpc::{Peer, Route};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// 路由路径上的一个中间节点。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hop {
    pub ip: String,
    pub hostname: String,
}

/// 节点表的一行：节点信息与它对应的路由信息合并在一起。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRow {
    pub ip: String,
    pub hostname: String,
    pub path_kind: String,
    /// 从本机到该节点途经的中间节点，不含目标自身；直连是空数组。
    pub path: Vec<Hop>,
    /// `path` 是否只走到了半路：路由表成环或下一跳不在表里时推不出整条链路，
    /// 此时 `path` 是已经走到的那一段，不能当成完整路径展示。
    pub path_incomplete: bool,
    pub hops: i32,
    /// 本行用到的隧道协议，已去重排序；本机那一行是整张网的聚合。
    pub tunnels: Vec<String>,
    /// `ListRoute` 给的原值，可能是空串。
    pub nat_type: String,
    /// 到该节点的延迟（毫秒）；`None` 表示未知。
    pub latency_ms: Option<f64>,
    /// core 上报的完整版本串；截取发行号由前端做。
    pub version: String,
    pub proxy_cidrs: Vec<String>,
}

/// 本机那一行需要的字段。`ListPeer` 与 `ListRoute` 都不含本机，只能由调用方补。
#[derive(Debug, Clone, Default)]
pub struct LocalNode {
    pub ip: String,
    pub hostname: String,
    pub version: String,
    /// 本机自己的 STUN 判定结果（快照里的 `natTypeUdp`）。
    pub nat: String,
}

/// 把两张表拼成界面要的行：路由给出地址、主机名、跳数与 NAT 类型，peer 给出隧道
/// 类型与实测延迟。中继链路上的节点只有路由、没有 peer。
pub fn build_nodes(peers: &[Peer], routes: &[Route], local: LocalNode) -> Vec<NodeRow> {
    let peer_by_id: HashMap<u32, &Peer> = peers.iter().map(|peer| (peer.peer_id, peer)).collect();
    // 一台机器可能在路由表里占两条：`peer_id` 标识的是「一次进程生命」，重启后旧的那条
    // 还要在网络里挂一阵（最长一小时），而地址是写死的，于是两个 peer_id 撞同一个 IPv4。
    // 这一列叫「节点」，一台机器一行，所以先按地址归并再往下拼。
    let live = one_route_per_address(routes, &peer_by_id);
    // 路径推导拿完整路由表：归并只发生在展示阶段，而下一跳引用的是 `peer_id`，
    // 被归并掉的那个 ID 仍可能是别的节点的下一跳。
    let mut resolver = PathResolver::new(&live, routes);

    let mut rows: Vec<NodeRow> = Vec::with_capacity(live.len() + 1);
    if !local.ip.is_empty() {
        rows.push(NodeRow {
            ip: local.ip.clone(),
            hostname: local.hostname.clone(),
            path_kind: "me".to_string(),
            nat_type: local.nat.clone(),
            version: local.version.clone(),
            ..NodeRow::default()
        });
    }
    for route in &live {
        let ip = strip_cidr(&route.ipv4).to_string();
        if !local.ip.is_empty() && ip == local.ip {
            // 对端路由表里尚未过期的旧会话会把本机学回来，跳过以免出现两行本机。
            continue;
        }
        let peer = peer_by_id.get(&route.peer_id).copied();
        let mut row = NodeRow {
            ip: ip.clone(),
            hostname: route.hostname.clone(),
            path_kind: "direct".to_string(),
            path: Vec::new(),
            path_incomplete: false,
            tunnels: Vec::new(),
            nat_type: route.nat_type.clone(),
            latency_ms: millis(route.path_latency),
            version: route.version.clone(),
            hops: route.cost,
            proxy_cidrs: route.proxy_cidrs.clone(),
        };
        // 直连时隧道实测延迟比路由表里的整数毫秒准，有就优先用它。
        if let Some(peer) = peer {
            row.tunnels = peer.tunnels();
            let measured = peer.latency();
            if !measured.is_zero() {
                row.latency_ms = millis(measured);
            }
        }
        if route.cost > 1 {
            row.path_kind = "relay".to_string();
            let (path, incomplete) = resolver.to(&ip);
            row.path = path;
            row.path_incomplete = incomplete;
        }
        rows.push(row);
    }
    // 本机没有“到自己的隧道”。协议列显示这张网里实际在用哪些传输，比“本机监听
    // 了哪些入口”更贴这一列的含义，也不会被 core 默认监听的长列表撑爆。
    if let Some(tunnels) = peer_tunnels(&rows) {
        for row in rows.iter_mut() {
            if row.path_kind == "me" {
                row.tunnels = tunnels.clone();
            }
        }
    }
    rows
}

/// 按地址归并路由：一台机器只留一条。
///
/// 优先留「有活隧道」的那条——`list_peer` 只列还连着的对等节点，所以它在表里就说明
/// 这条路的另一端还活着，另一条同地址的多半是上一个进程留下的。都是残留时按跳数和
/// 延迟挑，尽量留下代价小的。
fn one_route_per_address<'a>(
    routes: &'a [Route],
    peer_by_id: &HashMap<u32, &Peer>,
) -> Vec<&'a Route> {
    let mut chosen: Vec<&Route> = Vec::new();
    let mut at_by_ip: HashMap<&'a str, usize> = HashMap::new();
    for route in routes {
        let ip = strip_cidr(&route.ipv4);
        match at_by_ip.get(ip) {
            Some(&at) => {
                if closer(route, chosen[at], peer_by_id) {
                    chosen[at] = route;
                }
            }
            None => {
                at_by_ip.insert(ip, chosen.len());
                chosen.push(route);
            }
        }
    }
    chosen
}

fn closer(candidate: &Route, current: &Route, peer_by_id: &HashMap<u32, &Peer>) -> bool {
    let tunnel = |route: &Route| peer_by_id.contains_key(&route.peer_id);
    match (tunnel(candidate), tunnel(current)) {
        (true, false) => true,
        (false, true) => false,
        _ => (candidate.cost, candidate.path_latency) < (current.cost, current.path_latency),
    }
}

/// 一条路径的推导结果。
enum Path {
    /// 已经走到目标（到目标的下一跳就是它自己），`hops` 是整条链路。
    Complete(Vec<Hop>),
    /// 路由表成环或下一跳不在表里，`hops` 只是已经走到的那一段。
    Incomplete(Vec<Hop>),
}

impl Path {
    /// 把 `hop` 接到链路前面：先经过它，再走已经推出的那一段。完整性跟着链路走。
    fn prepend(mut self, hop: Hop) -> Path {
        match &mut self {
            Path::Complete(hops) | Path::Incomplete(hops) => hops.insert(0, hop),
        }
        self
    }
}

/// 把路由表还原成完整路径。路由表每一项只给出「到目标的下一跳」，而下一跳是
/// `peer_id`，需要先按 `peer_id` 找到它自己那一行的 IP，再逐级回推；结果按 IP 记忆。
struct PathResolver<'a> {
    /// 展示行用的那一份（一台机器一条）：跳链要与展示出来的那些行一致。
    routes: HashMap<String, &'a Route>,
    /// `peer_id` → IP，用**完整**路由表建：被地址归并掉的 `peer_id` 仍可能是别人的
    /// 下一跳，缺了它这条链路就推不下去。
    ip_by_peer: HashMap<u32, String>,
    /// 已经完整推出来的路径；不完整的那些不记，它们取决于从哪里进入环路。
    memo: HashMap<String, Vec<Hop>>,
}

impl<'a> PathResolver<'a> {
    fn new(live: &[&'a Route], all: &'a [Route]) -> PathResolver<'a> {
        let mut by_ip = HashMap::with_capacity(live.len());
        for route in live {
            by_ip.insert(strip_cidr(&route.ipv4).to_string(), *route);
        }
        PathResolver {
            routes: by_ip,
            ip_by_peer: all
                .iter()
                .map(|route| (route.peer_id, strip_cidr(&route.ipv4).to_string()))
                .collect(),
            memo: HashMap::new(),
        }
    }

    /// 路径，以及「只走到了半路」这个事实。
    fn to(&mut self, ip: &str) -> (Vec<Hop>, bool) {
        match self.walk(ip, &mut HashSet::new()) {
            Path::Complete(hops) => (hops, false),
            Path::Incomplete(hops) => (hops, true),
        }
    }

    fn walk(&mut self, ip: &str, active: &mut HashSet<String>) -> Path {
        if let Some(hops) = self.memo.get(ip) {
            // 完整路径与从哪条链路走进来无关，记下来能省掉重复回推。
            return Path::Complete(hops.clone());
        }
        // 当前链上已经走过的节点；成环的判据在 `advance` 里比这一份。
        active.insert(ip.to_string());
        let path = self.advance(ip, active);
        active.remove(ip);
        if let Path::Complete(hops) = &path {
            self.memo.insert(ip.to_string(), hops.clone());
        }
        path
    }

    /// 沿 `ip` 的下一跳往前走一步。下一跳就是自己时，这条路由直连目标。
    fn advance(&mut self, ip: &str, active: &mut HashSet<String>) -> Path {
        let Some(route) = self.routes.get(ip) else {
            return Path::Incomplete(Vec::new());
        };
        let Some(next) = self.ip_by_peer.get(&route.next_hop).cloned() else {
            // 下一跳不在任何一条路由里：路径只能到这里。
            return Path::Incomplete(Vec::new());
        };
        if next == ip {
            return Path::Complete(Vec::new());
        }
        if active.contains(&next) {
            // 下一跳已经在当前链上：再走下去只会重复，停在这里。
            return Path::Incomplete(Vec::new());
        }
        let hostname = self
            .routes
            .get(&next)
            .map(|route| route.hostname.clone())
            .unwrap_or_default();
        self.walk(&next, active).prepend(Hop { ip: next, hostname })
    }
}

/// `Duration` 换成毫秒；零值表示未知。
fn millis(duration: Duration) -> Option<f64> {
    if duration.is_zero() {
        return None;
    }
    Some(duration.as_secs_f64() * 1000.0)
}

/// 把各行的隧道协议摊平、去重、排序：`udp` 与 `tcp` → `["tcp", "udp"]`。
fn peer_tunnels(rows: &[NodeRow]) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for row in rows {
        if row.path_kind == "me" {
            continue;
        }
        for name in &row.tunnels {
            if name.is_empty() || names.iter().any(|known| known == name) {
                continue;
            }
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        return None;
    }
    names.sort();
    Some(names)
}

/// 去掉 `/掩码`：路由表给的是 `10.126.126.1/24`，界面与比较用的是地址本身。
pub(crate) fn strip_cidr(ip: &str) -> &str {
    match ip.find('/') {
        Some(index) => &ip[..index],
        None => ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_etrpc::PeerConn;
    use std::time::Duration;

    fn peer(peer_id: u32, tunnels: &[(&str, u64)]) -> Peer {
        Peer {
            peer_id,
            conns: tunnels
                .iter()
                .map(|(tunnel, latency_ms)| PeerConn {
                    id: format!("{tunnel}-{peer_id}"),
                    tunnel: tunnel.to_string(),
                    latency: Duration::from_millis(*latency_ms),
                    rx_bytes: 0,
                    tx_bytes: 0,
                    loss_rate: 0.0,
                })
                .collect(),
        }
    }

    fn route(
        peer_id: u32,
        ipv4: &str,
        hostname: &str,
        cost: i32,
        next_hop: u32,
        latency_ms: u64,
    ) -> Route {
        Route {
            peer_id,
            ipv4: ipv4.to_string(),
            hostname: hostname.to_string(),
            cost,
            next_hop,
            path_latency: Duration::from_millis(latency_ms),
            nat_type: "PortRestricted".to_string(),
            proxy_cidrs: Vec::new(),
            version: "2.7.1-abc".to_string(),
        }
    }

    #[test]
    fn merges_peers_and_routes_into_rows() {
        let peers = vec![peer(1, &[("udp", 6), ("tcp", 9)])];
        let routes = vec![route(1, "10.126.126.1/24", "ali-seed", 1, 1, 12)];
        let rows = build_nodes(
            &peers,
            &routes,
            LocalNode {
                ip: "10.126.126.100".to_string(),
                hostname: "laptop".to_string(),
                version: "2.7.1-abc".to_string(),
                nat: "PortRestricted".to_string(),
            },
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path_kind, "me");
        assert_eq!(rows[0].ip, "10.126.126.100");
        // 实测延迟比路由表的整数毫秒准：6ms 覆盖 12ms。
        assert_eq!(rows[1].latency_ms, Some(6.0));
        assert_eq!(rows[1].tunnels, vec!["tcp".to_string(), "udp".to_string()]);
        assert!(rows[1].path.is_empty());
        // 本机那一行的协议列跟着本网实际在用的协议。
        assert_eq!(rows[0].tunnels, vec!["tcp".to_string(), "udp".to_string()]);
    }

    #[test]
    fn relay_routes_carry_the_hop_chain() {
        // 10.126.126.5 经 10.126.126.2 中转；下一跳是 peer_id 2。
        let routes = vec![
            route(2, "10.126.126.2/24", "tencent-seed", 1, 2, 5),
            route(5, "10.126.126.5/24", "DESKTOP", 2, 2, 18),
        ];
        let rows = build_nodes(&[], &routes, LocalNode::default());
        let relay = rows.iter().find(|row| row.ip == "10.126.126.5").unwrap();
        assert_eq!(relay.path_kind, "relay");
        assert_eq!(relay.hops, 2);
        assert_eq!(relay.path.len(), 1);
        assert_eq!(relay.path[0].ip, "10.126.126.2");
        assert_eq!(relay.path[0].hostname, "tencent-seed");
        assert_eq!(relay.latency_ms, Some(18.0));
        assert!(!relay.path_incomplete, "两跳的链推得出来，就是完整的");
    }

    #[test]
    fn a_stale_route_to_ourselves_is_skipped() {
        let routes = vec![route(1, "10.126.126.100/24", "laptop", 1, 1, 1)];
        let rows = build_nodes(
            &[],
            &routes,
            LocalNode {
                ip: "10.126.126.100".to_string(),
                ..LocalNode::default()
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path_kind, "me");
    }

    #[test]
    fn unknown_values_stay_empty_for_the_frontend() {
        let peers = vec![peer(1, &[("", 0)])];
        let routes = vec![route(1, "10.0.0.1/24", "node", 1, 1, 0)];
        let rows = build_nodes(&peers, &routes, LocalNode::default());
        assert!(rows[0].tunnels.is_empty());
        assert_eq!(rows[0].latency_ms, None);
        assert_eq!(rows[0].version, "2.7.1-abc");
    }

    #[test]
    fn a_second_route_for_the_same_address_is_merged() {
        // 同一台机器的两代 peer_id：有隧道的那条才代表现在的它，另一条是上个进程的残留。
        let peers = vec![peer(7, &[("udp", 6)])];
        let routes = vec![
            route(7, "10.126.126.1/24", "ali-seed", 1, 7, 6),
            route(8, "10.126.126.1/24", "ali-seed", 2, 2, 525),
        ];
        let rows = build_nodes(&peers, &routes, LocalNode::default());
        assert_eq!(rows.len(), 1, "一台机器只该有一行: {rows:?}");
        assert_eq!(rows[0].ip, "10.126.126.1");
        assert_eq!(rows[0].path_kind, "direct");
        assert_eq!(rows[0].tunnels, vec!["udp".to_string()]);
        assert_eq!(rows[0].latency_ms, Some(6.0));
    }

    #[test]
    fn the_route_with_a_live_tunnel_wins_over_a_cheaper_stale_one() {
        // 残留那条看起来更近（1 跳），但它没有隧道；跳数少的假象不能压过“还连着”。
        let peers = vec![peer(9, &[("udp", 20)])];
        let routes = vec![
            route(2, "10.126.126.2/24", "tencent-seed", 1, 2, 5),
            route(10, "10.126.126.9/24", "node", 1, 10, 3),
            route(9, "10.126.126.9/24", "node", 2, 2, 30),
        ];
        let rows = build_nodes(&peers, &routes, LocalNode::default());
        let node = rows.iter().find(|row| row.ip == "10.126.126.9").unwrap();
        assert_eq!(node.path_kind, "relay");
        assert_eq!(node.tunnels, vec!["udp".to_string()]);
        assert_eq!(node.path.len(), 1);
        assert_eq!(node.path[0].ip, "10.126.126.2");
    }

    #[test]
    fn a_merged_peer_id_still_resolves_as_a_next_hop() {
        // 10.126.126.1 有两代 peer_id：7 有活隧道（展示行留它），8 是上一代的残留。
        // 10.126.126.5 的下一跳写的是 8——被地址归并掉的那个 ID，路径仍要推得出来：
        // 归并只发生在展示阶段，推导用的是完整的 peer_id 路由图。
        let peers = vec![peer(7, &[("udp", 6)])];
        let routes = vec![
            route(7, "10.126.126.1/24", "ali-seed", 1, 7, 6),
            route(8, "10.126.126.1/24", "ali-seed", 2, 2, 525),
            route(2, "10.126.126.2/24", "tencent-seed", 1, 2, 5),
            route(5, "10.126.126.5/24", "DESKTOP", 2, 8, 18),
        ];
        let rows = build_nodes(&peers, &routes, LocalNode::default());
        assert_eq!(rows.len(), 3, "一台机器一行: {rows:?}");
        let relay = rows.iter().find(|row| row.ip == "10.126.126.5").unwrap();
        assert_eq!(relay.path_kind, "relay");
        assert!(!relay.path_incomplete, "被归并的下一跳仍在图里: {relay:?}");
        assert_eq!(relay.path.len(), 1);
        assert_eq!(relay.path[0].ip, "10.126.126.1");
        assert_eq!(relay.path[0].hostname, "ali-seed");
    }

    #[test]
    fn a_cycle_is_reported_as_an_incomplete_path() {
        // 4 的下一跳是 6，6 的下一跳是 4：路由表成环，推不出整条链路。
        let routes = vec![
            route(4, "10.126.126.4/24", "alpha", 3, 6, 50),
            route(6, "10.126.126.6/24", "beta", 3, 4, 50),
        ];
        let rows = build_nodes(&[], &routes, LocalNode::default());
        let node = rows.iter().find(|row| row.ip == "10.126.126.4").unwrap();
        assert!(node.path_incomplete, "成环不是完整路径: {node:?}");
        // 只走到环又回到自己的地方为止：这一段仍然给出来，但标明只有这一段。
        assert_eq!(node.path.len(), 1);
        assert_eq!(node.path[0].ip, "10.126.126.6");
        assert_eq!(node.path[0].hostname, "beta");
    }

    #[test]
    fn a_dangling_next_hop_is_reported_as_an_incomplete_path() {
        // 下一跳 99 不在路由表里：只知道要绕行，绕到哪一段推不出来。
        let routes = vec![route(5, "10.126.126.5/24", "DESKTOP", 2, 99, 18)];
        let rows = build_nodes(&[], &routes, LocalNode::default());
        let node = &rows[0];
        assert_eq!(node.path_kind, "relay");
        assert!(node.path_incomplete, "缺失下一跳不是「直接可达」: {node:?}");
        assert!(node.path.is_empty());
    }

    #[test]
    fn between_two_stale_routes_the_shorter_one_wins() {
        let routes = vec![
            route(2, "10.126.126.2/24", "tencent-seed", 1, 2, 5),
            route(11, "10.126.126.9/24", "node", 4, 2, 1525),
            route(12, "10.126.126.9/24", "node", 2, 2, 525),
        ];
        let rows = build_nodes(&[], &routes, LocalNode::default());
        let node = rows.iter().find(|row| row.ip == "10.126.126.9").unwrap();
        assert_eq!(node.hops, 2);
        assert_eq!(node.latency_ms, Some(525.0));
    }
}
