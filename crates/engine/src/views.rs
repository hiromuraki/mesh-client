//! 对外发布的只读视图：快照、心跳输入、节点表。
//!
//! 这些结构就是面板 JSON 的形状（对应 Go 的 `engine/views.go` 与
//! `client/frontend/src/api/types.ts`），所以字段名用 camelCase、字段集合逐条对齐。

use crate::autostart;
use crate::run::{Engine, Status};
use mesh_etrpc::{Peer, Route};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// 设置页需要的状态。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub status: String,
    pub server_url: String,
    pub device_id: String,
    pub hostname: String,
    /// 配置里保存的设备名；空表示用本机主机名。
    pub hostname_set: String,
    pub config_hash: String,
    /// 配置里保存的 core 路径；空表示自动查找。
    pub core_path: String,
    /// 实际使用的 core 路径。
    pub core_path_used: String,
    pub core_running: bool,
    pub core_pid: u32,
    pub core_version: String,
    /// 本代 core 拉起的时间（unix 秒），0 表示还没起来。
    pub core_started_at: i64,
    pub local_ip: String,
    /// STUN 探测到的公网出口地址；core 还没探完时是空数组（不是 null）。
    pub public_ips: Vec<String>,
    pub nat_type_udp: String,
    pub nat_type_tcp: String,
    pub network_name: String,
    /// `formal`（节点私钥）/ `credential`（凭据），空表示还没有身份。
    pub auth_mode: String,
    pub credential_expiry: i64,
    pub config_updated_at: i64,
    pub encryption: String,
    pub last_heartbeat: i64,
    pub last_error: String,
    pub app_version: String,
    /// 空 / `downloading` / `ready` / `error`。
    pub update_state: String,
    pub update_version: String,
    pub update_notes: String,
    pub update_error: String,
    pub autostart_supported: bool,
    pub autostart_enabled: bool,
}

/// 心跳线程组请求要的参数。
#[derive(Debug, Clone, Default)]
pub struct BeatInput {
    /// 每次现读现算：外部改坏 `config.toml` 也能被服务端发现并纠正。
    pub config_hash: String,
    /// core **实际加载**的那张凭据（F1 的修法），不是磁盘上刚写好的那张。
    pub credential_id: String,
    pub app_version: String,
    /// 内存里最近一份 `appUpdate` 的 sha256：服务端据此把 `appUpdate` 换成精简形状。
    /// 只活在内存里（重启后为空，服务端就回完整内容），`None` 表示还没收到过。
    pub app_update_known: Option<String>,
}

/// 引擎发布给其它线程的只读视图。
///
/// 除快照外还带两样别的线程要用、但不该回问引擎的东西：心跳线程要的三个参数，以及
/// 面板自己查 core 时需要的 rpc 地址（`None` 表示本代 core 没在跑，面板直接返回空
/// 节点表，与 Go 的 `Dashboard` 在 core 未运行时一致）。
#[derive(Debug, Clone, Default)]
pub struct Published {
    pub snapshot: Snapshot,
    pub beat: BeatInput,
    pub rpc: Option<SocketAddr>,
}

/// 跨线程共享的全部可变状态：一个 `RwLock<Arc<Published>>`。
///
/// 引擎每处理完一条消息换一次 `Arc`；面板与托盘只读，一次请求 clone 一个 `Arc`，
/// 不复制快照。引擎阻塞在 HTTP 或 core 命令上时，面板读的仍是上一份快照。
pub struct StateCell(RwLock<Arc<Published>>);

impl StateCell {
    pub fn new(published: Published) -> StateCell {
        StateCell(RwLock::new(Arc::new(published)))
    }

    pub fn get(&self) -> Arc<Published> {
        Arc::clone(
            &self
                .0
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// 只有引擎线程调用。
    pub fn publish(&self, published: Published) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(published);
    }
}

impl Engine {
    /// 当前状态快照。不查询 core、不碰网络，只读引擎自己的状态与磁盘上的摘要。
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot_with_hash(self.hash())
    }

    /// snapshot 用调用方已经算好的配置摘要。一次 publish 里快照与心跳必须用同一份
    /// 摘要：各读一次文件、各算一次，两次取样之间外部改了文件就会自相矛盾。
    pub(crate) fn snapshot_with_hash(&self, hash: String) -> Snapshot {
        let core_running = self.core_running();
        let mut snapshot = Snapshot {
            status: self.status.as_str().to_string(),
            server_url: String::new(),
            device_id: String::new(),
            hostname: self.config.hostname.clone(),
            hostname_set: String::new(),
            config_hash: hash,
            core_path: String::new(),
            core_path_used: self.core_path_used.clone(),
            core_running,
            core_pid: 0,
            core_version: self.core_version.clone(),
            core_started_at: 0,
            local_ip: self.local_ip.clone(),
            public_ips: self.stun.public_ips.clone(),
            nat_type_udp: self.stun.udp_nat_type.clone(),
            nat_type_tcp: self.stun.tcp_nat_type.clone(),
            network_name: self.network_name.clone(),
            auth_mode: self.identity.mode.clone(),
            credential_expiry: self.identity.cred_expiry,
            config_updated_at: self.identity.config_at,
            encryption: self.identity.encryption.clone(),
            last_heartbeat: self.last_hb,
            last_error: self.last_error.clone(),
            app_version: self.config.self_version.clone(),
            update_state: self.update.phase.as_str().to_string(),
            update_version: self.update.version.clone(),
            update_notes: self.update.notes.clone(),
            update_error: self.update.error.clone(),
            autostart_supported: autostart::SUPPORTED,
            autostart_enabled: self.autostart_on,
        };
        if let Some(core) = self.core.as_ref() {
            snapshot.core_pid = core.pid;
            if core.started {
                snapshot.core_started_at = core.started_unix;
            }
        }
        if let Some(client) = self.client.as_ref() {
            snapshot.server_url = client.server_url.clone();
            snapshot.device_id = client.device_id.clone();
            snapshot.hostname_set = client.hostname.clone();
            snapshot.core_path = client.core_path.clone();
            if !client.hostname.trim().is_empty() {
                snapshot.hostname = client.hostname.clone();
            }
        }
        // core 不在了就不能继续报在线：状态与存活必须一致，否则界面显示“在线”
        // 而节点其实已经断了。
        if !core_running && snapshot.status == Status::Online.as_str() {
            snapshot.status = Status::Degraded.as_str().to_string();
            if snapshot.last_error.is_empty() {
                snapshot.last_error =
                    mesh_text::get(mesh_text::id::ENGINE_TEXT_CORE_NOT_RUNNING).to_string();
            }
        }
        if snapshot.last_error.is_empty() && !self.log_error.is_empty() {
            snapshot.last_error = self.log_error.clone();
        }
        snapshot
    }

    /// 发布一次快照。引擎每处理完一条消息调用它。
    pub(crate) fn publish(&self) {
        let hash = self.hash();
        let published = Published {
            snapshot: self.snapshot_with_hash(hash.clone()),
            beat: BeatInput {
                config_hash: hash,
                credential_id: self.credential_id(),
                app_version: self.config.self_version.clone(),
                app_update_known: self.known_app_update(),
            },
            rpc: self.core_rpc_addr(),
        };
        self.cell.publish(published);
    }
}

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
    let mut resolver = PathResolver::new(routes);

    let mut rows: Vec<NodeRow> = Vec::with_capacity(routes.len() + 1);
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
    for route in routes {
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
            row.path = resolver.to(&ip);
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

/// 把路由表还原成完整路径。路由表每一项只给出「到目标的下一跳」，而下一跳是
/// `peer_id`，需要先按 `peer_id` 找到它自己那一行的 IP，再逐级回推；结果按 IP 记忆。
struct PathResolver<'a> {
    routes: HashMap<String, &'a Route>,
    ip_by_peer: HashMap<u32, String>,
    memo: HashMap<String, Vec<Hop>>,
}

impl<'a> PathResolver<'a> {
    fn new(routes: &'a [Route]) -> PathResolver<'a> {
        let mut by_ip = HashMap::with_capacity(routes.len());
        let mut ip_by_peer = HashMap::with_capacity(routes.len());
        for route in routes {
            let ip = strip_cidr(&route.ipv4).to_string();
            ip_by_peer.insert(route.peer_id, ip.clone());
            by_ip.insert(ip, route);
        }
        PathResolver {
            routes: by_ip,
            ip_by_peer,
            memo: HashMap::new(),
        }
    }

    fn to(&mut self, ip: &str) -> Vec<Hop> {
        let mut active = HashSet::new();
        self.walk(ip, &mut active)
    }

    fn walk(&mut self, ip: &str, active: &mut HashSet<String>) -> Vec<Hop> {
        if let Some(hops) = self.memo.get(ip) {
            return hops.clone();
        }
        let next_hop = match self.routes.get(ip) {
            Some(route) => route.next_hop,
            None => return Vec::new(),
        };
        let next = match self.ip_by_peer.get(&next_hop) {
            Some(next) => next.clone(),
            None => return Vec::new(),
        };
        if next == ip {
            self.memo.insert(ip.to_string(), Vec::new());
            return Vec::new();
        }
        // 路由表异常时可能成环，沿当前递归栈截断就够了。
        if !active.insert(ip.to_string()) {
            return Vec::new();
        }
        let mut hops = self.walk(&next, active);
        active.remove(ip);
        let hostname = self
            .routes
            .get(&next)
            .map(|route| route.hostname.clone())
            .unwrap_or_default();
        hops.push(Hop { ip: next, hostname });
        self.memo.insert(ip.to_string(), hops.clone());
        hops
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

fn strip_cidr(ip: &str) -> &str {
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
}
