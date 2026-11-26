//! `ListPeer` 的应答解码。

use crate::pb;
use crate::wire;
use crate::Error;
use std::time::Duration;

/// 一条到某个节点的隧道。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerConn {
    pub id: String,
    /// `udp` / `tcp`；v6 端点可能带后缀，原样使用。
    pub tunnel: String,
    pub latency: Duration,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub loss_rate: f32,
}

/// 一个直连对等节点。
///
/// 同一台节点可能有两条以上的隧道（例如 udp 与 tcp 各一条），界面上的「协议」列
/// 就是这些隧道类型的并集。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Peer {
    pub peer_id: u32,
    pub conns: Vec<PeerConn>,
}

impl Peer {
    /// 去重并排序后的隧道类型，供界面显示 `tcp,udp`。
    pub fn tunnels(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for conn in &self.conns {
            if conn.tunnel.is_empty() || names.contains(&conn.tunnel) {
                continue;
            }
            names.push(conn.tunnel.clone());
        }
        names.sort();
        names
    }

    /// 该节点上最短的一条已测得延迟。
    ///
    /// 多隧道时快的那条才是实际体验；全都没测到时返回 0（界面显示成“—”）。
    pub fn latency(&self) -> Duration {
        let mut best = Duration::ZERO;
        for conn in &self.conns {
            if conn.latency.is_zero() {
                continue;
            }
            if best.is_zero() || conn.latency < best {
                best = conn.latency;
            }
        }
        best
    }
}

pub(crate) fn decode_list_peer(raw: &[u8]) -> Result<Vec<Peer>, Error> {
    let fields = pb::parse(raw, "ListPeerResponse")?;
    let mut peers = Vec::new();
    for item in fields.msgs(wire::LIST_PEER_FIELD_PEERS) {
        let mut peer = Peer {
            peer_id: item.uint(wire::PEER_FIELD_ID) as u32,
            conns: Vec::new(),
        };
        for conn in item.msgs(wire::PEER_FIELD_CONN) {
            peer.conns.push(decode_peer_conn(&conn));
        }
        peers.push(peer);
    }
    Ok(peers)
}

fn decode_peer_conn(fields: &pb::Fields) -> PeerConn {
    let mut conn = PeerConn {
        id: fields.str(wire::CONN_FIELD_ID),
        loss_rate: fields.f32(wire::CONN_FIELD_LOSS_RATE),
        ..PeerConn::default()
    };
    if let Some(tunnel) = fields.msg(wire::CONN_FIELD_TUNNEL) {
        conn.tunnel = tunnel.str(wire::TUNNEL_FIELD_TYPE);
    }
    if let Some(stats) = fields.msg(wire::CONN_FIELD_STATS) {
        conn.rx_bytes = stats.uint(wire::STATS_FIELD_RX_BYTES);
        conn.tx_bytes = stats.uint(wire::STATS_FIELD_TX_BYTES);
        conn.latency = Duration::from_micros(stats.uint(wire::STATS_FIELD_LATENCY_US));
    }
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/listpeer.bin"
    ));

    #[test]
    fn decodes_a_real_list_peer_response() {
        let peers = decode_list_peer(FIXTURE).unwrap();
        assert_eq!(peers.len(), 4, "对端数量");
        for peer in &peers {
            assert_ne!(peer.peer_id, 0);
            assert!(!peer.conns.is_empty(), "对端 {} 没有隧道", peer.peer_id);
            let tunnels = peer.tunnels();
            assert!(!tunnels.is_empty());
            for name in tunnels {
                assert!(name == "tcp" || name == "udp", "隧道类型异常: {name:?}");
            }
            assert!(!peer.latency().is_zero(), "延迟应大于 0");
        }
    }

    #[test]
    fn latency_takes_the_fastest_tunnel() {
        let peers = decode_list_peer(FIXTURE).unwrap();
        let multi = peers
            .iter()
            .find(|peer| peer.tunnels().len() > 1)
            .expect("样本里应至少有一个双隧道对端");
        let fastest = multi
            .conns
            .iter()
            .map(|conn| conn.latency)
            .filter(|latency| !latency.is_zero())
            .min()
            .unwrap();
        assert_eq!(multi.latency(), fastest);
    }

    #[test]
    fn empty_response_is_an_empty_table() {
        assert!(decode_list_peer(&[]).unwrap().is_empty());
    }
}
