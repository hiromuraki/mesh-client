use mesh_text::id;
use std::path::PathBuf;

/// 启动器只做三件事：启动、装本地包、从地址拉包装。没有别的开关。
pub enum Command {
    Launch,
    Install {
        source: Source,
        sha256: Option<String>,
        wait_for: Option<u32>,
        /// force 跳过“不比当前新”和“失败次数已满”这两道闸，只给手动修机器用。
        force: bool,
    },
}

pub enum Source {
    File(PathBuf),
    Url(String),
}

/// Options 是解析结果：要做什么，以及在哪个安装根里做。
pub struct Options {
    pub command: Command,
    /// root 显式指定安装根；不给就用启动器自己所在目录。systemd 的 ExecStart
    /// 会传它，手动排障时也用它把启动器指向别处的一份安装。
    pub root: Option<PathBuf>,
    /// data-root 显式指定数据根；不给就按平台规则推。与 `--root` 是两件事：
    /// 程序可以只读，配置与待装包必须可写。
    pub data_root: Option<PathBuf>,
}

/// 解析失败有两种：用户要帮助（正常请求），和参数真的不对。
#[derive(Debug)]
pub enum ParseError {
    Help,
    Invalid(String),
}

pub fn usage() -> String {
    let indented = |key: &'static str| format!("  {}", mesh_text::get(key));
    [
        mesh_text::get(id::LAUNCHER_TEXT_USAGE_LAUNCH).to_string(),
        indented(id::LAUNCHER_TEXT_USAGE_LAUNCH_LINE),
        String::new(),
        mesh_text::get(id::LAUNCHER_TEXT_USAGE_INSTALL_HEAD).to_string(),
        indented(id::LAUNCHER_TEXT_USAGE_INSTALL_LINE),
        String::new(),
        mesh_text::get(id::LAUNCHER_TEXT_USAGE_URL_HEAD).to_string(),
        indented(id::LAUNCHER_TEXT_USAGE_URL_LINE),
        String::new(),
        mesh_text::get(id::LAUNCHER_TEXT_USAGE_DATA_ROOT).to_string(),
    ]
    .join("\n")
}

pub fn parse(argv: &[String]) -> Result<Options, ParseError> {
    let mut root = None;
    let mut data_root = None;
    let mut file = None;
    let mut url = None;
    let mut sha = None;
    let mut wait = None;
    let mut force = false;
    let mut i = 0;
    while i < argv.len() {
        let raw = argv[i].as_str();
        let (name, inline) = match raw.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (raw, None),
        };
        match name {
            "--root" => root = Some(PathBuf::from(value(argv, &mut i, inline, name)?)),
            "--data-root" => data_root = Some(PathBuf::from(value(argv, &mut i, inline, name)?)),
            "--file" => file = Some(PathBuf::from(value(argv, &mut i, inline, name)?)),
            "--url" => url = Some(value(argv, &mut i, inline, name)?),
            "--sha256" => sha = Some(value(argv, &mut i, inline, name)?),
            "--wait-for" => {
                let v = value(argv, &mut i, inline, name)?;
                wait = Some(v.parse::<u32>().map_err(|_| {
                    ParseError::Invalid(mesh_text::format(
                        id::LAUNCHER_ERROR_WAIT_FOR_INVALID,
                        &[&v],
                    ))
                })?);
            }
            "--force" => force = true,
            "-h" | "--help" => return Err(ParseError::Help),
            other => {
                return Err(ParseError::Invalid(mesh_text::format(
                    id::LAUNCHER_ERROR_UNKNOWN_OPTION,
                    &[&other],
                )));
            }
        }
        i += 1;
    }
    let command = match (file, url) {
        (Some(_), Some(_)) => {
            return Err(ParseError::Invalid(
                mesh_text::get(id::LAUNCHER_ERROR_FILE_AND_URL).to_string(),
            ));
        }
        (Some(f), None) => Command::Install {
            source: Source::File(f),
            sha256: sha,
            wait_for: wait,
            force,
        },
        (None, Some(u)) => Command::Install {
            source: Source::Url(u),
            sha256: sha,
            wait_for: wait,
            force,
        },
        (None, None) => {
            if sha.is_some() || wait.is_some() || force {
                return Err(ParseError::Invalid(
                    mesh_text::get(id::LAUNCHER_ERROR_INSTALL_ONLY_FLAGS).to_string(),
                ));
            }
            Command::Launch
        }
    };
    Ok(Options {
        command,
        root,
        data_root,
    })
}

fn value(
    argv: &[String],
    i: &mut usize,
    inline: Option<String>,
    name: &str,
) -> Result<String, ParseError> {
    if let Some(v) = inline {
        return Ok(v);
    }
    *i += 1;
    argv.get(*i).cloned().ok_or_else(|| {
        ParseError::Invalid(mesh_text::format(
            id::LAUNCHER_ERROR_MISSING_VALUE,
            &[&name],
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_invocation_launches() {
        let opts = parse(&[]).unwrap();
        assert!(matches!(opts.command, Command::Launch));
        assert!(opts.root.is_none());
    }

    #[test]
    fn root_overrides_the_install_dir() {
        let opts = parse(&argv(&["--root", "/opt/mesh"])).unwrap();
        assert!(matches!(opts.command, Command::Launch));
        assert_eq!(
            opts.root.as_deref(),
            Some(std::path::Path::new("/opt/mesh"))
        );
    }

    #[test]
    fn file_install_accepts_inline_values() {
        match parse(&argv(&[
            "--file=p.tar.zst",
            "--wait-for=42",
            "--sha256",
            "abc",
        ]))
        .unwrap()
        .command
        {
            Command::Install {
                source,
                sha256,
                wait_for,
                force,
            } => {
                assert!(matches!(source, Source::File(ref p) if p.as_os_str() == "p.tar.zst"));
                assert_eq!(sha256.as_deref(), Some("abc"));
                assert_eq!(wait_for, Some(42));
                assert!(!force);
            }
            _ => panic!("expected install"),
        }
    }

    #[test]
    fn force_is_install_only() {
        match parse(&argv(&["--file", "p", "--force"])).unwrap().command {
            Command::Install { force, .. } => assert!(force),
            _ => panic!("expected install"),
        }
        assert!(parse(&argv(&["--force"])).is_err());
    }

    #[test]
    fn url_and_file_are_exclusive() {
        assert!(matches!(
            parse(&argv(&["--file", "a", "--url", "b"])),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn install_only_flags_are_rejected_without_a_source() {
        assert!(parse(&argv(&["--wait-for", "1"])).is_err());
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse(&argv(&["--nope"])).is_err());
    }

    #[test]
    fn help_is_its_own_case() {
        assert!(matches!(parse(&argv(&["--help"])), Err(ParseError::Help)));
        assert!(matches!(parse(&argv(&["-h"])), Err(ParseError::Help)));
    }
}
