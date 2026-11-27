use std::cmp::Ordering;

/// 发布版本号：`YYYY.MM.DD-N`，这是唯一的形状。
///
/// 定宽是契约的一部分：`2026.1.1-1` 与 `2026.01.01-1` 若都合法，同一个版本就有两种
/// 写法，而版本号同时是签名文本的一行、安装目录里的名字和「谁更新」的判据。Go 侧
/// `signing.ParseVersion` 是同一份规则，两边各有一次实现、一份同样的用例表。
///
/// | 段 | 形状 |
/// | --- | --- |
/// | `YYYY` | 4 位数字，2000..=2999 |
/// | `MM` | 2 位数字，01..=12 |
/// | `DD` | 2 位数字，01..=31 |
/// | `N` | 1..=4 位十进制，无前导零，1..=9999 |
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    year: u16,
    month: u8,
    day: u8,
    seq: u16,
}

/// 解析版本号。形状之外的一切都返回 `None`，没有「差不多」的解析。
pub fn parse(text: &str) -> Option<Version> {
    // `YYYY.MM.DD-` 是 11 字节，序号再加 1..=4 字节。
    if text.len() < 12 || text.len() > 15 {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes[4] != b'.' || bytes[7] != b'.' || bytes[10] != b'-' {
        return None;
    }
    let year = decimal(&bytes[0..4])?;
    if !(2000..=2999).contains(&year) {
        return None;
    }
    let month = decimal(&bytes[5..7])?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let day = decimal(&bytes[8..10])?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let rest = &bytes[11..];
    // 前导零会让同一个版本有两种写法。
    if rest[0] == b'0' {
        return None;
    }
    let seq = decimal(rest)?;
    if !(1..=9999).contains(&seq) {
        return None;
    }
    // 月、日、序号都已经落在各自的范围里，转窄不会溢出。
    Some(Version {
        year,
        month: u8::try_from(month).ok()?,
        day: u8::try_from(day).ok()?,
        seq,
    })
}

/// 版本号是否落在契约里。
pub fn valid(text: &str) -> bool {
    parse(text).is_some()
}

/// 比较两个版本号。
///
/// 必须解析后再比，不能按字符串：字典序里 `2026.11.26-9` 排在 `2026.11.26-11`
/// 后面，而那正好是「旧版本看起来更新」。2026-11-26 两个种子被降级到 -3、
/// 全部 401、整个网络掉线，起点就是这里少了一道方向判断。
///
/// 解析不出来的版本号一律按相等处理：宁可不更新，也不要在不认识的格式上做决定。
/// 这条与 Go 版 `Compare` 一致，两边的「相等」都意味着「不当成更新」。
pub fn compare(a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => Ordering::Equal,
    }
}

/// 判断 `candidate` 是否严格新于 `current`。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

/// 一段 ASCII 十进制数字。
///
/// 不能直接用 `str::parse`：Rust 接受前导 `+`（`"+7"` 解析成 7），Go 的
/// `strconv` 不接受。差一位的后果是同一串版本号在两边判出不同的方向，
/// 那是这套比较里唯一不能有的分歧。
fn decimal(bytes: &[u8]) -> Option<u16> {
    let mut value: u16 = 0;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u16::from(byte - b'0'))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 Go 侧 `TestParseVersion` 同一张表：两边各写一次实现，用例不许不一样。
    #[test]
    fn parses_every_legal_shape() {
        for good in [
            "2026.11.27-117",
            "2026.11.27-1",
            "2000.01.01-1",
            "2999.12.31-9999",
            "2026.11.27-1000",
        ] {
            assert!(valid(good), "{good} 应当合法");
        }
    }

    /// 每条非法用例只坏一处，失败时能看出是哪条规则松了。
    #[test]
    fn rejects_everything_outside_the_domain() {
        for bad in [
            "",
            // 月、日必须是两位。
            "2026.1.1-1",
            // 序号不许有前导零。
            "2026.11.26-013",
            "v2026.11.26-1",
            "2026.11.26-1-rc1",
            "2026.11.26-0",
            "2026.11.26-10000",
            "2026.11.26-",
            "2026.11-1",
            "2026-11-26-1",
            "1999.12.31-1",
            "3000.01.01-1",
            "2026.00.01-1",
            "2026.13.01-1",
            "2026.11.00-1",
            "2026.11.32-1",
            "2026.11.26-1 ",
            "2026.11.26-+1",
            "2026.11.26-x",
        ] {
            assert!(!valid(bad), "{bad:?} 不该合法");
        }
    }

    /// 与 Go 侧 `TestCompare` 同一张表。
    #[test]
    fn compares_by_the_numeric_tuple() {
        for (a, b, want) in [
            ("2026.11.26-4", "2026.11.26-4", Ordering::Equal),
            // 字典序里 "9" > "11"，这正是要挡掉的那个坑。
            ("2026.11.26-9", "2026.11.26-11", Ordering::Less),
            ("2026.11.26-11", "2026.11.26-9", Ordering::Greater),
            ("2026.11.26-99", "2026.11.26-100", Ordering::Less),
            // 日期先于序号。
            ("2026.11.25-99", "2026.11.26-1", Ordering::Less),
            ("2026.10.31-99", "2026.11.01-1", Ordering::Less),
            ("2099.12.31-7", "2100.01.01-1", Ordering::Less),
            ("2026.09.30-1", "2026.10.01-1", Ordering::Less),
        ] {
            assert_eq!(compare(a, b), want, "{a} 与 {b}");
        }
    }

    #[test]
    fn unknown_shapes_never_win() {
        for bad in [
            "garbage",
            "",
            "2026.11.26",
            "-13",
            "2026.11.26-013",
            "2026.11.26-+1",
        ] {
            assert!(!is_newer(bad, "2026.11.26-13"), "{bad:?} 不该赢");
            assert!(!is_newer("2026.11.26-13", bad), "不该和不认识的 {bad:?} 比");
            assert_eq!(compare(bad, "2026.11.26-13"), Ordering::Equal);
        }
    }
}
