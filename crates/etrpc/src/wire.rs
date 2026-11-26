//! 所有与 EasyTier 内部 wire 格式绑定的常量。
//!
//! 协议不是公开的：升级 EasyTier 时这里是最可能要改的地方。字段号与
//! `client/internal/easytierrpc/wire.go` 逐条对应，改动要两边一起改。
//!
//! 这是一张登记表而不是“用到的常量”：对应 Go 那边暂时没用到的项也留着，
//! 升级 EasyTier 时才能一眼对照，所以整模块允许 dead_code。
#![allow(dead_code)]

/// 对齐的 EasyTier 版本，只用于报错信息与文档追踪。
pub(crate) const WIRE_VERSION: &str = "2.7.1";

/// 帧头里的 packet_type（packet/mod.rs 的 PacketType 枚举）。
pub(crate) const PACKET_RPC_REQ: u8 = 8;
pub(crate) const PACKET_RPC_RESP: u8 = 9;

/// RpcDescriptor 的服务标识。domain_name 必须留空，填任何值都会得到
/// InvalidService。method_index 是 1-based：0 恒为 InvalidMethodIndex。
pub(crate) const PROTO_PEER_MANAGE: &str = "PeerManageRpc";
pub(crate) const SERVICE_PEER_MANAGE: &str = "PeerManageRpc";
pub(crate) const METHOD_LIST_PEER: u32 = 1;
pub(crate) const METHOD_LIST_ROUTE: u32 = 3;
pub(crate) const METHOD_SHOW_NODE_INFO: u32 = 7;

/// RpcPacket 的字段号（easytier-proto/proto/common.proto）。
pub(crate) const RPC_FIELD_FROM_PEER: u32 = 1;
pub(crate) const RPC_FIELD_TO_PEER: u32 = 2;
pub(crate) const RPC_FIELD_TRANSACTION_ID: u32 = 3;
pub(crate) const RPC_FIELD_DESCRIPTOR: u32 = 4;
pub(crate) const RPC_FIELD_BODY: u32 = 5;
pub(crate) const RPC_FIELD_IS_REQUEST: u32 = 6;
/// 大响应按片传输：body 只是整段 RpcResponse 的一个字节区间，收齐 total_pieces
/// 片后按 piece_index 拼接才能解。
pub(crate) const RPC_FIELD_TOTAL_PIECES: u32 = 7;
pub(crate) const RPC_FIELD_PIECE_INDEX: u32 = 8;

/// 请求体外面还要套一层 RpcRequest：业务请求放在 field 2，field 3 是超时毫秒。
pub(crate) const REQ_FIELD_REQUEST: u32 = 2;
pub(crate) const REQ_FIELD_TIMEOUT_MS: u32 = 3;
pub(crate) const CALL_TIMEOUT_MS: u64 = 5000;

/// RpcResponse 的字段号：1 是应答载荷，2 是错误。
pub(crate) const RESP_FIELD_BODY: u32 = 1;
pub(crate) const RESP_FIELD_ERROR: u32 = 2;

/// ShowNodeInfoResponse 只有一个字段。
pub(crate) const SHOW_NODE_FIELD_INFO: u32 = 1;

/// NodeInfo 的字段号。
pub(crate) const NODE_FIELD_PEER_ID: u32 = 1;
pub(crate) const NODE_FIELD_IPV4: u32 = 2;
pub(crate) const NODE_FIELD_HOSTNAME: u32 = 4;
pub(crate) const NODE_FIELD_INST_ID: u32 = 6;
pub(crate) const NODE_FIELD_VERSION: u32 = 9;
pub(crate) const NODE_FIELD_STUN_INFO: u32 = 5;

/// ListPeerResponse 的字段号。my_info(2) 服务端从来不填，本机信息只能走 ShowNodeInfo。
pub(crate) const LIST_PEER_FIELD_PEERS: u32 = 1;

/// PeerInfo 的字段号。
pub(crate) const PEER_FIELD_ID: u32 = 1;
pub(crate) const PEER_FIELD_CONN: u32 = 2;

/// PeerConnInfo 的字段号。
pub(crate) const CONN_FIELD_ID: u32 = 1;
pub(crate) const CONN_FIELD_TUNNEL: u32 = 5;
pub(crate) const CONN_FIELD_STATS: u32 = 6;
pub(crate) const CONN_FIELD_LOSS_RATE: u32 = 7;

/// TunnelInfo 的字段号；local_addr / remote_addr 是 common.Url。
pub(crate) const TUNNEL_FIELD_TYPE: u32 = 1;
pub(crate) const URL_FIELD_URL: u32 = 1;

/// PeerConnStats 的字段号。latency_us 是微秒。
pub(crate) const STATS_FIELD_RX_BYTES: u32 = 1;
pub(crate) const STATS_FIELD_TX_BYTES: u32 = 2;
pub(crate) const STATS_FIELD_LATENCY_US: u32 = 5;

/// ListRouteResponse 与 Route 的字段号。
pub(crate) const LIST_ROUTE_FIELD_ROUTES: u32 = 1;

pub(crate) const ROUTE_FIELD_PEER_ID: u32 = 1;
pub(crate) const ROUTE_FIELD_IPV4: u32 = 2;
pub(crate) const ROUTE_FIELD_NEXT_HOP: u32 = 3;
pub(crate) const ROUTE_FIELD_COST: u32 = 4;
pub(crate) const ROUTE_FIELD_PROXY_CIDRS: u32 = 5;
pub(crate) const ROUTE_FIELD_HOSTNAME: u32 = 6;
pub(crate) const ROUTE_FIELD_STUN_INFO: u32 = 7;
pub(crate) const ROUTE_FIELD_VERSION: u32 = 9;
pub(crate) const ROUTE_FIELD_PATH_LATENCY: u32 = 11;
/// 14 是「以延迟优先的那条路径」的毫秒数，`easytier-cli` 的 lat 列就用它。
pub(crate) const ROUTE_FIELD_PATH_LATENCY_LATENCY_FIRST: u32 = 14;

/// Ipv4Inet / Ipv4Addr 的字段号。addr 是网络字节序的 u32。
pub(crate) const INET_FIELD_ADDRESS: u32 = 1;
pub(crate) const INET_FIELD_PREFIX_LEN: u32 = 2;
pub(crate) const IPV4_FIELD_ADDR: u32 = 1;

/// StunInfo 的字段号：1 udp_nat_type、2 tcp_nat_type、4 public_ip（repeated）。
pub(crate) const STUN_FIELD_UDP_NAT_TYPE: u32 = 1;
pub(crate) const STUN_FIELD_TCP_NAT_TYPE: u32 = 2;
pub(crate) const STUN_FIELD_PUBLIC_IP: u32 = 4;

/// common.NatType 的枚举名，下标即枚举值。
const NAT_TYPE_NAMES: [&str; 10] = [
    "Unknown",
    "OpenInternet",
    "NoPAT",
    "FullCone",
    "Restricted",
    "PortRestricted",
    "Symmetric",
    "SymUdpFirewall",
    "SymmetricEasyInc",
    "SymmetricEasyDec",
];

/// 把枚举值转成名字；越界时退化成 `NatType(n)`，与 Go 版一致（新增枚举值时
/// 界面显示原始数字而不是空白，能立刻看出是“协议比人新”）。
pub(crate) fn nat_type_name(value: u64) -> String {
    match NAT_TYPE_NAMES.get(value as usize) {
        Some(name) => (*name).to_string(),
        None => format!("NatType({value})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_type_names_match_the_enum() {
        assert_eq!(nat_type_name(0), "Unknown");
        assert_eq!(nat_type_name(1), "OpenInternet");
        assert_eq!(nat_type_name(8), "SymmetricEasyInc");
        assert_eq!(nat_type_name(9), "SymmetricEasyDec");
        assert_eq!(nat_type_name(99), "NatType(99)");
    }
}
