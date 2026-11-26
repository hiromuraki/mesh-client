//! 从下发的 `config.toml` 里读出来的提示字段。
//!
//! 这些字段不参与 core 的启动（配置文件整份交给 core 解释），只用来确认
//! “刚启动的 core 确实是这份配置对应的节点”，以及决定身份走哪条路
//! （有 `network_secret` = 正式节点 = `--secure-mode`）。
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

/// 尽力读出提示字段；读不出来就给零值，不影响 core 启动。
///
/// 这是有意与 Go 版一致的容错：`config.toml` 的唯一权威解释者是 core，
/// 客户端把整份文件交给它；这里解析失败只说明“提示字段未知”，不该挡住启动。
/// 反过来，身份选择（`formal()`）在这一刻会退化成“凭据节点”，而 core 自己
/// 仍按文件里的 `network_secret` 走安全模式——所以解析失败时的行为是保守的：
/// 不主动把正式节点降级，也不给它塞凭据（塞凭据才是降级）。
pub fn parse_hints(text: &str) -> ConfigHints {
    let Ok(doc) = toml::from_str::<Value>(text) else {
        return ConfigHints::default();
    };
    let network_identity = doc.get("network_identity");
    ConfigHints {
        hostname: string_at(&doc, "hostname").unwrap_or_default(),
        ipv4: ipv4_at(&doc).unwrap_or_default(),
        network_name: network_identity
            .and_then(|section| string_at(section, "network_name"))
            // 顶层那份只为兼容早期配置：模板与所有真实下发配置都写在
            // `[network_identity]` 段里。
            .or_else(|| string_at(&doc, "network_name"))
            .unwrap_or_default(),
        network_secret: network_identity
            .and_then(|section| string_at(section, "network_secret"))
            .or_else(|| string_at(&doc, "network_secret"))
            .map(|secret| secret.trim().to_string())
            .unwrap_or_default(),
        encryption: doc
            .get("flags")
            .and_then(|flags| string_at(flags, "encryption_algorithm"))
            .map(|name| name.trim().to_string())
            .unwrap_or_default(),
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

    #[test]
    fn top_level_identity_is_the_fallback() {
        let text = r#"
network_name = "legacy"
network_secret = "legacy-secret"
"#;
        let hints = parse_hints(text);
        assert_eq!(hints.network_name, "legacy");
        assert_eq!(hints.network_secret, "legacy-secret");
        assert!(hints.formal());
    }

    #[test]
    fn ipv4_accepts_an_array() {
        let hints = parse_hints("ipv4 = [\"10.126.126.100/24\", \"fd00::1/64\"]\n");
        assert_eq!(hints.ipv4, "10.126.126.100/24");
    }

    #[test]
    fn unparsable_config_yields_zero_values() {
        assert_eq!(parse_hints("hostname = "), ConfigHints::default());
        assert!(!parse_hints("").formal());
    }
}
