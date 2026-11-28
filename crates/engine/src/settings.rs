//! 设置页的两个动作：保存设置、删除本地配置。
//!
//! 没有「注册」这一步：设备身份就是本机的 Ed25519 公钥，管理员把它填进控制面的
//! 「新建设备」，之后每一次签名心跳本身就是认领。保存设置因此只写本地，
//! 控制面认不认这把钥匙由心跳结果反映到面板上。

use crate::clock;
use crate::run::Engine;
use mesh_state::ClientConfig;

/// 设置页提交的表单（字段名与前端一致，线上是 camelCase）。
#[derive(Debug, Clone, Default)]
pub struct SettingsInput {
    pub server_url: String,
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

    /// 处理设置页提交：写本地配置，由心跳去连。
    ///
    /// 保存只落本地：地址与设备名是这台机器的属性，控制面那边不保存它们，也就没有
    /// 需要先获得同意的写。控制面认不认这把钥匙由心跳结果显示在面板上，一次 401
    /// 不该把用户拦在设置页：他能做的只有去控制台把公钥填进槽位，而那时面板上正
    /// 好写着原因。
    pub(crate) fn save_settings(&mut self, input: SettingsInput) -> Result<(), String> {
        let server_url = normalize_server_url(&input.server_url)?;
        // 留空表示用本机主机名，不把当前主机名固化进配置。
        let hostname = input.hostname.trim().to_string();

        let current = self.store.load_client().map_err(|err| err.to_string())?;
        let mut config = ClientConfig {
            server_url,
            hostname,
            created_at: 0,
        };
        if let Some(current) = current.as_ref() {
            config.created_at = current.created_at;
        }
        // created_at 是这份配置第一次生效的时刻，之后的每次保存不改它。
        if config.created_at == 0 {
            config.created_at = clock::unix_now();
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

    /// 删除本地配置：控制面地址、设备名、下发的配置与凭据一起清掉，回到首次设置。
    ///
    /// 本机公钥不受影响，控制台上的槽位也不受影响——重新填一次地址就能再连上。
    pub(crate) fn reset_config(&mut self) -> Result<(), String> {
        self.stop_core()?;
        self.store.remove_client().map_err(|err| {
            mesh_text::format(mesh_text::id::ENGINE_TEXT_DELETE_CONFIG_FAILED, &[&err])
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
        self.reload_identity();
        self.logf(
            "app",
            mesh_text::get(mesh_text::id::ENGINE_LOG_LOCAL_CONFIG_DELETED),
        );
        Ok(())
    }
}

/// 换代次：控制面地址变了。
///
/// 凭据与配置都属于一个控制面，换地址就必须把两者一起作废——否则新控制面会拿到
/// 上一个控制面签的凭据去连一个与它无关的网络。
fn generation_changed(previous: Option<&ClientConfig>, next: &ClientConfig) -> bool {
    match previous {
        Some(previous) => previous.server_url != next.server_url,
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
}
