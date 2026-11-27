//! `easytier-core` 子进程托管。
//!
//! 对应 Go 的 `internal/coreproc` 与 `engine/core.go` 的进程部分。三条硬约束：
//!
//! - `Child`、pid、代次、Job 句柄都不离开本 crate 的 core 线程；其它线程只能通过
//!   [`Cmd`] 提要求、通过 [`Event`] 收结果。
//! - 代次（`gen`）在**发起停止之前**就已经换掉：core 主动停止的退出事件不能被当成
//!   崩溃，否则会触发一次毫无必要的重拉。代次由引擎分配后放进 [`StartSpec`]，core
//!   线程原样回带——见下面的“与 MODULES.md 的差异”。
//! - `--rpc-portal` / `--rpc-portal-whitelist` 的 `MESH_RPC_PORTAL` /
//!   `MESH_RPC_WHITELIST` 覆盖、`ET_LOCAL_PRIVATE_KEY` / `ET_CREDENTIAL` 走环境变量、
//!   两把私钥都不进命令行，这些行为一条都不能改。
//!
//! # 与 MODULES.md 的差异（理由见 README）
//!
//! - [`StartSpec`] 多两个字段：`gen`（引擎分配的代号）与 `expect`（就绪判据）。
//!   MODULES 写的是“gen 归 core 线程所有”，但引擎需要判断一次 Start 的结果并区分
//!   “我主动停掉的那一代”，由引擎分配代号才能和 Start/Stop 的先后严格对上。
//! - [`spawn`] 收的是事件回调而不是 `Sender<Event>`：引擎要把 `Event` 包成
//!   `Cmd::Core`，多一个线程专门搬运不值得；回调里发的是无界 channel，不会阻塞。
//! - 就绪探测的时限与轮询间隔放在 [`Options`] 里（默认 25s / 500ms，与 Go 一致），
//!   默认入口 [`spawn`] 用默认值。

mod probe;
mod process;
mod pump;
mod thread;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

pub use mesh_etrpc::{NodeInfo, StunInfo};

/// core 线程的入队点。
///
/// `Start` 带着整份 [`StartSpec`]（几个字符串加一条路径，约 250 字节）：这条命令每次
/// 启动 core 只走一次，为它套一层 `Box` 只是多一次分配、多一个 `Box::new`，不值得。
#[allow(clippy::large_enum_variant)]
pub enum Cmd {
    /// 让这一代 core 按这份 spec 跑起来。已经在跑就先停掉——引擎只在“确实要
    /// （重新）启动”时才发它，所以替换语义比“已在运行就直接返回”更不容易留下
    /// 一个拿着旧配置的 core。
    Start(StartSpec),
    /// 停止并等进程退出；reply 里是失败原因（进程没能退出）。
    Stop { reply: Sender<Result<(), String>> },
}

/// 一次启动需要的全部输入（引擎在发 `Start` 之前就把它凑齐）。
pub struct StartSpec {
    /// 引擎分配的代号，事件原样回带。
    pub gen: u64,
    pub core_path: PathBuf,
    pub config_path: PathBuf,
    pub rpc_addr: SocketAddr,
    /// `--hostname` 的值：命令行覆盖 TOML，用户看到的设备名才算数。
    pub hostname: String,
    /// `--rpc-portal-whitelist` 的值（来自 `MESH_RPC_WHITELIST`）；空表示不加。
    pub rpc_whitelist: Option<String>,
    /// 身份二选一，由配置里有没有 `network_secret` 决定（见 DESIGN.md）。
    pub identity: Identity,
    /// 就绪判据：core 报回来的节点必须对得上，否则继续等（最多 `ready_timeout`）。
    pub expect: Expect,
}

/// 两种节点身份。两把私钥都只走环境变量，不进命令行：`/proc/<pid>/cmdline`
/// 同机可读，而它们各自等价于该节点的 mesh 身份。
pub enum Identity {
    /// 正式节点（含管理节点）：`--secure-mode` + `ET_LOCAL_PRIVATE_KEY`。
    Formal { key: String },
    /// 凭据节点：`ET_CREDENTIAL`。
    Credential { id: String, secret: String },
}

/// “core 已经起来了”的判据，与 Go 的 `identityMatches` 一致。
///
/// 只看“rpc portal 能连上”是不够的：默认端口 15888 常被同机别的 EasyTier 占着，
/// 查过去会读到**别人的**节点信息；而端口选对了、本机还没拿到虚拟 IP 时，节点信息
/// 里的地址是空的，这时候定格下来，面板会一直显示“未连接”而 core 其实早就通了。
#[derive(Debug, Clone, Default)]
pub struct Expect {
    /// 配置里写的 hostname（非空时优先比对）。
    pub hostname: String,
    /// 配置里写的 ipv4（带掩码），作为 hostname 缺失时的兜底比对。
    pub ipv4: String,
}

/// core 线程回给引擎的事件。`gen` 让引擎分辨“这一代”的退出与上一代的迟到事件。
#[derive(Debug, Clone)]
pub enum Event {
    /// 就绪探测（`ShowNodeInfo`）成功，`node` 就是那一刻的节点信息。
    Started {
        gen: u64,
        pid: u32,
        node: NodeInfo,
    },
    StartFailed {
        gen: u64,
        reason: String,
    },
    /// `err` 为 `None` 表示正常退出；`Some` 是退出码或等待失败的原因。
    Exited {
        gen: u64,
        err: Option<String>,
    },
}

/// core 线程的可调参数。默认值与 Go 的常量一致（25s / 500ms）。
#[derive(Debug, Clone)]
pub struct Options {
    /// 从进程拉起算起，最多等多久确认就绪。
    pub ready_timeout: Duration,
    /// 就绪探测的轮询间隔。
    pub ready_interval: Duration,
    /// 就绪探测单次查询的超时（Go 是 3s）。
    pub probe_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ready_timeout: Duration::from_secs(25),
            ready_interval: Duration::from_millis(500),
            probe_timeout: Duration::from_secs(3),
        }
    }
}

/// 起一个 core 代管线程，返回它的命令入口（默认 [`Options`]）。
pub fn spawn(
    events: impl Fn(Event) + Send + 'static,
    on_line: impl Fn(&str) + Send + 'static,
) -> Sender<Cmd> {
    spawn_with(events, on_line, Options::default())
}

/// 同 [`spawn`]，但可以指定就绪探测的时限。
pub fn spawn_with(
    events: impl Fn(Event) + Send + 'static,
    on_line: impl Fn(&str) + Send + 'static,
    options: Options,
) -> Sender<Cmd> {
    thread::spawn(events, on_line, options)
}

/// 按契约 2.4 的顺序查找 `easytier-core`：安装根的 `bin/` → 安装根本身 → PATH。
///
/// 返回绝对路径：core 一旦选定就不再变，之后工作目录怎么挪都不影响这一份。
pub fn locate(root: &Path) -> Result<PathBuf, String> {
    let root = absolute(root.to_path_buf());
    for candidate in [root.join("bin").join(CORE_NAME), root.join(CORE_NAME)] {
        if is_file(&candidate) {
            return Ok(candidate);
        }
    }
    if let Some(path) = search_path(CORE_NAME) {
        return Ok(absolute(path));
    }
    Err(mesh_text::format(
        mesh_text::id::CORE_ERROR_CORE_NOT_FOUND,
        &[&CORE_NAME],
    ))
}

/// 拼成绝对路径。`std::path::absolute` 不解析符号链接，也不改大小写或短名，
/// 所以面板上显示的仍是人认得的那个路径；读不到当前目录时退回原样。
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// 与 `mesh_install::platform::core_name()` 同一个名字。
///
/// 不依赖 `mesh-install` 是依赖方向的硬规则（core 是叶子能力）；一个文件名常量
/// 的两行重复，比让叶子 crate 记住“谁是安装根”便宜。
#[cfg(windows)]
const CORE_NAME: &str = "easytier-core.exe";
#[cfg(not(windows))]
const CORE_NAME: &str = "easytier-core";

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

/// 在 PATH 里找可执行文件。没有 shell 的机器上也成立，所以不调 `where`/`which`。
fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_falls_through_to_an_error_when_nothing_is_installed() {
        // 安装根是空目录，bin/ 与根下都没有 core，于是整条查找链走完只能报“找不到”
        // （PATH 上通常也没有它）。
        let empty = std::env::temp_dir().join(format!("mesh-core-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        match locate(&empty) {
            Err(err) => assert!(err.contains("找不到"), "err = {err}"),
            // 开发机上真的把 easytier-core 装进 PATH 时，这条用例就退化成交给环境。
            Ok(found) => assert!(
                found.is_absolute() && found.ends_with(CORE_NAME),
                "found = {found:?}"
            ),
        }
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn locate_prefers_bin_and_returns_an_absolute_path() {
        let root = std::env::temp_dir().join(format!("mesh-core-locate-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let core = root.join("bin").join(CORE_NAME);
        std::fs::write(&core, b"stub").unwrap();
        let found = locate(&root).unwrap();
        assert!(found.is_absolute(), "found = {found:?}");
        assert_eq!(found, core);
        let _ = std::fs::remove_dir_all(&root);
    }
}
