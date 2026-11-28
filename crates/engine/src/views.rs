//! 对外发布的只读视图：状态快照与心跳输入。
//!
//! 这些结构就是面板 JSON 的形状（对应 Go 的 `engine/views.go` 与
//! `client/frontend/src/api/types.ts`），所以字段名用 camelCase、字段集合逐条对齐。

use crate::autostart;
use crate::run::Engine;
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

/// 设置页需要的状态。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub status: String,
    pub server_url: String,
    pub device_id: String,
    /// 本机设备公钥（标准 base64）。管理员把它填进「新建设备」，认领就靠它。
    pub pubkey: String,
    pub hostname: String,
    /// 配置里保存的设备名；空表示用本机主机名。
    pub hostname_set: String,
    pub config_hash: String,
    /// 实际使用的 core 路径。
    pub core_path_used: String,
    pub core_running: bool,
    pub core_pid: u32,
    pub core_version: String,
    /// 本代 core 拉起的时间（unix 秒），0 表示还没起来。
    pub core_started_at: i64,
    pub local_ip: String,
    /// STUN 探测到的公网出口地址；core 还没探完时是空数组（不是 null）。
    pub public_ips: Vec<String>,
    pub nat_type_udp: String,
    pub nat_type_tcp: String,
    pub network_name: String,
    /// `formal`（节点私钥）/ `credential`（凭据），空表示还没有身份。
    pub auth_mode: String,
    pub credential_expiry: i64,
    pub config_updated_at: i64,
    pub encryption: String,
    pub last_heartbeat: i64,
    pub last_error: String,
    pub app_version: String,
    /// 空 / `downloading` / `ready` / `error`。
    pub update_state: String,
    pub update_version: String,
    pub update_notes: String,
    pub update_error: String,
    pub autostart_supported: bool,
    pub autostart_enabled: bool,
}

/// 心跳线程组请求要的参数。
#[derive(Debug, Clone, Default)]
pub struct BeatInput {
    /// 每次现读现算：外部改坏 `config.toml` 也能被服务端发现并纠正。
    pub config_hash: String,
    /// core **实际加载**的那张凭据（F1 的修法），不是磁盘上刚写好的那张。
    pub credential_id: String,
    pub app_version: String,
}

/// 引擎发布给其它线程的只读视图。
///
/// 除快照外还带两样别的线程要用、但不该回问引擎的东西：心跳线程要的三个参数，以及
/// 面板自己查 core 时需要的 rpc 地址与代次（`rpc` 为 `None` 表示本代 core 没在跑，
/// 面板直接返回空节点表，与 Go 的 `Dashboard` 在 core 未运行时一致）。
#[derive(Debug, Clone, Default)]
pub struct Published {
    pub snapshot: Snapshot,
    pub beat: BeatInput,
    pub rpc: Option<SocketAddr>,
    /// 本代 core 的代次（引擎分配，每起一代加一）；0 表示还没有 core 进过。
    ///
    /// 与 `rpc` 一起构成「哪个 core 实例」：重启后即使复用同一个回环端口，代次也不同，
    /// 面板的查询缓存按这一对值绑定，core 换代即失效。
    pub core_gen: u64,
}

/// 跨线程共享的全部可变状态：一个 `RwLock<Arc<Published>>`。
///
/// 引擎每处理完一条消息换一次 `Arc`；面板与托盘只读，一次请求 clone 一个 `Arc`，
/// 不复制快照。引擎阻塞在 HTTP 或 core 命令上时，面板读的仍是上一份快照。
pub struct StateCell(RwLock<Arc<Published>>);

impl StateCell {
    pub fn new(published: Published) -> StateCell {
        StateCell(RwLock::new(Arc::new(published)))
    }

    pub fn get(&self) -> Arc<Published> {
        Arc::clone(
            &self
                .0
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// 只有引擎线程调用。
    pub fn publish(&self, published: Published) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(published);
    }
}

impl Engine {
    /// 当前状态快照。不查询 core、不碰网络，只读引擎自己的状态与磁盘上的摘要。
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot_with_hash(self.hash())
    }

    /// snapshot 用调用方已经算好的配置摘要。一次 publish 里快照与心跳必须用同一份
    /// 摘要：各读一次文件、各算一次，两次取样之间外部改了文件就会自相矛盾。
    pub(crate) fn snapshot_with_hash(&self, hash: String) -> Snapshot {
        let core_running = self.core_running();
        let mut snapshot = Snapshot {
            status: self.status().as_str().to_string(),
            server_url: String::new(),
            device_id: String::new(),
            // 公钥在认领之前就有：它正是「把哪台设备建进控制台」的凭据。
            pubkey: self
                .store
                .signing_key()
                .map(|key| mesh_state::public_key_base64(&key))
                .unwrap_or_default(),
            hostname: self.config.hostname.clone(),
            hostname_set: String::new(),
            config_hash: hash,
            core_path_used: self.core_path_used.clone(),
            core_running,
            core_pid: 0,
            core_version: self.core_version.clone(),
            core_started_at: 0,
            local_ip: self.local_ip.clone(),
            public_ips: self.stun.public_ips.clone(),
            nat_type_udp: self.stun.udp_nat_type.clone(),
            nat_type_tcp: self.stun.tcp_nat_type.clone(),
            network_name: self.network_name.clone(),
            auth_mode: self.identity.mode.clone(),
            credential_expiry: self.identity.cred_expiry,
            config_updated_at: self.identity.config_at,
            encryption: self.identity.encryption.clone(),
            last_heartbeat: self.last_hb,
            last_error: if self.fault.is_empty() {
                self.last_error.clone()
            } else {
                self.fault.clone()
            },
            app_version: self.config.self_version.clone(),
            update_state: self.update.phase().as_str().to_string(),
            update_version: self
                .update
                .offer
                .as_ref()
                .map(|offer| offer.version.clone())
                .unwrap_or_default(),
            update_notes: self
                .update
                .offer
                .as_ref()
                .map(|offer| offer.notes.clone())
                .unwrap_or_default(),
            update_error: self.update.error().to_string(),
            autostart_supported: autostart::SUPPORTED,
            autostart_enabled: self.autostart_on,
        };
        if let Some(core) = self.core.as_ref() {
            snapshot.core_pid = core.pid;
            snapshot.core_started_at = core.started_unix;
        }
        if let Some(client) = self.client.as_ref() {
            snapshot.server_url = client.server_url.clone();
            snapshot.device_id = client.device_id.clone();
            snapshot.hostname_set = client.hostname.clone();
            if !client.hostname.trim().is_empty() {
                snapshot.hostname = client.hostname.clone();
            }
        }
        if snapshot.last_error.is_empty() && !self.log_error.is_empty() {
            snapshot.last_error = self.log_error.clone();
        }
        snapshot
    }

    /// 发布一次快照。引擎每处理完一条消息调用它。
    ///
    /// 两个平面都恢复之后，上一次失败的原因在这里统一收口——状态本身已是推导值，
    /// 快照不再事后修正任何字段。
    pub(crate) fn publish(&mut self) {
        self.settle();
        let hash = self.hash();
        let published = Published {
            snapshot: self.snapshot_with_hash(hash.clone()),
            beat: BeatInput {
                config_hash: hash,
                credential_id: self.credential_id(),
                app_version: self.config.self_version.clone(),
            },
            rpc: self.core_rpc_addr(),
            // 与 rpc 分开取：core 还没就绪时 rpc 是 None，而代次已经是新的那一代。
            core_gen: self.core.as_ref().map(|core| core.gen).unwrap_or(0),
        };
        self.cell.publish(published);
    }
}
