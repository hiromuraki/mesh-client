//! core 输出的落地改写：去掉它自带的时间戳、把密钥打码。
//!
//! 对应 Go 的 `engine.coreOnLine` 里的两条正则（`coreTS` 与 `secretValue`）。这里手写
//! 是为了不引入 regex：规则很短，而正则引擎会给二进制加上几百 KB。

/// 去掉行首的 ISO 时间戳，并给 `local_private_key` / `network_secret` 的值打码。
pub(crate) fn clean(line: &str) -> String {
    mask_secrets(strip_timestamp(line))
}

/// core 每行自带 `2026-11-24T14:37:23.5+08:00  INFO ...` 这样的前缀，日志本身已有
/// 时间列，留着只会重复。格式对不上就原样返回。
fn strip_timestamp(line: &str) -> &str {
    let bytes = line.as_bytes();
    if bytes.len() < 12 {
        return line;
    }
    let date_like = bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
        && bytes[5].is_ascii_digit()
        && bytes[6].is_ascii_digit()
        && bytes[7] == b'-'
        && bytes[8].is_ascii_digit()
        && bytes[9].is_ascii_digit()
        && bytes[10] == b'T';
    if !date_like {
        return line;
    }
    // 时间戳一路到第一个空白；`[0-9:.+-]` 之外的字符说明这不是时间戳。
    let end = bytes.iter().position(|byte| *byte == b' ');
    let Some(end) = end else {
        return line;
    };
    let stamp = &bytes[11..end];
    if !stamp
        .iter()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b':' | b'.' | b'+' | b'-'))
    {
        return line;
    }
    line[end..].trim_start_matches(' ')
}

/// 把两个密钥字段的字符串值换成 `"***"`。
fn mask_secrets(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let Some((index, key)) = find_key(rest) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..index]);
        out.push_str(key);
        rest = &rest[index + key.len()..];
        // 关键字之后：可有空白，然后必须是 `=`，再可有空白，然后必须是 `"`。
        let trimmed = rest.trim_start();
        let after_space = rest.len() - trimmed.len();
        out.push_str(&rest[..after_space]);
        let Some(after_eq) = trimmed.strip_prefix('=') else {
            rest = trimmed;
            continue;
        };
        let value_space = after_eq.len() - after_eq.trim_start().len();
        out.push_str(&trimmed[..1 + value_space]);
        let value = after_eq.trim_start();
        let Some(inner) = value.strip_prefix('"') else {
            rest = value;
            continue;
        };
        // 没有闭合引号（日志被截断）时保持原样：打码不能把一行吃掉。
        let Some(close) = inner.find('"') else {
            out.push('"');
            rest = inner;
            continue;
        };
        out.push_str("\"***\"");
        rest = &inner[close + 1..];
    }
}

const KEYS: [&str; 2] = ["local_private_key", "network_secret"];

fn find_key(line: &str) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for key in KEYS {
        if let Some(index) = line.find(key) {
            if best.map(|(at, _)| index < at).unwrap_or(true) {
                best = Some((index, key));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_core_timestamp_prefix() {
        let line = "2026-11-24T14:37:23.5+08:00  INFO CORE: hello";
        assert_eq!(clean(line), "INFO CORE: hello");
        // 不像时间戳的行原样保留（行首缩进要留着）。
        assert_eq!(clean("  INFO: no timestamp"), "  INFO: no timestamp");
        assert_eq!(clean("plain text"), "plain text");
    }

    #[test]
    fn masks_private_key_and_network_secret() {
        let line = "2026-11-24T14:37:23.5+08:00  INFO CORE: local_private_key = \"SUPERSECRET\"";
        let cleaned = clean(line);
        assert!(!cleaned.contains("SUPERSECRET"), "cleaned = {cleaned}");
        assert!(cleaned.contains("\"***\""), "cleaned = {cleaned}");
        let line = "network_secret=\"hidden-secret\"";
        let cleaned = clean(line);
        assert!(!cleaned.contains("hidden-secret"), "cleaned = {cleaned}");
        assert_eq!(cleaned, "network_secret=\"***\"");
    }

    #[test]
    fn masks_both_keys_on_one_line() {
        let line = "a local_private_key = \"k1\" b network_secret = \"k2\" c";
        assert_eq!(
            clean(line),
            "a local_private_key = \"***\" b network_secret = \"***\" c"
        );
    }

    #[test]
    fn leaves_other_lines_alone() {
        let line = "INFO peer_id = 42";
        assert_eq!(clean(line), line);
        // 关键词出现在句子里但没有赋值：不改写。
        let line = "INFO loading network_secret from config";
        assert_eq!(clean(line), line);
    }
}
