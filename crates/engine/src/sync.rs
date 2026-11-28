//! 与控制面对齐：配置应用、401 处理、身份摘要。
//!
//! 一次连接与一轮心跳不再分开：请求由 [`crate::run`] 的心跳线程发出，响应统一由
//! `Engine::on_beat` 处理，这里只保留“拿到响应之后怎么改本地状态”的判断。

use crate::clock;
use crate::run::{
    config_digest, AuthMode, Engine, IdentityMeta, AUTH_CREDENTIAL, AUTH_FORMAL, NO_CREDENTIAL_ID,
};
use mesh_remote as remote;
use mesh_state::hints::{parse_hints, ConfigHints};
use mesh_state::ClientConfig;
use std::net::SocketAddr;

/// 本地状态的三种读法：读坏了、还没有配置、有配置。
pub(crate) enum Loaded {
    Failed(String),
    Missing,
    Configured(ClientConfig),
}

/// 没有凭据时 core 不起：下一轮心跳拿到就会重试。
pub(crate) fn no_credential() -> String {
    mesh_text::get(mesh_text::id::ENGINE_TEXT_NO_CREDENTIAL).to_string()
}

/// 时钟偏差上限：与服务端校验签名的时间窗一致（契约 1.3：`|now-ts| <= 300` 秒）。
const CLOCK_SKEW_LIMIT_SECONDS: i64 = 300;

/// 本机凭据的实际状态。
///
/// 「磁盘上有私钥」不等于「core 已经加载并确认它」：控制面把上报的 id 当作
/// 「设备正在用这张」的证据，确认之后才撤销旧的。把两者混成一个回退判断，
/// 一次写盘就等于宣称 core 已经跑在新凭据上，旧凭据会被提前撤销（F8）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialState {
    /// 正式节点：用 `config/secure.key`，不参与凭据签发。
    Formal,
    /// 本地没有凭据。
    Missing,
    /// 磁盘上有，但这一代 core 还没确认加载它（没起来、起不来或正在重启）。
    Held(String),
    /// core 已确认用这张凭据启动。
    Loaded(String),
}

impl CredentialState {
    /// 上报给控制面的 id。
    ///
    /// 只有 core 确认加载过的那张才算「设备在用」。持有但未确认时上报空串：
    /// 控制面对空串既不确认也不签发，正好是这一刻该做的事——既不能因为一份刚
    /// 写盘的私钥就去撤销 core 还在用的旧凭据，也不该在设备已经拿到凭据时再签一张。
    /// 正式节点同样报空串：它用 `network_secret` 认证，报 `none` 只会让服务端每轮
    /// 签一张用不上的。
    fn reported_id(&self) -> String {
        match self {
            CredentialState::Formal | CredentialState::Held(_) => String::new(),
            CredentialState::Missing => NO_CREDENTIAL_ID.to_string(),
            CredentialState::Loaded(id) => id.clone(),
        }
    }
}

impl Engine {
    /// 读 `client.json`。
    pub(crate) fn load_client(&self) -> Loaded {
        match self.store.load_client() {
            Err(err) => Loaded::Failed(err.to_string()),
            Ok(None) => Loaded::Missing,
            Ok(Some(config)) => Loaded::Configured(config),
        }
    }

    /// 应用一份配置。
    ///
    /// 文件内容不同才重写（先停旧进程，再原子写）。换不换进程由调用方的
    /// `ensure_core_or_report` 按规格决定：这里是写事实，不是收敛点，所以不自己
    /// 再算一遍规格。服务端给的 `configHash` 不参与判断：字节相同就是同一份配置，
    /// 摘要算法换过也不必让已经跑对的设备抖一下。
    pub(crate) fn apply_locked(&mut self, config: &ClientConfig, toml: &str) -> Result<(), String> {
        self.client = Some(config.clone());
        if self.store.read_config().ok().flatten().as_deref() != Some(toml) {
            // 换配置只能换进程：先停旧的，再原子写，最后由规格比较启动。
            self.stop_core()?;
            self.store.write_config(toml).map_err(|err| {
                mesh_text::format(mesh_text::id::ENGINE_TEXT_WRITE_CONFIG_FAILED, &[&err])
            })?;
            // 配置换了，认证方式、加密算法与修改时间都可能变。
            self.refresh_identity();
            self.local_ip.clear();
            self.logf(
                "app",
                &mesh_text::format(
                    mesh_text::id::ENGINE_LOG_CONFIG_APPLIED,
                    &[&short_hash(&config_digest(toml))],
                ),
            );
        }
        Ok(())
    }

    /// 应用心跳带回来的配置。
    ///
    /// 只有响应确实带配置正文时才走这里（入口条件在 `Engine::on_beat`）：服务端只在
    /// 上报的哈希对不上时才下发，所以这里不必再比一次。
    pub(crate) fn apply_remote(&mut self, heartbeat: &remote::Heartbeat) {
        let toml = match heartbeat.config_toml() {
            Ok(toml) => toml,
            Err(err) => {
                self.logf(
                    "app",
                    &mesh_text::format(mesh_text::id::ENGINE_TEXT_CONFIG_DECODE_FAILED, &[&err]),
                );
                return;
            }
        };
        let Some(config) = self.client.clone() else {
            return;
        };
        if let Err(err) = self.apply_locked(&config, &toml) {
            // 不记“这份 hash 永久失败”：写配置失败往往是瞬时的（杀软扫描、备份占用、
            // rename 撞上 Access is denied），永久放弃只会让设备静默停在旧配置上。
            self.fault = err.clone();
            self.logf("app", &err);
        }
    }

    /// 处理 401：解释原因，不改本地任何状态。
    ///
    /// 设备身份就是本机那把钥匙，401 说明控制面不认它——槽位没建、换了钥匙、或者
    /// 设备被删了。这三种都只能去控制面看：客户端自己猜不出来，也不该拿本地状态去
    /// 试错，所以这里只把原因报出来，由控制面失败退避继续重试。
    ///
    /// 唯一需要分辨的是时钟偏差：本机时间跑偏会让所有签名落在时间窗之外，那时提示
    /// 去对时比提示「钥匙不被接受」有用得多。
    pub(crate) fn report_auth_rejected(&mut self, err: &remote::Error) {
        if let remote::Error::Unauthorized {
            server_time: Some(server_time),
            ..
        } = err
        {
            let skew = clock::skew_seconds(*server_time);
            if skew.abs() > CLOCK_SKEW_LIMIT_SECONDS {
                let msg = mesh_text::format(mesh_text::id::ENGINE_TEXT_CLOCK_SKEW, &[&skew]);
                self.logf("app", &msg);
                self.last_error = msg;
                return;
            }
        }
        let detail = match err.unauthorized_code() {
            Some(code) if !code.is_empty() => format!("401 {code}"),
            _ => "401".to_string(),
        };
        let msg = mesh_text::format(mesh_text::id::ENGINE_TEXT_KEY_REJECTED, &[&detail]);
        self.logf("app", &msg);
        self.last_error = msg;
    }

    /// 重读磁盘上的 `config.toml` 与 `credential.json`，重算身份摘要与提示字段。
    ///
    /// 只在启动与状态真的变化之后调用：这里会读文件与 stat，不能挂在快照轮询上。
    pub(crate) fn refresh_identity(&mut self) {
        let hints = match self.store.read_config() {
            Ok(Some(toml)) => parse_hints(&toml),
            _ => ConfigHints::default(),
        };
        let mut next = IdentityMeta {
            encryption: hints.encryption.clone(),
            ..IdentityMeta::default()
        };
        if hints.formal() {
            next.mode = AUTH_FORMAL.to_string();
        }
        if let Ok(metadata) = std::fs::metadata(self.store.config_path()) {
            if let Ok(modified) = metadata.modified() {
                next.config_at = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs() as i64)
                    .unwrap_or(0);
            }
        }
        // 凭据是正式节点用不上的东西：本地就算还留着一张，也不能把认证方式说成凭据。
        if next.mode.is_empty() {
            if let Ok(Some(credential)) = self.store.load_credential() {
                next.mode = AUTH_CREDENTIAL.to_string();
                next.cred_expiry = credential.expires_at;
            }
        }
        self.hints = hints;
        self.identity = next;
    }

    /// 本机凭据状态：磁盘事实与 core 事实各取一次，不缓存。
    ///
    /// 两个事实不能互相顶替：磁盘上有私钥只说明「拿到了」，core 起来之后加载它
    /// 才说明「用上了」。控制面把上报的 id 当作后者（确认切换、撤销旧凭据），
    /// 所以这两种状态必须分开表达（F8）。
    pub(crate) fn credential_state(&self) -> CredentialState {
        if self.hints.formal() {
            return CredentialState::Formal;
        }
        // core 实际应用的那张规格是唯一的「已确认」来源：它是启动时按配置取的，
        // 真的就绪之后才代表 core 带着它跑起来了。
        if let Some(core) = self.core.as_ref().filter(|core| core.pid != 0) {
            if let AuthMode::Credential(id) = &core.spec.auth {
                return CredentialState::Loaded(id.clone());
            }
        }
        match self.disk_credential_id() {
            Some(id) => CredentialState::Held(id),
            None => CredentialState::Missing,
        }
    }

    /// 磁盘上那张凭据的 id；没有或读不出来都是 None。
    fn disk_credential_id(&self) -> Option<String> {
        match self.store.load_credential() {
            Ok(Some(credential)) => Some(credential.id),
            _ => None,
        }
    }

    /// 上报给控制面的凭据 id（见 [`CredentialState::reported_id`]）。
    pub(crate) fn credential_id(&self) -> String {
        self.credential_state().reported_id()
    }

    /// 把心跳带来的凭据落盘，返回是否真的换了。
    ///
    /// 服务端只在签发的那一刻带私钥，其余时候只有 id；同 id 的重复下发直接忽略。
    /// 落盘之后不需要在这里重启 core：`ensure_core` 会看到凭据 id 变了，换进程。
    ///
    /// 私钥优先取封装的那一份（[remote::Client::credential_secret]）：解不开就这一
    /// 轮作废，不写盘、不回落到明文字段，下一轮心跳会再封一份。
    pub(crate) fn save_credential(
        &mut self,
        api: &remote::Client,
        heartbeat: &remote::Heartbeat,
    ) -> bool {
        if heartbeat.credential_id.is_empty() {
            return false;
        }
        let secret = match api.credential_secret(heartbeat) {
            Ok(secret) => secret,
            Err(err) => {
                let msg =
                    mesh_text::format(mesh_text::id::ENGINE_LOG_SAVE_CREDENTIAL_FAILED, &[&err]);
                self.logf("app", &msg);
                self.last_error = msg;
                return false;
            }
        };
        if secret.is_empty() {
            return false;
        }
        if let Ok(Some(current)) = self.store.load_credential() {
            if current.id == heartbeat.credential_id {
                return false;
            }
        }
        let credential = mesh_state::Credential {
            id: heartbeat.credential_id.clone(),
            secret,
            expires_at: heartbeat.credential_expiry,
            protect: String::new(),
        };
        if let Err(err) = self.store.save_credential(&credential) {
            let msg = mesh_text::format(mesh_text::id::ENGINE_LOG_SAVE_CREDENTIAL_FAILED, &[&err]);
            self.logf("app", &msg);
            self.last_error = msg;
            return false;
        }
        self.refresh_identity();
        true
    }
}

/// core 的 RPC 监听地址：默认在 loopback 上挑一个空闲端口；`MESH_RPC_PORTAL` 指定时
/// 用它，供控制面从容器或另一台机器调用。
pub(crate) fn rpc_listen_addr() -> Result<SocketAddr, String> {
    let configured = std::env::var("MESH_RPC_PORTAL")
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty());
    match configured {
        Some(value) => value
            .parse::<SocketAddr>()
            // 解析不了就报错：这个端口能签发凭据，静默换成随机端口会让控制面的转发
            // 打到一个没人监听的地址上，表现成“签发一直超时”。
            .map_err(|err| {
                let shown = format!("{value:?}");
                mesh_text::format(
                    mesh_text::id::ENGINE_TEXT_RPC_PORTAL_INVALID,
                    &[&shown, &err],
                )
            }),
        None => Ok(mesh_etrpc::free_loopback_addr()),
    }
}

/// `MESH_RPC_WHITELIST`：非空时收窄 rpc portal 的来源。
pub(crate) fn rpc_whitelist() -> Option<String> {
    std::env::var("MESH_RPC_WHITELIST")
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
}

pub(crate) fn short_hash(hash: &str) -> String {
    if hash.len() <= 12 {
        hash.to_string()
    } else {
        hash[..12].to_string()
    }
}
