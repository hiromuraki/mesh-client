//! 设置页的两个动作：注册/保存设置、删除本地配置。
//!
//! 对应 Go 的 `engine/settings.go`。注册是唯一需要 token 的路径；`device_id` 由客户端
//! 先随机生成、服务端接受后以响应为准。

use crate::clock;
use crate::run::{Engine, Status};
use mesh_remote as remote;
use mesh_state::{public_key_base64, ClientConfig};
use std::time::Duration;

/// 设置页提交的表单（字段名与前端一致，线上是 camelCase）。
#[derive(Debug, Clone, Default)]
pub struct SettingsInput {
    pub server_url: String,
    pub token: String,
    pub hostname: String,
}

impl Engine {
    /// 实际使用的设备名：配置为空时用本机主机名。
    pub(crate) fn effective_hostname(&self, config: &ClientConfig) -> String {
        if config.hostname.trim().is_empty() {
            self.config.hostname.clone()
        } else {
            config.hostname.trim().to_string()
        }
    }

    /// 处理设置页提交：有 token 则注册，否则保存配置并重启连接。
    pub(crate) fn save_settings(&mut self, input: SettingsInput) -> Result<(), String> {
        let server_url = normalize_server_url(&input.server_url)?;
        // 留空表示用本机主机名，不把当前主机名固化进配置。
        let hostname = input.hostname.trim().to_string();
        let token = input.token.trim().to_string();

        let current = self.store.load_client().map_err(|err| err.to_string())?;
        let key = self.store.signing_key().map_err(|err| err.to_string())?;
        let mut config = ClientConfig {
            server_url,
            device_id: String::new(),
            hostname,
            created_at: 0,
        };
        let previous_device_id = current
            .as_ref()
            .map(|current| current.device_id.clone())
            .unwrap_or_default();
        if let Some(current) = current.as_ref() {
            config.device_id = current.device_id.clone();
            config.created_at = current.created_at;
        }

        if !token.is_empty() {
            if config.device_id.is_empty() {
                config.device_id = new_device_id();
            }
            let api = remote::Client::new(&config.server_url, &config.device_id, key.clone());
            let pubkey = public_key_base64(&key);
            let hostname_used = self.effective_hostname(&config);
            let reply = api
                .register(&token, &hostname_used, &pubkey, Duration::from_secs(30))
                .map_err(|err| register_error(&err))?;
            if !reply.device_id.is_empty() {
                config.device_id = reply.device_id;
            }
            config.created_at = clock::unix_now();
            // 注册成功即已消耗 token：身份必须立刻落盘。后面停 core / 写配置失败只
            // 影响“生效”，不能让用户拿着一个已经作废的 token 重来。
            self.store.save_client(&config).map_err(|err| {
                mesh_text::format(mesh_text::id::ENGINE_TEXT_SAVE_REGISTER_FAILED, &[&err])
            })?;
            // 设备身份变了（管理端新建了槽位、或这台机器改绑另一台设备）：上一台设备
            // 的凭据还留在磁盘上，core 会先带着它启动，而控制面要等冷却才会签发新的。
            // 丢掉旧的那份，下一轮心跳就按「没有凭据」重签。
            if previous_device_id != config.device_id {
                self.store.remove_credential().map_err(|err| {
                    mesh_text::format(
                        mesh_text::id::ENGINE_TEXT_REMOVE_OLD_CREDENTIAL_FAILED,
                        &[&err],
                    )
                })?;
            }
            self.stop_core()?;
            self.store.write_config(&reply.config_toml).map_err(|err| {
                mesh_text::format(mesh_text::id::ENGINE_TEXT_WRITE_CONFIG_FAILED, &[&err])
            })?;
            self.refresh_identity();
            self.client = Some(config.clone());
            self.local_ip.clear();
            self.set_status(Status::Connecting, String::new());
            self.logf(
                "app",
                &mesh_text::format(mesh_text::id::ENGINE_LOG_REGISTERED, &[&config.device_id]),
            );
            return Ok(());
        }

        if config.device_id.is_empty() {
            return Err(
                mesh_text::get(mesh_text::id::ENGINE_TEXT_NOT_REGISTERED_TOKEN).to_string(),
            );
        }
        self.stop_core()?;
        self.store
            .save_client(&config)
            .map_err(|err| err.to_string())?;
        self.client = Some(config);
        self.set_status(Status::Connecting, String::new());
        self.logf(
            "app",
            mesh_text::get(mesh_text::id::ENGINE_LOG_SETTINGS_SAVED),
        );
        Ok(())
    }

    /// 断开并清空本地身份与下发的配置，保留服务端地址、设备名等用户输入。
    /// 服务端上的设备记录不受影响，重新注册即可再次绑定。
    pub(crate) fn reset_config(&mut self) -> Result<(), String> {
        self.stop_core()?;
        self.store.clear_device().map_err(|err| {
            mesh_text::format(mesh_text::id::ENGINE_TEXT_CLEAR_DEVICE_FAILED, &[&err])
        })?;
        self.store.remove_config().map_err(|err| {
            mesh_text::format(mesh_text::id::ENGINE_TEXT_DELETE_CONFIG_FAILED, &[&err])
        })?;
        let _ = self.store.remove_credential();
        self.refresh_identity();
        self.client = None;
        self.local_ip.clear();
        self.core_version.clear();
        self.network_name.clear();
        self.set_status(Status::Unconfigured, String::new());
        self.logf(
            "app",
            mesh_text::get(mesh_text::id::ENGINE_LOG_LOCAL_CONFIG_DELETED),
        );
        Ok(())
    }
}

/// 规范化服务端地址：没写协议就补 `http://`，只接受 http/https，去掉末尾的 `/`。
fn normalize_server_url(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_SERVER_URL_REQUIRED).to_string());
    }
    let with_scheme = if text.contains("://") {
        text.to_string()
    } else {
        format!("http://{text}")
    };
    let (scheme, rest) = with_scheme
        .split_once("://")
        .ok_or_else(|| mesh_text::get(mesh_text::id::ENGINE_TEXT_SERVER_URL_INVALID).to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_SERVER_URL_SCHEME).to_string());
    }
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(mesh_text::get(mesh_text::id::ENGINE_TEXT_SERVER_URL_NO_HOST).to_string());
    }
    Ok(format!("{scheme}://{}", rest.trim_end_matches('/')))
}

/// 注册失败的原因，翻成用户能看懂的一句话。
fn register_error(err: &remote::Error) -> String {
    match err {
        remote::Error::Unauthorized { .. } => {
            mesh_text::get(mesh_text::id::ENGINE_TEXT_TOKEN_INVALID).to_string()
        }
        remote::Error::Api { status: 401, .. } => {
            mesh_text::get(mesh_text::id::ENGINE_TEXT_TOKEN_INVALID).to_string()
        }
        remote::Error::Api {
            status: 409,
            message,
            ..
        } => mesh_text::format(mesh_text::id::ENGINE_TEXT_REGISTER_REJECTED, &[&message]),
        remote::Error::Api { .. } => {
            mesh_text::format(mesh_text::id::ENGINE_TEXT_REGISTER_FAILED, &[&err])
        }
        _ => mesh_text::format(mesh_text::id::ENGINE_TEXT_REGISTER_CONNECT_FAILED, &[&err]),
    }
}

/// 新设备的随机 id（16 字节十六进制）。服务端接受后以响应里的 `deviceId` 为准。
fn new_device_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_gets_a_scheme_and_no_trailing_slash() {
        assert_eq!(
            normalize_server_url(" 10.126.126.1:8088/ ").unwrap(),
            "http://10.126.126.1:8088"
        );
        assert_eq!(
            normalize_server_url("https://mesh.example.com/").unwrap(),
            "https://mesh.example.com"
        );
        assert!(normalize_server_url("").is_err());
        assert!(normalize_server_url("ftp://x").is_err());
        assert!(normalize_server_url("http://").is_err());
    }

    #[test]
    fn device_ids_are_32_hex_chars() {
        let id = new_device_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(id, new_device_id());
    }
}
