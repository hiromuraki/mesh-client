//! 设置页的两个动作：注册/保存设置、删除本地配置。
//!
//! 注册是唯一需要 token 的路径；`device_id` 由客户端先随机生成、服务端接受后以响应为准。

use crate::clock;
use crate::run::Engine;
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
    ///
    /// 注册分两半：**服务端接受**（token 就此用掉）与**本地激活**。只有前一半
    /// 能退回设置页重填 token；后一半失败只记录，身份已经在盘上，连接循环按同一份
    /// 身份继续重试。
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
            // 注册成功即已消耗 token：身份必须立刻落盘。
            self.bind(config.clone(), current.as_ref())?;
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
        self.bind(config, current.as_ref())?;
        self.logf(
            "app",
            mesh_text::get(mesh_text::id::ENGINE_LOG_SETTINGS_SAVED),
        );
        Ok(())
    }

    /// 把身份写盘并交给事件循环。
    ///
    /// 换代次（控制面地址或设备 id 变了）时先确认旧数据面停下，再让旧一代的
    /// `config.toml` 与 `credential.json` 从可读位置消失，最后原子写入新
    /// `client.json`：任何中断点之后，本地只可能读到旧一代、空、或新一代，不会出现
    /// 「新身份配旧配置」。删除或停止失败就是这次保存失败——宁可让用户重试，也不留
    /// 一代混合数据，更不留一个还在旧网络上跑着的进程。
    fn bind(
        &mut self,
        config: ClientConfig,
        previous: Option<&ClientConfig>,
    ) -> Result<(), String> {
        if generation_changed(previous, &config) {
            // 停旧数据面是换绑的第一步：下面删的是「下一次启动读哪份配置」，
            // 不是「已经连着旧网络的那个进程」。停不掉就整个换绑失败，旧一代原样留着。
            self.stop_core()?;
            self.store.remove_config().map_err(|err| {
                mesh_text::format(mesh_text::id::ENGINE_TEXT_DELETE_CONFIG_FAILED, &[&err])
            })?;
            self.store.remove_credential().map_err(|err| {
                mesh_text::format(
                    mesh_text::id::ENGINE_TEXT_REMOVE_OLD_CREDENTIAL_FAILED,
                    &[&err],
                )
            })?;
            // 旧一代的发布授权随旧一代一起作废；删除失败时它还在，下一轮心跳照常。
            self.clear_stale_update();
            // 认证失败计数记的是「这一代身份被拒绝了几次」：换一代就从零开始，
            // 否则上一代控制面的 401 会记到新身份头上，凑满 3 次就清掉刚换好的身份。
            self.reset_auth_fails();
        }
        self.store.save_client(&config).map_err(|err| {
            mesh_text::format(mesh_text::id::ENGINE_TEXT_SAVE_SETTINGS_FAILED, &[&err])
        })?;
        self.client = Some(config);
        self.refresh_identity();
        self.local_ip.clear();
        self.last_error.clear();
        self.fault.clear();
        self.rebind();
        // 设备名、身份或凭据变了就是另一份规格：这里统一换进程，不再单开一条改名路径。
        self.ensure_core_or_report();
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
        self.store.remove_credential().map_err(|err| {
            mesh_text::format(
                mesh_text::id::ENGINE_TEXT_REMOVE_OLD_CREDENTIAL_FAILED,
                &[&err],
            )
        })?;
        self.clear_stale_update();
        self.refresh_identity();
        self.local_ip.clear();
        self.core_version.clear();
        self.network_name.clear();
        self.last_error.clear();
        self.fault.clear();
        self.unbind();
        self.logf(
            "app",
            mesh_text::get(mesh_text::id::ENGINE_LOG_LOCAL_CONFIG_DELETED),
        );
        Ok(())
    }
}

/// 换代次：控制面地址或设备 id 变了。
///
/// 凭据与配置都属于一个代次，换一处就必须把另一处也作废——只比设备 id 不够：
/// id 是客户端生成的，同一台机器在两个控制面上可以是同一个 id，而那张凭据与
/// 那个网络毫无关系。
fn generation_changed(previous: Option<&ClientConfig>, next: &ClientConfig) -> bool {
    match previous {
        Some(previous) => {
            previous.device_id != next.device_id || previous.server_url != next.server_url
        }
        // 首次注册没有上一代可作废：磁盘上若还有别的残留，那是人工留下的，
        // 归「删除本地配置」处理，不在这里顺手删。
        None => false,
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
