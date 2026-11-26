//! 手写 protobuf：只做字段号读写这一层。
//!
//! 为什么不引 prost/protoc：EasyTier 的 `.proto` 不在仓库里，为五个消息引一套
//! 代码生成不划算；而 Go 那边用的 `protowire` 在 Rust 没有对等物，所以这里
//! 自己写 varint/tag/bytes。字段号都从 `wire.rs` 取，本文件不认识任何业务字段。
//!
//! 解析结果是一串扁平字段而不是结构体：同一字段号可能重复出现（repeated），
//! 也可能整段缺失（proto3 默认值不上线），按需取值比先建结构更不容易出错。

use crate::Error;

const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_BYTES: u8 = 2;
const WIRE_FIXED32: u8 = 5;

/// protobuf 字段号的上限（2^29 - 1）。
const MAX_FIELD_NUMBER: u32 = (1 << 29) - 1;

#[derive(Debug, Clone)]
pub(crate) struct Field {
    pub num: u32,
    pub wt: u8,
    pub uint: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Fields {
    fields: Vec<Field>,
}

impl Fields {
    /// 第一个 varint 字段的值；缺失即 0（proto3 默认值）。
    pub(crate) fn uint(&self, num: u32) -> u64 {
        self.fields
            .iter()
            .find(|field| field.num == num && field.wt == WIRE_VARINT)
            .map(|field| field.uint)
            .unwrap_or(0)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// 第一个字段。`error.Error` 的 oneof 靠“第一个字段的字段号”选分支，
    /// 所以这里必须按出现顺序取，而不是按字段号找。
    pub(crate) fn first(&self) -> Option<&Field> {
        self.fields.first()
    }

    pub(crate) fn has(&self, num: u32) -> bool {
        self.fields.iter().any(|field| field.num == num)
    }

    pub(crate) fn data(&self, num: u32) -> Option<&'_ [u8]> {
        self.fields
            .iter()
            .find(|field| field.num == num && field.wt == WIRE_BYTES)
            .map(|field| field.data.as_slice())
    }

    pub(crate) fn str(&self, num: u32) -> String {
        self.data(num)
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
            .unwrap_or_default()
    }

    pub(crate) fn f32(&self, num: u32) -> f32 {
        self.fields
            .iter()
            .find(|field| field.num == num && field.wt == WIRE_FIXED32)
            .and_then(|field| field.data.get(0..4))
            .map(|raw| f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
            .unwrap_or(0.0)
    }

    /// 解出嵌套 message。
    ///
    /// 解析失败时按“缺失”处理，与 Go 的 `fields.msg` 一致：这些字段只用于界面
    /// 显示，一条坏记录不该把整张节点表清空（`ListPeer` 里 200 个对端，坏一个
    /// 就全不显示，比少一行更糟）。
    pub(crate) fn msg(&self, num: u32) -> Option<Fields> {
        parse(self.data(num)?, "nested").ok()
    }

    /// repeated 字段按出现顺序返回，不打包（proto2 风格，与 EasyTier 生成代码一致）。
    pub(crate) fn repeated(&self, num: u32) -> Vec<&'_ [u8]> {
        self.fields
            .iter()
            .filter(|field| field.num == num && field.wt == WIRE_BYTES)
            .map(|field| field.data.as_slice())
            .collect()
    }

    pub(crate) fn strs(&self, num: u32) -> Vec<String> {
        self.repeated(num)
            .into_iter()
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
            .collect()
    }

    pub(crate) fn msgs(&self, num: u32) -> Vec<Fields> {
        self.repeated(num)
            .into_iter()
            .filter_map(|raw| parse(raw, "nested").ok())
            .collect()
    }
}

/// 把一段字节拆成字段列表。
///
/// 不认识的字段按线上的长度整段跳过（与 protobuf 运行时一致）；长度或 tag
/// 自相矛盾时报错，因为那说明对端不是我们以为的消息。`what` 只用于错误信息
/// （哪一层 message 解不开）。
pub(crate) fn parse(raw: &[u8], what: &'static str) -> Result<Fields, Error> {
    let mut fields = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let (tag, used) = read_varint(rest).ok_or(Error::Protocol { field: what })?;
        rest = &rest[used..];
        let num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;
        if num == 0 || num > MAX_FIELD_NUMBER {
            return Err(Error::Protocol { field: what });
        }
        let mut field = Field {
            num,
            wt: wire_type,
            uint: 0,
            data: Vec::new(),
        };
        match wire_type {
            WIRE_VARINT => {
                let (value, used) = read_varint(rest).ok_or(Error::Protocol { field: what })?;
                field.uint = value;
                rest = &rest[used..];
            }
            WIRE_BYTES => {
                let (length, used) = read_varint(rest).ok_or(Error::Protocol { field: what })?;
                let length =
                    usize::try_from(length).map_err(|_| Error::Protocol { field: what })?;
                let body = rest
                    .get(used..used + length)
                    .ok_or(Error::Protocol { field: what })?;
                field.data = body.to_vec();
                rest = &rest[used + length..];
            }
            WIRE_FIXED64 => {
                let body = rest.get(0..8).ok_or(Error::Protocol { field: what })?;
                field.data = body.to_vec();
                rest = &rest[8..];
            }
            WIRE_FIXED32 => {
                let body = rest.get(0..4).ok_or(Error::Protocol { field: what })?;
                field.data = body.to_vec();
                rest = &rest[4..];
            }
            // proto3 不产生 group（wire type 3/4），EasyTier 也不用；遇到就报错，
            // 而不是猜它的长度继续往下读。
            _ => return Err(Error::Protocol { field: what }),
        }
        fields.push(field);
    }
    Ok(Fields { fields })
}

/// 读一个 varint：最多 10 字节，第 10 字节只能贡献 1 位（与 protowire 一致）。
pub(crate) fn read_varint(raw: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, byte) in raw.iter().take(10).enumerate() {
        let bits = u64::from(byte & 0x7f);
        if index == 9 && bits > 1 {
            return None;
        }
        value |= bits << (7 * index);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

// 编码侧：proto3 的默认值（0、空串、false、空 bytes）不上线，与 EasyTier 生成
// 代码的行为一致。

pub(crate) fn put_uint(out: &mut Vec<u8>, num: u32, value: u64) {
    if value == 0 {
        return;
    }
    put_tag(out, num, WIRE_VARINT);
    put_varint(out, value);
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, num: u32, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    put_tag(out, num, WIRE_BYTES);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub(crate) fn put_str(out: &mut Vec<u8>, num: u32, text: &str) {
    put_bytes(out, num, text.as_bytes());
}

pub(crate) fn put_bool(out: &mut Vec<u8>, num: u32, value: bool) {
    if !value {
        return;
    }
    put_uint(out, num, 1);
}

fn put_tag(out: &mut Vec<u8>, num: u32, wire_type: u8) {
    put_varint(out, (u64::from(num) << 3) | u64::from(wire_type));
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips_at_the_boundaries() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut encoded = Vec::new();
            put_varint(&mut encoded, value);
            assert_eq!(read_varint(&encoded), Some((value, encoded.len())));
        }
    }

    #[test]
    fn a_ten_byte_varint_with_a_bad_last_byte_is_rejected() {
        let mut encoded = vec![0xffu8; 9];
        encoded.push(0x7f);
        assert_eq!(read_varint(&encoded), None);
        let mut encoded = vec![0xffu8; 9];
        encoded.push(0x01);
        assert!(read_varint(&encoded).is_some());
    }

    #[test]
    fn defaults_are_not_encoded() {
        let mut out = Vec::new();
        put_uint(&mut out, 1, 0);
        put_str(&mut out, 2, "");
        put_bool(&mut out, 3, false);
        put_bytes(&mut out, 4, &[]);
        assert!(out.is_empty(), "proto3 默认值不该上线: {out:?}");
    }

    #[test]
    fn unknown_fields_are_skipped_by_length() {
        let mut raw = Vec::new();
        put_uint(&mut raw, 1, 7);
        put_str(&mut raw, 99, "skip me");
        put_uint(&mut raw, 2, 8);
        let fields = parse(&raw, "test").unwrap();
        assert_eq!(fields.uint(1), 7);
        assert_eq!(fields.uint(2), 8);
        assert_eq!(fields.uint(99), 0);
        assert_eq!(fields.str(99), "skip me");
    }

    #[test]
    fn truncated_field_data_is_an_error() {
        // 声明 5 字节，只给 2 字节。
        let raw = [0x0a, 0x05, 0x61, 0x62];
        assert!(matches!(parse(&raw, "test"), Err(Error::Protocol { .. })));
    }

    #[test]
    fn a_zero_field_number_is_an_error() {
        assert!(matches!(
            parse(&[0x00, 0x01], "test"),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn groups_are_rejected_instead_of_guessed() {
        // wire type 3（start group），proto3 不会出现。
        let raw = [(1 << 3) | 3];
        assert!(matches!(parse(&raw, "test"), Err(Error::Protocol { .. })));
    }

    #[test]
    fn nested_messages_decode() {
        let mut inner = Vec::new();
        put_str(&mut inner, 1, "10.126.126.1/24");
        put_uint(&mut inner, 2, 24);
        let mut outer = Vec::new();
        put_bytes(&mut outer, 1, &inner);
        let fields = parse(&outer, "test").unwrap();
        let nested = fields.msg(1).unwrap();
        assert_eq!(nested.str(1), "10.126.126.1/24");
        assert_eq!(nested.uint(2), 24);
    }
}
