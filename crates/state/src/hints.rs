//! 从下发的 `config.toml` 里读出来的提示字段。
//!
//! 这些字段不参与 core 的启动（配置文件整份交给 core 解释），只用来确认
//! “刚启动的 core 确实是这份配置对应的节点”，以及决定身份走哪条路
//! （`[network_identity]` 里有 `network_secret` = 正式节点 = `--secure-mode`）。
//!
//! 身份只看 `[network_identity]`：顶层同名键是早期配置的写法，已经不再支持。
//! 两处都认的时候，一份「子表为空、顶层有值」的合法 TOML 会让两端判出不同的
//! 身份，而身份决定客户端要不要等一张服务端根本不会签给它的凭据。
//!
//! 对应 Go 的 `engine/toml.go`。

use toml::Value;

/// `config.toml` 里界面与状态机用得上的字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigHints {
    pub hostname: String,
    /// 带掩码，形如 `10.126.126.100/24`；配置里写成数组时取第一个。
    pub ipv4: String,
    pub network_name: String,
    /// 非空表示这是一台“正式节点”：身份来自 `secure_mode` 的本地私钥，不需要凭据。
    pub network_secret: String,
    /// `[flags] encryption_algorithm` 的原文（服务端模板写的是 `chacha20`）。
    /// 老配置没有这一段时为空，界面据此决定要不要显示这一项。
    pub encryption: String,
    /// 配置读不出来的原因。解析不了就不能拿“没有 `network_secret`”的普通节点
    /// 语义承接：那会把一台正式节点当成凭据节点。
    pub parse_error: Option<String>,
}

impl ConfigHints {
    /// 用 `network_secret` 认证的节点（正式节点，含管理节点）。
    ///
    /// 正式节点一律走 `--secure-mode`：即便本地还存着一张凭据也不用——
    /// `--credential` 会顶掉 `network_secret`，把管理节点降级成凭据身份，
    /// 它发布的可信凭据列表就没人认了。
    pub fn formal(&self) -> bool {
        !self.network_secret.is_empty()
    }
}

/// 读出提示字段。解析不了时把原因记在 [`ConfigHints::parse_error`] 里，其余字段
/// 取零值：`config.toml` 的权威解释者是 core，这里读不出来只说明“提示字段未知”，
/// 由调用方决定要不要继续（启动 core 时不该继续，列表展示时可以照旧）。
pub fn parse_hints(text: &str) -> ConfigHints {
    let doc = match toml::from_str::<Value>(text) {
        Ok(doc) => doc,
        Err(err) => {
            return ConfigHints {
                parse_error: Some(err.to_string()),
                ..ConfigHints::default()
            }
        }
    };
    let network_identity = doc.get("network_identity");
    ConfigHints {
        hostname: string_at(&doc, "hostname").unwrap_or_default(),
        ipv4: ipv4_at(&doc).unwrap_or_default(),
        network_name: network_identity
            .and_then(|section| string_at(section, "network_name"))
            .unwrap_or_default(),
        network_secret: network_identity
            .and_then(|section| string_at(section, "network_secret"))
            .map(|secret| secret.trim().to_string())
            .unwrap_or_default(),
        encryption: doc
            .get("flags")
            .and_then(|flags| string_at(flags, "encryption_algorithm"))
            .map(|name| name.trim().to_string())
            .unwrap_or_default(),
        parse_error: None,
    }
}

fn string_at(table: &Value, key: &str) -> Option<String> {
    table.get(key)?.as_str().map(|value| value.to_string())
}

/// `ipv4` 可以是字符串，也可以是数组（easytier 两种都接受），只取第一个。
fn ipv4_at(doc: &Value) -> Option<String> {
    match doc.get("ipv4")? {
        Value::String(address) => Some(address.clone()),
        Value::Array(items) => items
            .iter()
            .find_map(|item| item.as_str())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_hint_from_a_real_shaped_config() {
        let text = r#"
hostname = "laptop"
ipv4 = "10.126.126.100/24"
dhcp = false

[network_identity]
network_name = "test-net"
network_secret = " s3cret "

[flags]
encryption_algorithm = "chacha20"
"#;
        let hints = parse_hints(text);
        assert_eq!(hints.hostname, "laptop");
        assert_eq!(hints.ipv4, "10.126.126.100/24");
        assert_eq!(hints.network_name, "test-net");
        // network_secret 去掉首尾空白后再判断“是不是正式节点”。
        assert_eq!(hints.network_secret, "s3cret");
        assert_eq!(hints.encryption, "chacha20");
        assert!(hints.formal());
    }

    #[test]
    fn credential_node_has_no_secret() {
        let text = r#"
ipv4 = "10.126.126.5/24"

[network_identity]
network_name = "test-net"
"#;
        let hints = parse_hints(text);
        assert!(!hints.formal());
        assert_eq!(hints.network_name, "test-net");
        assert_eq!(hints.encryption, "");
    }

    /// 顶层同名键不再算身份：与 Go 侧 `identityOf` 同一个判据。
    #[test]
    fn top_level_identity_is_not_supported() {
        let text = r#"
network_name = "legacy"
network_secret = "legacy-secret"
"#;
        let hints = parse_hints(text);
        assert_eq!(hints.network_name, "");
        assert_eq!(hints.network_secret, "");
        assert!(!hints.formal());
        assert!(hints.parse_error.is_none());
    }

    /// 子表为空、顶层有值：合法 TOML，身份仍是凭据节点。
    #[test]
    fn an_empty_section_wins_over_a_top_level_secret() {
        let text = r#"
network_secret = "top"

[network_identity]
network_name = "test-net"
network_secret = ""
"#;
        let hints = parse_hints(text);
        assert!(!hints.formal());
        assert_eq!(hints.network_name, "test-net");
    }

    #[test]
    fn ipv4_accepts_an_array() {
        let hints = parse_hints("ipv4 = [\"10.126.126.100/24\", \"fd00::1/64\"]\n");
        assert_eq!(hints.ipv4, "10.126.126.100/24");
    }

    /// 读不出来要说出来：零值字段与「没有 network_secret 的凭据节点」长得一样。
    #[test]
    fn unparsable_config_reports_the_reason() {
        let hints = parse_hints("hostname = ");
        assert!(hints.parse_error.is_some(), "解析失败必须留下原因");
        assert!(!hints.formal());
        assert!(parse_hints("").parse_error.is_none());
    }
}
