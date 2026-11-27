//! 与控制面对齐：一次连接、配置应用、401 处理、身份摘要。
//!
//! 对应 Go 的 `engine/connect|applyLocked|applyRemote|handleAuthRejected`。

use crate::clock;
use crate::run::HEARTBEAT_TIMEOUT;
use crate::run::{
    config_digest, Engine, IdentityMeta, Status, AUTH_CREDENTIAL, AUTH_FORMAL, NO_CREDENTIAL_ID,
};
use mesh_remote as remote;
use mesh_state::hints::{parse_hints, ConfigHints};
use mesh_state::ClientConfig;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

/// 本地状态的三种读法：读坏了、还没有身份、有身份。
pub(crate) enum Loaded {
    Failed(String),
    Missing,
    Configured(ClientConfig),
}

/// 一次连接尝试的失败：身份失效要回设置页，其它按退避重试。
pub(crate) enum ConnectError {
    IdentityInvalid,
    Failed(String),
}

/// 没有凭据时 core 不起：下一轮心跳拿到就会重试。
pub(crate) const NO_CREDENTIAL: &str = "尚未拿到凭据";

/// 时钟偏差上限：与服务端校验签名的时间窗一致（契约 1.3：`|now-ts| <= 300` 秒）。
const CLOCK_SKEW_LIMIT_SECONDS: i64 = 300;
/// 连续 401 的次数上限：单次可能只是时钟抖动或 nonce 撞车。
const AUTH_FAIL_LIMIT: u32 = 3;

impl Engine {
    /// 读 `client.json`。
    pub(crate) fn load_client(&self) -> Loaded {
        match self.store.load_client() {
            Err(err) => Loaded::Failed(err.to_string()),
            Ok(None) => Loaded::Missing,
            Ok(Some(config)) if config.device_id.is_empty() => Loaded::Missing,
            Ok(Some(config)) => Loaded::Configured(config),
        }
    }

    /// 打一次心跳把配置要回来：服务端在哈希对不上时直接随响应下发整份 `config.toml`，
    /// 所以启动不需要单独的取配置接口。
    pub(crate) fn connect(&mut self, config: &ClientConfig) -> Result<(), ConnectError> {
        let key = self
            .store
            .signing_key()
            .map_err(|err| ConnectError::Failed(format!("读取设备私钥失败: {err}")))?;
        let api = Arc::new(remote::Client::new(
            &config.server_url,
            &config.device_id,
            key,
        ));
        self.api = Some(Arc::clone(&api));
        let hash = self.hash();
        let credential_id = self.credential_id();
        let known = self.known_app_update();
        let result = api.heartbeat(
            &hash,
            &self.config.self_version,
            &credential_id,
            known.as_deref(),
            HEARTBEAT_TIMEOUT,
        );
        let heartbeat = match result {
            Ok(heartbeat) => heartbeat,
            Err(err) => {
                if matches!(err, remote::Error::Unauthorized { .. }) {
                    let cleared = self.handle_auth_rejected(&err);
                    if cleared {
                        return Err(ConnectError::IdentityInvalid);
                    }
                    // 不清身份就按普通连接失败退避重试，避免每次 401 都立刻重连。
                    let reason = if self.last_error.is_empty() {
                        err.to_string()
                    } else {
                        self.last_error.clone()
                    };
                    return Err(ConnectError::Failed(reason));
                }
                return Err(ConnectError::Failed(format!("连接服务端失败: {err}")));
            }
        };
        self.reset_auth_fails();
        let heartbeat = heartbeat.unwrap_or_default();
        // 首次心跳就可能带上凭据：先落盘，下面才有身份去起 core。少了这一步，第一轮
        // 必然起不来，只能等三十秒后的下一次心跳。
        if self.save_credential(&heartbeat) {
            self.logf("app", "已收到新凭据");
        }
        let (toml, hash) = if !heartbeat.config_b64.is_empty() {
            let toml = heartbeat
                .config_toml()
                .map_err(|err| ConnectError::Failed(format!("配置解码失败: {err}")))?;
            (toml, heartbeat.config_hash.clone())
        } else {
            // 心跳没带配置：服务端认为本地已是最新，直接用磁盘上那份。
            match self.store.read_config() {
                Ok(Some(local)) => {
                    let digest = config_digest(&local);
                    (local, digest)
                }
                _ => {
                    return Err(ConnectError::Failed(
                        "服务端未下发配置，本地也没有 config.toml".to_string(),
                    ))
                }
            }
        };
        self.apply_locked(config, &toml, &hash)
            .map_err(ConnectError::Failed)
    }

    /// 应用一份配置（必要时重启 core）。
    ///
    /// 与磁盘逐字节相同、或摘要对得上时不重写文件：摘要对不上但内容相同只会出现在
    /// 服务端换过摘要算法时，没必要让已经跑对的设备抖一下。
    pub(crate) fn apply_locked(
        &mut self,
        config: &ClientConfig,
        toml: &str,
        hash: &str,
    ) -> Result<(), String> {
        self.client = Some(config.clone());
        let local = self.store.read_config().ok().flatten();
        let same_hash = !hash.is_empty()
            && local
                .as_deref()
                .map(|local| config_digest(local) == hash)
                .unwrap_or(false);
        // `Some(toml)` 才算“磁盘上有且一模一样”；文件不存在时必须落盘。
        let same_content = local.as_deref() == Some(toml);
        let running = self.core_running();
        if same_hash || same_content {
            // 配置没变，core 却可能该动了：没在跑要补启动，拿着旧凭据要换进程
            // ——凭据只在启动时读一次，core 不支持热更新。
            if running && !self.credential_stale() {
                return Ok(());
            }
            if self.start_core_or_report(running) {
                self.logf(
                    "app",
                    &format!("配置未变，已启动 core {}", short_hash(hash)),
                );
            }
            return Ok(());
        }
        if running {
            self.stop_core()?;
        }
        self.store
            .write_config(toml)
            .map_err(|err| format!("写入 config.toml 失败: {err}"))?;
        // 配置换了，认证方式、加密算法与修改时间都可能变。
        self.refresh_identity();
        self.local_ip.clear();
        // 起不来就是起不来，不退回上一份：回滚只把设备留在旧配置上，服务端下一轮
        // 心跳还会把同一份新配置发下来，除了多绕一圈没有任何作用。
        self.start_core_or_report(true);
        self.logf("app", &format!("配置已应用 {}", short_hash(hash)));
        Ok(())
    }

    /// 应用心跳带回来的配置。服务端只在哈希变化时才下发，所以这里不必再比一次。
    pub(crate) fn apply_remote(&mut self, heartbeat: &remote::Heartbeat) {
        let toml = match heartbeat.config_toml() {
            Ok(toml) => toml,
            Err(err) => {
                self.logf("app", &format!("配置解码失败: {err}"));
                return;
            }
        };
        if toml.is_empty() {
            self.logf("app", "服务端报告配置已变更，但没有下发内容");
            return;
        }
        if self.api.is_none() {
            return;
        }
        let config = match self.store.load_client() {
            Ok(Some(config)) => config,
            _ => return,
        };
        if let Err(err) = self.apply_locked(&config, &toml, &heartbeat.config_hash) {
            // 不记“这份 hash 永久失败”：写配置失败往往是瞬时的（杀软扫描、备份占用、
            // rename 撞上 Access is denied），永久放弃只会让设备静默停在旧配置上。
            self.set_status(Status::Error, err.clone());
            self.logf("app", &err);
            return;
        }
        if self.core_running() {
            self.mark_online();
        }
    }

    /// 处理 401，返回是否已经清空设备身份。
    ///
    /// 服务端只在“设备身份被删除”时明确回 `revoked`，其余校验失败一律回
    /// `unauthorized`（不区分时钟偏差、nonce 冲突与签名错误）。所以：
    /// - `revoked`：身份已被服务端判定无效，直接清。
    /// - 其它：看响应的 `Date` 头。本机时间与服务端相差超过契约允许的 300 秒时只
    ///   降级提示去对时，绝不清身份——一次对时错误不该换来一次换机重注册。
    /// - 时间没有明显偏差时，连续 3 次 401 才按身份失效处理。
    pub(crate) fn handle_auth_rejected(&mut self, err: &remote::Error) -> bool {
        let code = err.unauthorized_code().unwrap_or("").to_string();
        if code == "revoked" {
            self.reset_auth_fails();
            self.handle_identity_invalid(err);
            return true;
        }
        if let remote::Error::Unauthorized {
            server_time: Some(server_time),
            ..
        } = err
        {
            let skew = clock::skew_seconds(*server_time);
            if skew.abs() > CLOCK_SKEW_LIMIT_SECONDS {
                self.reset_auth_fails();
                let msg = format!(
                    "本机时间与服务端相差 {skew} 秒，签名请求被拒（401）；请校准本机时间后重试"
                );
                self.logf("app", &msg);
                self.set_status(Status::Degraded, msg);
                return false;
            }
        }
        let failures = self.note_auth_fail();
        if failures >= AUTH_FAIL_LIMIT {
            self.logf(
                "app",
                &format!("连续 {failures} 次 401，按设备身份失效处理"),
            );
            self.reset_auth_fails();
            self.handle_identity_invalid(err);
            return true;
        }
        let msg = format!("签名请求被拒（401，第 {failures}/{AUTH_FAIL_LIMIT} 次），稍后重试");
        self.set_status(Status::Degraded, msg);
        false
    }

    fn note_auth_fail(&mut self) -> u32 {
        self.auth_fails += 1;
        self.auth_fails
    }

    pub(crate) fn reset_auth_fails(&mut self) {
        self.auth_fails = 0;
    }

    /// 服务端明确判定设备身份无效：清 `device_id`、停 core、回设置页。
    fn handle_identity_invalid(&mut self, err: &remote::Error) {
        let detail = match err.unauthorized_code() {
            Some(code) if !code.is_empty() => format!("401 {code}"),
            _ => "401".to_string(),
        };
        self.logf(
            "app",
            &format!("设备身份失效（{detail}），已清空 device_id"),
        );
        if let Err(err) = self.store.clear_device() {
            self.logf("app", &format!("清空 device_id 失败: {err}"));
        }
        let _ = self.stop_core();
        // 本地凭据这时已经作废（服务端删设备是先撤销凭据再删记录），留着只是把一份
        // 再也用不上的私钥继续放在磁盘上。
        let _ = self.store.remove_credential();
        self.refresh_identity();
        self.client = None;
        self.local_ip.clear();
        self.set_status(
            Status::Unconfigured,
            format!("设备身份已失效（{detail}），请重新获取 token 注册"),
        );
        self.trigger_restart();
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

    /// 上报给控制面的凭据 id。
    ///
    /// 正式节点报空串：它用 `network_secret` 认证，本来就不该持有凭据，报 `none` 只会
    /// 让服务端每轮签一张用不上的。core 在跑时只报它加载的那一张：先报磁盘上新写的
    /// 那张，控制面会认为设备已经换好了，立刻吊销 core 正在用的旧凭据（F1）。
    pub(crate) fn credential_id(&self) -> String {
        if self.hints.formal() {
            return String::new();
        }
        // `core_cred_id` 只在 core 启动成功后写入、停止时清空，它本身就是“core 正在用
        // 哪一张”的唯一事实来源。
        if let Some(core) = self.core.as_ref().filter(|core| core.started) {
            if !core.cred_id.is_empty() {
                return core.cred_id.clone();
            }
        }
        match self.store.load_credential() {
            Ok(Some(credential)) => credential.id,
            _ => NO_CREDENTIAL_ID.to_string(),
        }
    }

    /// 磁盘上的凭据是否已经不是 core 正在用的那张：换身份只能换进程。
    pub(crate) fn credential_stale(&self) -> bool {
        if self.hints.formal() {
            return false;
        }
        let used = match self.core.as_ref().filter(|core| core.started) {
            Some(core) => core.cred_id.clone(),
            None => return false,
        };
        if used.is_empty() {
            return false;
        }
        match self.store.load_credential() {
            Ok(Some(credential)) => credential.id != used,
            _ => false,
        }
    }

    /// 把心跳带来的凭据落盘，返回是否真的换了。
    ///
    /// 服务端只在签发的那一刻带私钥，其余时候只有 id；同 id 的重复下发直接忽略。
    pub(crate) fn save_credential(&mut self, heartbeat: &remote::Heartbeat) -> bool {
        if heartbeat.credential.is_empty() || heartbeat.credential_id.is_empty() {
            return false;
        }
        if let Ok(Some(current)) = self.store.load_credential() {
            if current.id == heartbeat.credential_id {
                return false;
            }
        }
        let credential = mesh_state::Credential {
            id: heartbeat.credential_id.clone(),
            secret: heartbeat.credential.clone(),
            expires_at: heartbeat.credential_expiry,
            protect: String::new(),
        };
        if let Err(err) = self.store.save_credential(&credential) {
            let msg = format!("保存凭据失败: {err}");
            self.logf("app", &msg);
            self.degrade(&msg);
            return false;
        }
        self.refresh_identity();
        true
    }

    /// 保存新凭据并重启 core（core 不支持热更新，换身份只能换进程）。
    pub(crate) fn apply_credential(&mut self, heartbeat: &remote::Heartbeat) -> bool {
        if !self.save_credential(heartbeat) {
            return false;
        }
        self.logf("app", "已收到新凭据，重启 core");
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
            .map_err(|err| format!("MESH_RPC_PORTAL 不是有效的监听地址 {value:?}: {err}")),
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

/// 文件的完整 sha256（十六进制）。
pub(crate) fn file_digest(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex::encode(hasher.finalize()))
}

pub(crate) fn short_hash(hash: &str) -> String {
    if hash.len() <= 12 {
        hash.to_string()
    } else {
        hash[..12].to_string()
    }
}
