//! 文案地板：语言清单、按 id 查表与占位符替换。
//!
//! 规范见 `docs/i18n.md`。三条约定：
//!
//! - 文案只存在于 `lang/*.lang`，代码里只出现 [`id`] 的常量；
//! - [`get`] / [`format`] 的 id 参数是 `&'static str`，运行时拼出来的 key 传不进来；
//! - 当前只装载中文；英文清单只在测试里读，不进产物。

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// 中文清单：当前运行时唯一装载的表。
const CHINESE_LANG: &str = include_str!("../lang/chinese.lang");
/// 英文清单与中文同 key，运行时不装载。它不嵌进产物：那 23 KB 只为测试存在，
/// 进了 `.rodata` 就是「生产代码里留着只为测试的数据」。测试自己 `include_str!`
/// 读源文件，文件缺失同样是编译错误。
#[cfg(test)]
const ENGLISH_LANG: &str = include_str!("../lang/english.lang");

/// 首次调用时装载一次的 `key → 文案` 表。
static CHINESE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

/// 清单里的全部 id。构建期由 `build.rs` 从 `lang/chinese.lang` 生成：清单是唯一
/// 事实来源，常量名是 id 全大写、`.` 换 `_`。手写那张表时加一条文案要改四处，而
/// 漏掉 `ALL_IDS` 不会报错，偏偏它是唯一证明「id 与清单一一对应」的东西。
pub mod id {
    include!(concat!(env!("OUT_DIR"), "/ids.rs"));
}

/// 查一条文案。id 不在清单里时原样返回 id，不 panic。
pub fn get(id: &'static str) -> &'static str {
    chinese().get(id).copied().unwrap_or(id)
}

/// 按模板替换 `{0}`、`{1}`……，返回结果。
///
/// 参数个数与模板占位符对不上时，DEBUG 构建由 `debug_assert!` 报出；发布构建
/// 按缺省替换成空串，不 panic。
pub fn format(id: &'static str, args: &[&dyn core::fmt::Display]) -> String {
    let (text, counts_match) = render(get(id), args);
    debug_assert!(counts_match, "mesh-text: {id} 的占位符与参数个数不一致");
    text
}

fn chinese() -> &'static HashMap<&'static str, &'static str> {
    CHINESE.get_or_init(|| {
        parse(CHINESE_LANG).unwrap_or_else(|err| panic!("mesh-text: 内嵌中文清单解析失败: {err:?}"))
    })
}

/// 纯粹的替换：返回（结果, 占位符编号与参数个数是否一致）。
fn render(template: &str, args: &[&dyn core::fmt::Display]) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut highest: Option<usize> = None;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after.as_bytes().get(digits) == Some(&b'}') {
            let index = after[..digits].parse::<usize>().unwrap_or(usize::MAX);
            if let Some(arg) = args.get(index) {
                let _ = write!(out, "{arg}");
            }
            highest = Some(highest.map_or(index, |seen| seen.max(index)));
            rest = &after[digits + 1..];
        } else {
            // 清单里不会出现这种形态（解析阶段就拒绝）；真出现了按字面量输出。
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    let expected = highest.map_or(0, |index| index + 1);
    (out, expected == args.len())
}

/// 允许的文案类别。
const CLASSES: [&str; 5] = ["text", "log", "error", "menu", "notice"];

/// 一行里 key/value 不合法时的原因。行号从 1 起。
#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    /// 一行既不是空行/注释，也没有 `=`。
    MissingEquals { line: usize, text: String },
    /// key 不是 `<crate>.<类>.<标识>`，或含 `[a-z0-9_.]` 以外的字符。
    InvalidKey { line: usize, key: String },
    /// value 去掉首尾空白后为空。
    EmptyValue { line: usize, key: String },
    /// 同一个 key 出现两次。
    DuplicateKey { line: usize, key: String },
    /// value 里的花括号不是 `{n}`，或编号不是从 0 连续。
    BadPlaceholder {
        line: usize,
        key: String,
        value: String,
    },
}

/// 解析一份清单。key 与 value 各自 trim，第一个 `=` 是分隔符。
fn parse(text: &str) -> Result<HashMap<&str, &str>, ParseError> {
    let mut table = HashMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(ParseError::MissingEquals {
                line,
                text: trimmed.to_string(),
            });
        };
        let key = key.trim();
        let value = value.trim();
        if !valid_key(key) {
            return Err(ParseError::InvalidKey {
                line,
                key: key.to_string(),
            });
        }
        if value.is_empty() {
            return Err(ParseError::EmptyValue {
                line,
                key: key.to_string(),
            });
        }
        if !valid_placeholders(value) {
            return Err(ParseError::BadPlaceholder {
                line,
                key: key.to_string(),
                value: value.to_string(),
            });
        }
        if table.insert(key, value).is_some() {
            return Err(ParseError::DuplicateKey {
                line,
                key: key.to_string(),
            });
        }
    }
    Ok(table)
}

/// key 只允许 `[a-z0-9_.]`，且必须是非空的三段点分、类名在白名单里。
fn valid_key(key: &str) -> bool {
    if key.is_empty()
        || !key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'_'
        })
    {
        return false;
    }
    let mut parts = key.split('.');
    let (Some(crate_name), Some(class), Some(name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !crate_name.is_empty() && CLASSES.contains(&class) && !name.is_empty()
}

/// value 里的花括号必须都是 `{n}`，且用到的编号恰好是 0..k。
fn valid_placeholders(value: &str) -> bool {
    let mut used = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return false;
        }
        let after = &rest[open + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || after.as_bytes().get(digits) != Some(&b'}') {
            return false;
        }
        let Ok(index) = after[..digits].parse::<usize>() else {
            return false;
        };
        used.push(index);
        rest = &after[digits + 1..];
    }
    used.sort_unstable();
    used.dedup();
    used.iter()
        .enumerate()
        .all(|(index, &value)| index == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn english() -> HashMap<&'static str, &'static str> {
        parse(ENGLISH_LANG).expect("内嵌英文清单必须可解析")
    }

    #[test]
    fn the_two_catalogues_define_the_same_keys() {
        let zh = chinese();
        let en = english();
        assert_eq!(zh.len(), en.len(), "两个清单的条目数必须相同");
        let zh_keys: BTreeSet<&str> = zh.keys().copied().collect();
        let en_keys: BTreeSet<&str> = en.keys().copied().collect();
        assert_eq!(zh_keys, en_keys, "两个清单的 key 集合必须完全相同");
    }

    #[test]
    fn ids_and_the_chinese_table_are_one_to_one() {
        let zh = chinese();
        let listed: BTreeSet<&str> = id::ALL_IDS.iter().copied().collect();
        assert_eq!(listed.len(), id::ALL_IDS.len(), "ALL_IDS 里有重复的常量");
        let table: BTreeSet<&str> = zh.keys().copied().collect();
        assert_eq!(
            table, listed,
            "id 常量与中文清单必须一一对应（不许有孤儿条目）"
        );
    }

    #[test]
    fn catalogue_files_are_utf8_lf_without_bom() {
        for (name, text) in [
            ("chinese.lang", CHINESE_LANG),
            ("english.lang", ENGLISH_LANG),
        ] {
            assert!(!text.starts_with('\u{feff}'), "{name} 不能带 BOM");
            assert!(!text.contains('\r'), "{name} 必须用 LF 换行");
            assert!(text.ends_with('\n'), "{name} 应以换行结尾");
        }
    }

    #[test]
    fn get_returns_the_id_itself_when_it_is_missing() {
        assert_eq!(get("no.such.key"), "no.such.key");
        assert_eq!(get(id::CORE_TEXT_TIMEOUT), "超时");
    }

    #[test]
    fn format_substitutes_numbered_placeholders() {
        assert_eq!(
            format(
                id::STATE_ERROR_KEY_LENGTH,
                &[&"/tmp/client.key", &9usize, &32usize]
            ),
            "/tmp/client.key 长度异常：9（应为 32）"
        );
        assert_eq!(
            format(id::CORE_ERROR_TASKKILL_STATUS, &[&"status 1", &"detail"]),
            "taskkill 退出 status 1：detail"
        );
        assert_eq!(
            format(id::REMOTE_ERROR_API, &[&"设备数已达上限", &409u16]),
            "设备数已达上限（HTTP 409）"
        );
    }

    #[test]
    fn render_repeats_a_placeholder_without_extra_arguments() {
        let (text, matched) = render("{0}-{1}-{0}", &[&1, &2]);
        assert_eq!(text, "1-2-1");
        assert!(matched);
    }

    #[test]
    fn missing_arguments_become_empty_instead_of_panicking() {
        let (text, matched) = render("x{0}y{1}z", &[&7]);
        assert_eq!(text, "x7yz");
        assert!(!matched);
        let (text, matched) = render("{0}", &[]);
        assert_eq!(text, "");
        assert!(!matched);
        let (text, matched) = render("plain", &[&7]);
        assert_eq!(text, "plain");
        assert!(!matched);
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines() {
        let table =
            parse("# 注释\n\n  state.error.io =   {0}  读写失败：{1}  \n").expect("必须可解析");
        assert_eq!(table.len(), 1);
        assert_eq!(table["state.error.io"], "{0}  读写失败：{1}");
    }

    #[test]
    fn parse_accepts_a_valid_catalogue() {
        let table = parse("remote.error.decode = 响应解析失败：{0}\n").unwrap();
        assert_eq!(table["remote.error.decode"], "响应解析失败：{0}");
    }

    #[test]
    fn parse_rejects_a_line_without_equals() {
        assert_eq!(
            parse("state.error.io"),
            Err(ParseError::MissingEquals {
                line: 1,
                text: "state.error.io".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_a_key_with_illegal_characters_or_shape() {
        for bad in [
            "state.error.Io = x",
            "state.error.io! = x",
            "state.error.io-extra = x",
            "state.io = x",
            "state.unknown.io = x",
            ".error.io = x",
        ] {
            assert!(
                matches!(parse(bad), Err(ParseError::InvalidKey { .. })),
                "{bad:?} 必须被拒绝"
            );
        }
    }

    #[test]
    fn parse_rejects_an_empty_value() {
        assert_eq!(
            parse("state.error.io =   "),
            Err(ParseError::EmptyValue {
                line: 1,
                key: "state.error.io".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_a_duplicate_key() {
        assert_eq!(
            parse("state.error.io = a\nstate.error.io = b"),
            Err(ParseError::DuplicateKey {
                line: 2,
                key: "state.error.io".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_illegal_braces_and_numbering_gaps() {
        for bad in ["{1}", "{0} {2}", "{x}", "a } b", "{0", "{{0}}"] {
            let text = format!("state.error.io = {bad}");
            assert!(
                matches!(parse(&text), Err(ParseError::BadPlaceholder { .. })),
                "{bad:?} 必须被拒绝"
            );
        }
        assert!(parse("state.error.io = {0} {1}").is_ok());
    }
}
