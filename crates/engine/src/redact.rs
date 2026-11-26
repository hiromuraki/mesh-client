//! 日志脱敏：写盘之前把密钥类字段的值换成 `***`。
//!
//! 只有一个入口（[`crate::log::Log::add`]），app 与 core 的行都会经过：core 的输出里
//! 会有配置回显与握手参数，这些行既会被用户贴进 issue，也会被面板读出来。
//!
//! 脱敏是「按键名找值」的白名单，不是熵检测：宁可漏掉，也不要满屏 `***` 把日志变得没用。

/// 这些键名后面的值一律打码（大小写不敏感）。
// 长的排前面：先匹配到的键名会先被拿去取值，短键名（secret / credential）只是兜底。
const SECRET_KEYS: [&str; 9] = [
    "credential_secret",
    "network_secret",
    "private_key",
    "api_key",
    "token",
    "password",
    "passwd",
    "secret",
    "credential",
];

const MASK: &str = "***";

/// 一行脱敏后的文本。没有命中任何模式时原样返回。
pub(crate) fn line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(key_len) = key_at(text, i) {
            if let Some((start, end)) = value_span(text, i + key_len) {
                out.push_str(&text[i..start]);
                out.push_str(MASK);
                i = end;
                continue;
            }
        }
        if let Some(len) = device_token_at(text, i) {
            out.push_str("etk_***");
            i += len;
            continue;
        }
        let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// `i` 处是不是一个密钥键名；返回键名长度。左边不能紧邻字母数字，右边不能紧邻字母数字
/// （所以 `mytoken` 不算 `token`，而 `ET_CREDENTIAL` 里的 `credential` 算）。
fn key_at(text: &str, i: usize) -> Option<usize> {
    if !text.is_char_boundary(i) {
        return None;
    }
    if i > 0 {
        let prev = text[..i].chars().next_back()?;
        if prev.is_alphanumeric() {
            return None;
        }
    }
    // 按字节比较：比较失败时 key.len() 可能落在多字节字符中间，字符串切片会 panic。
    let rest = text.as_bytes();
    SECRET_KEYS.iter().find_map(|key| {
        let key = key.as_bytes();
        let end = i + key.len();
        if rest.len() < end || !rest[i..end].eq_ignore_ascii_case(key) {
            return None;
        }
        match text[end..].chars().next() {
            Some(c) if c.is_alphanumeric() => None,
            _ => Some(key.len()),
        }
    })
}

/// 从键名之后找到 `= 值` 或 `: 值`（JSON 键还多一个结尾引号），返回值在原文里的
/// 区间：引号包住的值整段换掉，裸值换到分隔符为止。
fn value_span(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = skip_spaces(bytes, from);
    if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        i = skip_spaces(bytes, i + 1);
    }
    if i >= bytes.len() || (bytes[i] != b'=' && bytes[i] != b':') {
        return None;
    }
    i = skip_spaces(bytes, i + 1);
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'"' || bytes[i] == b'\'' {
        let quote = bytes[i];
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != quote {
            j += 1;
        }
        // 引号没闭合就不动：改一半比不改更难读。
        return (j < bytes.len()).then_some((i, j + 1));
    }
    let start = i;
    // 值的字符集是 URL / 十六进制 / 标识符：任何非 ASCII 字节都当作值的结尾，
    // 这样紧跟其后的中文说明不会被整段吞掉。
    while i < bytes.len()
        && bytes[i].is_ascii()
        && !matches!(
            bytes[i],
            b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']' | b'"' | b'\'' | b';'
        )
    {
        i += 1;
    }
    (i > start).then_some((start, i))
}

fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// `i` 处是不是一个入网令牌（`etk_` 加至少 16 位十六进制）。
fn device_token_at(text: &str, i: usize) -> Option<usize> {
    let rest = text.get(i..)?;
    if !rest.starts_with("etk_") {
        return None;
    }
    let digits = rest[4..].bytes().take_while(u8::is_ascii_hexdigit).count();
    (digits >= 16).then_some(4 + digits)
}

#[cfg(test)]
mod tests {
    use super::line;

    #[test]
    fn a_toml_secret_is_masked() {
        assert_eq!(line("network_secret = \"abc123\""), "network_secret = ***");
    }

    #[test]
    fn a_json_secret_is_masked() {
        assert_eq!(
            line(r#"{{"credential_secret":"xyz","n":1}}"#),
            r#"{{"credential_secret":***,"n":1}}"#
        );
    }

    #[test]
    fn env_style_is_masked() {
        assert_eq!(line("ET_CREDENTIAL=deadbeef"), "ET_CREDENTIAL=***");
    }

    #[test]
    fn device_tokens_are_masked() {
        assert_eq!(
            line("注册令牌 etk_0123456789abcdef0123456789 已发出"),
            "注册令牌 etk_*** 已发出"
        );
    }

    #[test]
    fn ordinary_lines_are_untouched() {
        for text in [
            "connected to wg0.daveny.cn:11010",
            "peer 12345 latency 12ms",
            "credential 还没下发",
        ] {
            assert_eq!(line(text), text);
        }
    }

    #[test]
    fn multibyte_text_survives() {
        assert_eq!(
            line("连接失败: token = abc，稍后重试"),
            "连接失败: token = ***，稍后重试"
        );
    }

    #[test]
    fn an_unterminated_quote_is_left_alone() {
        let text = "secret = \"abc";
        assert_eq!(line(text), text);
    }
}
