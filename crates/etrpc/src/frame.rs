//! EasyTier rpc portal 的帧格式。
//!
//! 裸 TCP，一帧由两层定长头加 protobuf 载荷组成：
//!
//! ```text
//! [4B  tcp_len, LE]       = 16 + len(payload)
//! [16B PeerManagerHeader]  u32 from_peer | u32 to_peer | u8 packet_type
//!                          u8 flags | u8 forward_counter | u8 reserved
//!                          u32 payload_len
//! [payload: protobuf RpcPacket]
//! ```
//!
//! 偏移来自 EasyTier 2.7.1 的 `packet/mod.rs`，与 Go 的 `frame.go` 一一对应。

use crate::Error;
use std::io::{Read, Write};

pub(crate) const TCP_LEN_SIZE: usize = 4;
pub(crate) const PEER_HEADER_SIZE: usize = 16;
pub(crate) const FRAME_HEAD_SIZE: usize = TCP_LEN_SIZE + PEER_HEADER_SIZE;
/// 一帧最多 32 MiB：peer 列表在几百个节点时也只有几十 KB，超过这个量说明对端
/// 不是我们以为的服务，报错比分配一大块内存安全。
pub(crate) const MAX_PAYLOAD_SIZE: usize = 32 << 20;

/// 按上面的布局编码一帧并写出。
///
/// flags / forward_counter / reserved 一律留 0：它们服务于 peer 之间的转发与加密，
/// 管理面的直连 RPC 用不到。
pub(crate) fn write_frame<W: Write>(
    writer: &mut W,
    packet_type: u8,
    from: u32,
    to: u32,
    payload: &[u8],
) -> Result<(), Error> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(Error::Frame(format!(
            "rpc 载荷过大: {} 字节",
            payload.len()
        )));
    }
    let mut frame = vec![0u8; FRAME_HEAD_SIZE + payload.len()];
    let total = (PEER_HEADER_SIZE + payload.len()) as u32;
    frame[0..TCP_LEN_SIZE].copy_from_slice(&total.to_le_bytes());
    frame[TCP_LEN_SIZE..TCP_LEN_SIZE + 4].copy_from_slice(&from.to_le_bytes());
    frame[TCP_LEN_SIZE + 4..TCP_LEN_SIZE + 8].copy_from_slice(&to.to_le_bytes());
    frame[TCP_LEN_SIZE + 8] = packet_type;
    frame[TCP_LEN_SIZE + 12..FRAME_HEAD_SIZE]
        .copy_from_slice(&(payload.len() as u32).to_le_bytes());
    frame[FRAME_HEAD_SIZE..].copy_from_slice(payload);
    writer.write_all(&frame).map_err(io_error)
}

/// 读一帧，返回 packet_type 与载荷。
///
/// 载荷之外的头字段都不需要：本地直连只会收到应答，peer id 与转发计数没有意义。
/// 头里的 `payload_len` 也不校验（与 Go 版一致）：长度以 `tcp_len` 为准，
/// 多出来的那 4 字节是给转发方看的。
pub(crate) fn read_frame<R: Read>(reader: &mut R) -> Result<(u8, Vec<u8>), Error> {
    let mut head = [0u8; TCP_LEN_SIZE];
    reader.read_exact(&mut head).map_err(io_error)?;
    let total = u32::from_le_bytes(head) as usize;
    if !(PEER_HEADER_SIZE..=PEER_HEADER_SIZE + MAX_PAYLOAD_SIZE).contains(&total) {
        return Err(Error::Frame(format!("rpc 帧长度异常: {total}")));
    }
    let mut rest = vec![0u8; total];
    reader.read_exact(&mut rest).map_err(io_error)?;
    Ok((rest[8], rest[PEER_HEADER_SIZE..].to_vec()))
}

/// 读超时单独成变体：上层要区分“core 还没起来/卡住”和“对端不是我们以为的服务”。
fn io_error(err: std::io::Error) -> Error {
    match err.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Error::Timeout,
        _ => Error::Frame(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, 9, 1, 1, b"abc").unwrap();
        assert_eq!(buffer.len(), FRAME_HEAD_SIZE + 3);
        assert_eq!(&buffer[0..4], &19u32.to_le_bytes());
        assert_eq!(&buffer[4..8], &1u32.to_le_bytes());
        assert_eq!(&buffer[8..12], &1u32.to_le_bytes());
        assert_eq!(buffer[12], 9);
        assert_eq!(&buffer[16..20], &3u32.to_le_bytes());
        let mut reader = std::io::Cursor::new(buffer);
        let (packet_type, payload) = read_frame(&mut reader).unwrap();
        assert_eq!(packet_type, 9);
        assert_eq!(payload, b"abc");
    }

    #[test]
    fn a_too_short_length_is_rejected() {
        let mut reader = std::io::Cursor::new(vec![0u8, 0, 0, 0]);
        assert!(matches!(read_frame(&mut reader), Err(Error::Frame(_))));
    }

    #[test]
    fn a_too_long_frame_is_rejected() {
        let mut bytes = ((PEER_HEADER_SIZE + MAX_PAYLOAD_SIZE + 1) as u32)
            .to_le_bytes()
            .to_vec();
        bytes.extend_from_slice(&[0u8; 4]);
        let mut reader = std::io::Cursor::new(bytes);
        assert!(matches!(read_frame(&mut reader), Err(Error::Frame(_))));
    }

    #[test]
    fn an_oversized_payload_is_not_written() {
        let mut buffer = Vec::new();
        let payload = vec![0u8; MAX_PAYLOAD_SIZE + 1];
        assert!(matches!(
            write_frame(&mut buffer, 8, 1, 1, &payload),
            Err(Error::Frame(_))
        ));
    }
}
