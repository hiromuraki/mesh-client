//! `ShowNodeInfo` 的应答解码。

use crate::pb;
use crate::wire;
use crate::Error;

/// core 记录的 STUN 探测结果。core 刚起来时里面是空的（探测还没完成）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StunInfo {
    /// 枚举名（`Unknown`、`PortRestricted`、`Symmetric`…）。
    pub udp_nat_type: String,
    pub tcp_nat_type: String,
    /// STUN 报回来的本机公网地址，通常只有一个。
    pub public_ips: Vec<String>,
}

/// 本机的节点信息（`ShowNodeInfo`）。
///
/// `ListPeer` 的 `my_info` 永远是空的，要拿本机身份只能单独查一次。
/// 只保留界面用得到的字段：`NodeInfo` 里还有 `config`（完整实例 TOML，含
/// secure_mode 私钥）、`ip_list` 等，客户端不需要，也就不解码。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeInfo {
    pub peer_id: u32,
    /// 带掩码，形如 `10.126.126.100/24`；未分配地址时为空。
    pub ipv4: String,
    pub hostname: String,
    pub inst_id: String,
    pub version: String,
    pub stun: StunInfo,
}

pub(crate) fn decode_node_info(raw: &[u8]) -> Result<NodeInfo, Error> {
    let fields = pb::parse(raw, "ShowNodeInfoResponse")?;
    let Some(inner) = fields.msg(wire::SHOW_NODE_FIELD_INFO) else {
        // core 还没准备好填这段（刚起的实例），按“什么都没有”处理。
        return Ok(NodeInfo::default());
    };
    let mut node = NodeInfo {
        peer_id: inner.uint(wire::NODE_FIELD_PEER_ID) as u32,
        ipv4: inner.str(wire::NODE_FIELD_IPV4),
        hostname: inner.str(wire::NODE_FIELD_HOSTNAME),
        inst_id: inner.str(wire::NODE_FIELD_INST_ID),
        version: inner.str(wire::NODE_FIELD_VERSION),
        stun: StunInfo::default(),
    };
    if let Some(stun) = inner.msg(wire::NODE_FIELD_STUN_INFO) {
        node.stun = StunInfo {
            udp_nat_type: wire::nat_type_name(stun.uint(wire::STUN_FIELD_UDP_NAT_TYPE)),
            tcp_nat_type: wire::nat_type_name(stun.uint(wire::STUN_FIELD_TCP_NAT_TYPE)),
            public_ips: stun.strs(wire::STUN_FIELD_PUBLIC_IP),
        };
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在一台真实 core（wsl）上抓的应答，抓法见
    /// `client/internal/easytierrpc/capture_test.go`（ET_RPC_ADDR 跑 TestCapture）。
    /// 断言里的值就是那次抓包的内容，重新抓包时跟着改。
    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/nodeinfo.bin"
    ));

    #[test]
    fn decodes_a_real_node_info_response() {
        let node = decode_node_info(FIXTURE).unwrap();
        assert_eq!(node.peer_id, 2_528_517_226);
        assert_eq!(node.ipv4, "10.126.126.4/24");
        assert_eq!(node.hostname, "wsl");
        assert_eq!(node.inst_id, "02a60618-28b0-4eda-859a-bd40280f5b78");
        assert!(
            node.version.starts_with("2.7.1-"),
            "version = {:?}",
            node.version
        );
    }

    #[test]
    fn an_empty_response_is_not_an_error() {
        assert_eq!(decode_node_info(&[]).unwrap(), NodeInfo::default());
    }
}
