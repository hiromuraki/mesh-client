//! core 的 rpc portal：裸 TCP 帧、手写 protobuf、三个查询。
//!
//! 对应 Go 的 `internal/easytierrpc`。EasyTier 的管理 RPC 不是公开协议，也
//! 没有 `.proto` 在仓库里，所以这里手写字段读写，字段号与 `wire.rs` 集中登记：
//! 升级 EasyTier 时只需改那一个文件。
//!
//! 一次调用就是一个短连接。留长连接要额外处理 core 重启后连接已死、分片按事务号
//! 归并这些情况，而查询本身是毫秒级操作，反复建连的代价可以忽略。

mod client;
mod frame;
mod node;
mod pb;
mod peer;
mod route;
mod wire;

pub use node::{NodeInfo, StunInfo};
pub use peer::{Peer, PeerConn};
pub use route::Route;

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::time::Duration;

/// 一次查询的错误。
///
/// `Rpc` 里已经是把 `error.Error` 的 oneof 翻成的一句话，调用方直接显示即可；
/// `Timeout` 与 `Connect` 分开是因为“core 还没起来”和“core 卡住了”在上层是
/// 两种状态（就绪探测要区分它们）。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("连接 rpc portal {addr} 失败: {reason}")]
    Connect { addr: SocketAddr, reason: String },
    #[error("rpc 帧错误: {0}")]
    Frame(String),
    #[error("{field} 的 protobuf 字段非法")]
    Protocol { field: &'static str },
    #[error("{0}")]
    Rpc(String),
    #[error("rpc 查询超时")]
    Timeout,
}

/// 查询本机节点信息（`ShowNodeInfo`）。
pub fn show_node_info(addr: SocketAddr, timeout: Duration) -> Result<NodeInfo, Error> {
    let raw = client::call(addr, wire::METHOD_SHOW_NODE_INFO, &[], timeout)?;
    node::decode_node_info(&raw)
}

/// 查询直连对等节点（`ListPeer`）。中继链路上的节点不在这里，只在路由表里出现。
pub fn list_peer(addr: SocketAddr, timeout: Duration) -> Result<Vec<Peer>, Error> {
    let raw = client::call(addr, wire::METHOD_LIST_PEER, &[], timeout)?;
    peer::decode_list_peer(&raw)
}

/// 查询路由表（`ListRoute`）。刚起的实例可能还没收敛，返回空表是正常状态。
pub fn list_route(addr: SocketAddr, timeout: Duration) -> Result<Vec<Route>, Error> {
    let raw = client::call(addr, wire::METHOD_LIST_ROUTE, &[], timeout)?;
    route::decode_list_route(&raw)
}

/// 每代 core 换一个空闲回环端口。
///
/// 默认的 15888 常被别人（同机别的 EasyTier、上一条没退干净的 core）占着，
/// 查过去会读到别人的节点表。绑定 `:0` 让内核挑一个，然后立刻释放：窗口只有
/// 这几行，真正的监听由 core 自己建立。
pub fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("绑定回环临时端口失败：无法为 core 选端口");
    listener
        .local_addr()
        .expect("读取回环临时端口失败：刚绑定的 socket 没有本地地址")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_loopback_addr_is_a_usable_loopback_port() {
        let addr = free_loopback_addr();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0);
        // 立刻还能绑上（说明它确实空着），这正是 core 接下来要做的事。
        TcpListener::bind(addr).unwrap();
    }
}
