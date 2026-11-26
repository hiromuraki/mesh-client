use std::cmp::Ordering;

/// 比较形如 `2026.11.26-13` 的版本号：日期加当日序号。
///
/// 必须解析后再比，不能按字符串：字典序里 `2026.11.26-9` 排在 `2026.11.26-11`
/// 后面，而那正好是“旧版本看起来更新”。2026-11-26 两个种子被降级到 -3、
/// 全部 401、整个网络掉线，起点就是这里少了一道方向判断。
///
/// 解析不出来的版本号一律按相等处理：宁可不更新，也不要在不认识的格式上做决定。
/// 这条与 Go 版 `version.Compare` 一致，两边的“相等”都意味着“不当成更新”。
pub fn compare(a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        (Some((date_a, seq_a)), Some((date_b, seq_b))) => {
            date_a.cmp(&date_b).then(seq_a.cmp(&seq_b))
        }
        _ => Ordering::Equal,
    }
}

/// 判断 `candidate` 是否严格新于 `current`。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

/// 拆成 (日期各段, 当日序号)。日期段数按实际有的比，`2026.11` 比 `2026.11.0` 旧。
fn parse(v: &str) -> Option<(Vec<u64>, u64)> {
    // 序号是最后一个 `-` 之后的部分：`2026.11.26-1-rc1` 的序号是 `rc1`，解析失败，
    // 于是整串按“不认识”处理——预发布后缀不该让版本号看起来更新。
    let (date, seq) = v.rsplit_once('-')?;
    if date.is_empty() || seq.is_empty() {
        return None;
    }
    let seq = parse_u64(seq)?;
    let date: Option<Vec<u64>> = date.split('.').map(parse_u64).collect();
    Some((date?, seq))
}

/// 只接受 ASCII 数字。
///
/// 不能直接用 `str::parse::<u64>`：Rust 接受前导 `+`（`"+7"` 会解析成 7），
/// Go 的 `strconv.ParseUint` 不接受。差一位的后果是同一串版本号在两边判出不同
/// 的方向，那是这套比较里唯一不能有的分歧。
fn parse_u64(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_beats_lexicographic_order() {
        // 字典序里 "9" > "11"，这正是要挡掉的那个坑。
        assert!(is_newer("2026.11.26-11", "2026.11.26-9"));
        assert!(!is_newer("2026.11.26-9", "2026.11.26-11"));
    }

    #[test]
    fn dates_are_compared_numerically() {
        assert!(is_newer("2026.11.27-1", "2026.11.26-99"));
        assert!(is_newer("2027.1.1-1", "2026.12.31-9"));
        assert!(!is_newer("2026.9.9-99", "2026.10.1-1"));
    }

    #[test]
    fn same_version_is_not_newer() {
        assert!(!is_newer("2026.11.26-13", "2026.11.26-13"));
        assert_eq!(compare("2026.11.26-13", "2026.11.26-13"), Ordering::Equal);
    }

    #[test]
    fn unknown_shapes_never_win() {
        for bad in [
            "garbage",
            "",
            "2026.11.26",
            "-13",
            "2026.11.26-",
            "2026.11.26-+1",
        ] {
            assert!(!is_newer(bad, "2026.11.26-13"), "{bad:?} 不该赢");
            assert!(!is_newer("2026.11.26-13", bad), "不该和不认识的 {bad:?} 比");
            assert_eq!(compare(bad, "2026.11.26-13"), Ordering::Equal);
        }
    }

    #[test]
    fn prerelease_suffixes_do_not_order() {
        // 版本号只有“日期-序号”，没有预发布语义；带后缀的串按不认识处理。
        assert_eq!(
            compare("2026.11.26-14-rc1", "2026.11.26-14"),
            Ordering::Equal
        );
        assert!(!is_newer("2026.11.26-14-rc1", "2026.11.26-13"));
    }

    #[test]
    fn different_number_of_date_parts_compares_by_length() {
        // Go 版逐段比完再比段数，这里用 Vec 的序比较复现同一规则。
        // 注意每一串都必须带 `-序号`，否则整串按“不认识”处理（见下一条用例）。
        assert_eq!(compare("2026.11-1", "2026.11.0-1"), Ordering::Less);
        assert_eq!(compare("2026.11.0-1", "2026.11-1"), Ordering::Greater);
        assert_eq!(compare("2026.11.1-1", "2026.11-1"), Ordering::Greater);
        assert_eq!(compare("2026.11.26-1", "2026.11.26.1-1"), Ordering::Less);
        // 少了序号就是另一件事：Go 与 Rust 都判“不认识 → 相等”。
        assert_eq!(compare("2026.11", "2026.11.0"), Ordering::Equal);
    }

    #[test]
    fn leading_zeros_are_the_same_number() {
        assert_eq!(compare("2026.11.26-013", "2026.11.26-13"), Ordering::Equal);
        assert_eq!(compare("02026.11.26-13", "2026.11.26-13"), Ordering::Equal);
    }
}
