//! 一次 RPC 调用：建连、发请求、按片收应答、取出业务载荷。
//!
//! 每次查询一条短连接（与 Go 版一致）：留长连接要处理 core 重启后连接已死、
//! 分片按事务号归并这些情况，而查询本身是毫秒级操作。

use crate::frame;
use crate::pb;
use crate::wire;
use crate::Error;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// `from_peer` / `to_peer` 只是本地占位：portal 不做对端校验，包也不会被转发。
const LOCAL_PEER_ID: u32 = 1;

/// 服务端按 `(from_peer, transaction_id)` 归并分片，所以即使是短连接也要保持
/// 唯一，避免和同进程里别处的取值撞上。
static TRANSACTION_ID: AtomicI64 = AtomicI64::new(0);

/// 单次响应允许的分片数。实测 `DumpRoute` 160KB 也只有 139 片，超过这个数说明
/// 对端不是我们以为的服务，报错比一直收下去安全。
const MAX_PIECES: u32 = 4096;

/// 发一次请求并返回完整的业务载荷。
pub(crate) fn call(
    addr: SocketAddr,
    method: u32,
    payload: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + timeout;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|err| match err.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => Error::Timeout,
            _ => Error::Connect {
                addr,
                reason: err.to_string(),
            },
        })?;
    set_timeout(&stream, deadline)?;

    let transaction = TRANSACTION_ID.fetch_add(1, Ordering::Relaxed) + 1;
    let request = build_request(method, payload, transaction);
    frame::write_frame(
        &mut stream,
        wire::PACKET_RPC_REQ,
        LOCAL_PEER_ID,
        LOCAL_PEER_ID,
        &request,
    )?;

    let mut pieces: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut total = 0u32;
    loop {
        set_timeout(&stream, deadline)?;
        let (packet_type, raw) = frame::read_frame(&mut stream)?;
        if packet_type != wire::PACKET_RPC_RESP {
            return Err(Error::Frame(format!(
                "rpc 应答的 packet_type 是 {packet_type}，不是应答帧"
            )));
        }
        let packet = decode_packet(&raw)?;
        // 单片应答时服务端不写 total_pieces/piece_index（proto3 默认值不上线）。
        let total_pieces = if packet.total_pieces == 0 {
            1
        } else {
            packet.total_pieces
        };
        if total_pieces > MAX_PIECES {
            return Err(Error::Frame(format!("rpc 应答分片数异常: {total_pieces}")));
        }
        if total == 0 {
            total = total_pieces;
        }
        if packet.piece_index < total {
            pieces.insert(packet.piece_index, packet.body);
        }
        if (pieces.len() as u32) < total {
            continue;
        }
        let mut body = Vec::with_capacity(4096);
        for index in 0..total {
            let chunk = pieces.get(&index).ok_or_else(|| {
                Error::Frame(format!("rpc 应答缺少第 {index} 片（共 {total} 片）"))
            })?;
            body.extend_from_slice(chunk);
        }
        return decode_response(&body);
    }
}

/// 每次读之前重设一次超时：整体上限是 deadline，而不是“每片各给一个 timeout”。
fn set_timeout(stream: &TcpStream, deadline: Instant) -> Result<(), Error> {
    let now = Instant::now();
    if now >= deadline {
        return Err(Error::Timeout);
    }
    let remaining = deadline - now;
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|err| Error::Frame(err.to_string()))?;
    stream
        .set_write_timeout(Some(remaining))
        .map_err(|err| Error::Frame(err.to_string()))
}

struct RpcPacket {
    body: Vec<u8>,
    total_pieces: u32,
    piece_index: u32,
}

fn decode_packet(raw: &[u8]) -> Result<RpcPacket, Error> {
    let fields = pb::parse(raw, "RpcPacket")?;
    // transaction_id 只用于服务端归并；客户端一次只有一条连接在飞，读了也没用。
    Ok(RpcPacket {
        body: fields
            .data(wire::RPC_FIELD_BODY)
            .unwrap_or_default()
            .to_vec(),
        total_pieces: fields.uint(wire::RPC_FIELD_TOTAL_PIECES) as u32,
        piece_index: fields.uint(wire::RPC_FIELD_PIECE_INDEX) as u32,
    })
}

/// 从 RpcResponse 里取出业务载荷；带 error 字段时把它翻成人能读的错误，
/// 否则调用方会拿到一段空载荷而不是失败原因。
fn decode_response(raw: &[u8]) -> Result<Vec<u8>, Error> {
    let fields = pb::parse(raw, "RpcResponse")?;
    if let Some(error) = fields.data(wire::RESP_FIELD_ERROR) {
        return Err(Error::Rpc(rpc_error_text(error)));
    }
    Ok(fields
        .data(wire::RESP_FIELD_BODY)
        .unwrap_or_default()
        .to_vec())
}

/// 把 `error.Error` 的 oneof 翻成一句中文。字段号即错误类型，与 common.proto 一致。
fn rpc_error_text(raw: &[u8]) -> String {
    let Ok(fields) = pb::parse(raw, "error.Error") else {
        return "rpc 失败（错误内容无法解析）".to_string();
    };
    if fields.is_empty() {
        return "rpc 失败（未给出原因）".to_string();
    }
    let kind = fields.first().expect("非空已在上面判过");
    let inner = pb::parse(&kind.data, "error.Error").unwrap_or_default();
    match kind.num {
        1 => format!("rpc 失败: {}", inner.str(1)),
        2 => format!("rpc 方法不存在: {} #{}", inner.str(1), inner.uint(2)),
        3 => format!("rpc 服务不存在: {}", inner.str(1)),
        4 => "rpc 请求解码失败".to_string(),
        5 => "rpc 应答编码失败".to_string(),
        6 => format!("rpc 执行失败: {}", inner.str(1)),
        7 => format!("rpc 报文格式错误: {}", inner.str(1)),
        8 => format!("rpc 超时: {}", inner.str(1)),
        other => format!("rpc 失败（错误类型 {other}）"),
    }
}

/// 组装一个 RpcPacket。
///
/// body 不是裸的业务 message：服务端先把它解成 `common.RpcRequest`，再从
/// `request(2)` 取业务载荷。
fn build_request(method: u32, payload: &[u8], transaction: i64) -> Vec<u8> {
    let mut descriptor = Vec::new();
    // domain_name 留空：填任何值都会得到 InvalidService。
    pb::put_str(&mut descriptor, 1, "");
    pb::put_str(&mut descriptor, 2, wire::PROTO_PEER_MANAGE);
    pb::put_str(&mut descriptor, 3, wire::SERVICE_PEER_MANAGE);
    pb::put_uint(&mut descriptor, 4, u64::from(method));

    let mut request = Vec::new();
    pb::put_bytes(&mut request, wire::REQ_FIELD_REQUEST, payload);
    pb::put_uint(
        &mut request,
        wire::REQ_FIELD_TIMEOUT_MS,
        wire::CALL_TIMEOUT_MS,
    );

    let mut packet = Vec::new();
    pb::put_uint(
        &mut packet,
        wire::RPC_FIELD_FROM_PEER,
        u64::from(LOCAL_PEER_ID),
    );
    pb::put_uint(
        &mut packet,
        wire::RPC_FIELD_TO_PEER,
        u64::from(LOCAL_PEER_ID),
    );
    pb::put_uint(
        &mut packet,
        wire::RPC_FIELD_TRANSACTION_ID,
        transaction as u64,
    );
    pb::put_bytes(&mut packet, wire::RPC_FIELD_DESCRIPTOR, &descriptor);
    pb::put_bytes(&mut packet, wire::RPC_FIELD_BODY, &request);
    pb::put_bool(&mut packet, wire::RPC_FIELD_IS_REQUEST, true);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// 起一个只服务一条连接的假 portal。
    ///
    /// 请求帧先在这里吃掉再交给 `serve`：Windows 上带着未读的入站数据关闭 socket
    /// 会发 RST，把还没被读走的应答一起丢掉，测试会莫名其妙地看到连接被重置。
    fn portal<F>(serve: F) -> SocketAddr
    where
        F: FnOnce(&mut TcpStream, u8, Vec<u8>) + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Ok((packet_type, request)) = frame::read_frame(&mut stream) else {
                return;
            };
            serve(&mut stream, packet_type, request);
        });
        addr
    }

    fn send_piece(stream: &mut TcpStream, body: &[u8], index: u32, total: u32) {
        let mut packet = Vec::new();
        pb::put_uint(&mut packet, wire::RPC_FIELD_TRANSACTION_ID, 1);
        pb::put_bytes(&mut packet, wire::RPC_FIELD_BODY, body);
        pb::put_uint(&mut packet, wire::RPC_FIELD_TOTAL_PIECES, u64::from(total));
        pb::put_uint(&mut packet, wire::RPC_FIELD_PIECE_INDEX, u64::from(index));
        frame::write_frame(
            stream,
            wire::PACKET_RPC_RESP,
            LOCAL_PEER_ID,
            LOCAL_PEER_ID,
            &packet,
        )
        .unwrap();
    }

    fn with_body(payload: &[u8]) -> Vec<u8> {
        let mut response = Vec::new();
        pb::put_bytes(&mut response, wire::RESP_FIELD_BODY, payload);
        response
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn reassembles_pieces_arriving_out_of_order() {
        let business = b"hello world".to_vec();
        let response = with_body(&business);
        let split = 5;
        let first = response[..split].to_vec();
        let second = response[split..].to_vec();
        let addr = portal(move |stream, _, _| {
            // 后一片先到：分片按 piece_index 归并，不能假设顺序。
            send_piece(stream, &second, 1, 2);
            send_piece(stream, &first, 0, 2);
        });
        assert_eq!(
            call(addr, wire::METHOD_SHOW_NODE_INFO, &[], TIMEOUT).unwrap(),
            business
        );
    }

    #[test]
    fn the_request_wraps_the_business_payload() {
        let addr = portal(|stream, packet_type, raw| {
            assert_eq!(packet_type, wire::PACKET_RPC_REQ);
            let packet = pb::parse(&raw, "RpcPacket").unwrap();
            assert_eq!(
                packet.uint(wire::RPC_FIELD_FROM_PEER),
                u64::from(LOCAL_PEER_ID)
            );
            assert_eq!(packet.uint(wire::RPC_FIELD_IS_REQUEST), 1);
            let descriptor = packet.msg(wire::RPC_FIELD_DESCRIPTOR).unwrap();
            assert_eq!(descriptor.str(1), "", "domain_name 必须留空");
            assert_eq!(descriptor.str(2), wire::PROTO_PEER_MANAGE);
            assert_eq!(descriptor.str(3), wire::SERVICE_PEER_MANAGE);
            assert_eq!(descriptor.uint(4), u64::from(wire::METHOD_SHOW_NODE_INFO));
            let request = packet.msg(wire::RPC_FIELD_BODY).unwrap();
            let payload = request.data(wire::REQ_FIELD_REQUEST).unwrap().to_vec();
            assert_eq!(
                request.uint(wire::REQ_FIELD_TIMEOUT_MS),
                wire::CALL_TIMEOUT_MS
            );
            send_piece(stream, &with_body(&payload), 0, 1);
        });
        assert_eq!(
            call(addr, wire::METHOD_SHOW_NODE_INFO, b"ping", TIMEOUT).unwrap(),
            b"ping"
        );
    }

    #[test]
    fn rpc_errors_become_readable_messages() {
        let addr = portal(|stream, _, _| {
            let mut inner = Vec::new();
            pb::put_str(&mut inner, 1, "PeerManageRpc");
            pb::put_uint(&mut inner, 2, 9);
            let mut error = Vec::new();
            // 字段号 2 = MethodNotFound。
            pb::put_bytes(&mut error, 2, &inner);
            let mut response = Vec::new();
            pb::put_bytes(&mut response, wire::RESP_FIELD_ERROR, &error);
            send_piece(stream, &response, 0, 1);
        });
        match call(addr, wire::METHOD_SHOW_NODE_INFO, &[], TIMEOUT) {
            Err(Error::Rpc(message)) => {
                assert_eq!(message, "rpc 方法不存在: PeerManageRpc #9");
            }
            other => panic!("应是 Rpc 错误，得到 {other:?}"),
        }
    }

    #[test]
    fn a_request_frame_instead_of_a_response_is_rejected() {
        let addr = portal(|stream, _, _| {
            frame::write_frame(stream, wire::PACKET_RPC_REQ, 1, 1, b"").unwrap();
        });
        assert!(matches!(
            call(addr, wire::METHOD_LIST_PEER, &[], TIMEOUT),
            Err(Error::Frame(_))
        ));
    }

    #[test]
    fn a_stalled_portal_times_out() {
        // 请求已经读走、但一直不应答：客户端应当在自己的超时上限处放弃。
        let addr = portal(|_stream, _, _| {
            std::thread::sleep(Duration::from_millis(400));
        });
        assert!(matches!(
            call(
                addr,
                wire::METHOD_LIST_ROUTE,
                &[],
                Duration::from_millis(80)
            ),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn a_closed_portal_is_a_frame_error() {
        let addr = portal(|stream, _, _| {
            // 读完请求就关，客户端会读到 EOF。
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        assert!(matches!(
            call(addr, wire::METHOD_LIST_PEER, &[], TIMEOUT),
            Err(Error::Frame(_))
        ));
    }
}
