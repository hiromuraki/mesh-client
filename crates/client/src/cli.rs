//! 手写 CLI：八个开关、无子命令、无补全。
//!
//! 风格沿用启动器的 args.rs（支持 --x=y），参数名与含义照 Go 的 flag 定义。
//! 帮助文本走 stdout、退出码 0（启动器一致）；Go 的 flag 包帮到 stderr、退出码 2，
//! 这是有意的差异（见 README 的偏差表）。

use mesh_text::id;
use std::path::PathBuf;

/// 一次解析的结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    pub headless: bool,
    pub version: bool,
    pub root: Option<PathBuf>,
    pub server: String,
    pub token: String,
    pub name: String,
    pub disable_auto_update: bool,
    /// 旧启动器在安装交接时传的一次性标记：写 `.boot-ok-<标记>` 让它确认启动。
    /// **下一版删除。**
    pub boot_check: Option<String>,
}

/// 解析失败有两种：用户要帮助（正常请求），和参数真的不对。
#[derive(Debug)]
pub(crate) enum ParseError {
    Help,
    Invalid(String),
}

pub(crate) fn usage() -> String {
    let mut text = [
        mesh_text::get(id::CLIENT_TEXT_USAGE_HEAD).to_string(),
        String::new(),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_HEADLESS)),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_VERSION)),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_ROOT)),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_SERVER)),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_TOKEN)),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_NAME)),
        format!(
            "  {}",
            mesh_text::get(id::CLIENT_TEXT_USAGE_DISABLE_AUTO_UPDATE)
        ),
        format!("  {}", mesh_text::get(id::CLIENT_TEXT_USAGE_HELP)),
    ]
    .join("\n");
    text.push_str("\n\n");
    text.push_str(&mesh_text::format(
        id::CLIENT_TEXT_USAGE_BUILD_VERSION,
        &[&env!("MESH_CLIENT_BUILD_VERSION")],
    ));
    text
}

pub(crate) fn parse(argv: &[String]) -> Result<Options, ParseError> {
    let mut options = Options::default();
    let mut index = 0;
    while index < argv.len() {
        let raw = argv[index].as_str();
        if raw == "--" {
            // 与 Go 的 flag 一样：-- 之后都是位置参数，而这里没有位置参数。
            if index + 1 < argv.len() {
                return Err(ParseError::Invalid(mesh_text::format(
                    id::CLIENT_ERROR_EXTRA_ARGS,
                    &[&argv[index + 1..].join(" ")],
                )));
            }
            break;
        }
        if !raw.starts_with('-') || raw == "-" {
            return Err(ParseError::Invalid(mesh_text::format(
                id::CLIENT_ERROR_EXTRA_ARGS,
                &[&argv[index..].join(" ")],
            )));
        }
        let name = raw.trim_start_matches('-');
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (name, None),
        };
        match name {
            "h" | "help" => return Err(ParseError::Help),
            "version" => options.version = boolean(inline, name)?,
            "headless" => options.headless = boolean(inline, name)?,
            "disable-auto-update" => options.disable_auto_update = boolean(inline, name)?,
            "root" => options.root = Some(PathBuf::from(value(argv, &mut index, inline, name)?)),
            "server" => options.server = value(argv, &mut index, inline, name)?,
            "token" => options.token = value(argv, &mut index, inline, name)?,
            "name" => options.name = value(argv, &mut index, inline, name)?,
            // 一次性兼容：旧启动器在安装交接时会传 `--boot-check <标记>`，它在等
            // `.boot-ok-<标记>` 这个文件；不写它，旧启动器会判定新版起不来并回滚。
            // **下一版删除。**
            "boot-check" => options.boot_check = Some(value(argv, &mut index, inline, name)?),
            other => {
                return Err(ParseError::Invalid(mesh_text::format(
                    id::CLIENT_ERROR_UNKNOWN_OPTION,
                    &[&other],
                )));
            }
        }
        index += 1;
    }
    Ok(options)
}

fn value(
    argv: &[String],
    index: &mut usize,
    inline: Option<String>,
    name: &str,
) -> Result<String, ParseError> {
    if let Some(value) = inline {
        return Ok(value);
    }
    *index += 1;
    argv.get(*index).cloned().ok_or_else(|| {
        ParseError::Invalid(mesh_text::format(id::CLIENT_ERROR_MISSING_VALUE, &[&name]))
    })
}

/// 布尔开关；--flag=false 也认（Go 的 flag 支持 strconv.ParseBool 的那几种写法）。
fn boolean(inline: Option<String>, name: &str) -> Result<bool, ParseError> {
    match inline {
        None => Ok(true),
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Ok(true),
            "false" | "f" | "0" => Ok(false),
            _ => Err(ParseError::Invalid(mesh_text::format(
                id::CLIENT_ERROR_BOOLEAN_INVALID,
                &[&name, &raw],
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn bare_invocation_uses_defaults() {
        let options = parse(&[]).unwrap();
        assert_eq!(options, Options::default());
    }

    #[test]
    fn all_flags_are_recognized() {
        let options = parse(&argv(&[
            "--headless",
            "--root",
            "S:\\mesh",
            "--server",
            "http://10.8.0.1:8088",
            "--token=t",
            "--name",
            "laptop",
            "--disable-auto-update",
        ]))
        .unwrap();
        assert!(options.headless);
        assert_eq!(options.root, Some(PathBuf::from("S:\\mesh")));
        assert_eq!(options.server, "http://10.8.0.1:8088");
        assert_eq!(options.token, "t");
        assert_eq!(options.name, "laptop");
        assert!(options.disable_auto_update);
    }

    #[test]
    fn single_dash_flags_work_like_go() {
        let options = parse(&argv(&["-headless", "-root", "x"])).unwrap();
        assert!(options.headless);
        assert_eq!(options.root, Some(PathBuf::from("x")));
    }

    #[test]
    fn boolean_flags_accept_explicit_values() {
        assert!(!parse(&argv(&["--headless=false"])).unwrap().headless);
        assert!(!parse(&argv(&["--headless=0"])).unwrap().headless);
        assert!(parse(&argv(&["--headless=TRUE"])).unwrap().headless);
        assert!(parse(&argv(&["--headless=maybe"])).is_err());
    }

    #[test]
    fn missing_values_are_rejected() {
        assert!(matches!(
            parse(&argv(&["--root"])),
            Err(ParseError::Invalid(message)) if message.contains("缺值")
        ));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        assert!(matches!(
            parse(&argv(&["--nope"])),
            Err(ParseError::Invalid(message)) if message.contains("未知参数")
        ));
    }

    #[test]
    fn positional_arguments_are_rejected() {
        assert!(matches!(
            parse(&argv(&["extra"])),
            Err(ParseError::Invalid(message)) if message.contains("多余的参数")
        ));
        assert!(matches!(
            parse(&argv(&["--headless", "extra", "more"])),
            Err(ParseError::Invalid(message)) if message.contains("extra more")
        ));
    }

    #[test]
    fn help_is_its_own_case() {
        assert!(matches!(parse(&argv(&["--help"])), Err(ParseError::Help)));
        assert!(matches!(parse(&argv(&["-h"])), Err(ParseError::Help)));
    }

    #[test]
    fn usage_lists_every_flag_and_the_build_version() {
        let text = usage();
        for flag in [
            "--headless",
            "--version",
            "--root",
            "--server",
            "--token",
            "--name",
            "--disable-auto-update",
            "--help",
        ] {
            assert!(text.contains(flag), "帮助里少了 {flag}");
        }
        assert!(text.contains(env!("MESH_CLIENT_BUILD_VERSION")));
    }
}
