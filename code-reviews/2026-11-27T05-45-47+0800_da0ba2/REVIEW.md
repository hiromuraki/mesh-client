# 客户端代码审查（Rust workspace）

- **提交**：`da0ba2805dc8c01a02e30861f199845fa6fc35ec`（完整 SHA）
- **分支**：`main`（`## main...local/main [ahead 1]`）
- **审查时间**：2026-11-27T05:45:47+08:00（Asia/Shanghai）
- **审查范围**：`client/` 全仓库——`crates/{install,state,remote,etrpc,core,engine,panel,client,launcher}`、
  `easytier/`（钉位与补丁说明）、`.github/workflows/build.yml`、workspace 清单与 `rust-toolchain.toml`。
  **不含 `frontend/`**（Vue/TS 不在本次范围）。
- **审查开始时的工作区状态**：`git status --porcelain=v1 -b` = `## main...local/main [ahead 1]`，
  无未提交改动；`cargo test --all` 等命令没有留下被跟踪文件的改动。本轮只新增本目录，
  未修改生产代码与测试代码，未 `git add`、未 `git commit`。
- **判据文档**：`client/README.md`、`client/MODULES.md`、`client/PLAN.md`、mesh 根 `DESIGN.md`、
  `docs/contract/{client,updates,device-api}.md`、`docs/client-rust-migration.md`。
- **近 30 个提交**：逐条对照了日志降噪（`74782b7`）、CI（`29a2408`/`f403384`/`dfc1f4c`/`daaea2b`）、
  契约文档路径（`4acaaf5`）、前端迁入（`cc16efa`）、工具链钉住（`da0ba28`）所改的代码。

## 结论

没有发现 P0 / P1。共 10 条：5 条 P2、5 条 P3。建议按 F1 → F2 → F3 → F4 → F5 的顺序修：
F1–F4 都有探针复现（见下文），F5 由代码证明、但要造出触发条件只能上真机；五条改动都很小。

| 编号 | 级别 | 摘要 | 证据 |
| --- | --- | --- | --- |
| F1 | P2 | STUN 刷新线程每次重连新起一个且永不退出，线程无界增长 | 探针复现（20 次重连 +20 线程） |
| F2 | P2 | core 就绪探测期间不接受 Stop，退出/更新被拖 15s；Unix 上会留下孤儿 core | 探针复现（退出耗时 15.7s） |
| F3 | P2 | 启动器 `install::run` 不先 `recover`，直接删掉 `bin.old`，回滚材料可能被毁 | 探针复现 |
| F4 | P2 | `mesh --file` 安装失败会删掉用户给的包文件 | 探针复现 |
| F5 | P2 | Windows 启动标记在托盘初始化之前写出：托盘失败时更新已提交且无法回滚 | 代码可证 |
| F6 | P3 | CI 从不编译启动器的 Windows 代码，Windows 上也不跑测试 | 代码可证 |
| F7 | P3 | `publish` 每条消息读两次 `config.toml` 并算两次摘要，快照与心跳输入分两次取样 | 代码可证 |
| F8 | P3 | `core::stop` 失败时把 `Child` 与输出泵一起丢掉，之后可能起第二个 core | 代码可证 |
| F9 | P3 | etrpc 分片重组只限片数与单帧大小，没有总字节上限 | 代码可证 |
| F10 | P3 | `Log::tail` 是只为单测存在的生产 API | 代码可证 |

## 发现

### [P2][F1] STUN 刷新线程每次重连都新起一个且永不退出，线程数无界增长

- **位置**：[crates/engine/src/run.rs:938-956](../../crates/engine/src/run.rs#L938-L956)（`start_stun_loop`：
  循环里只有 `cell.get()` / 查询 / `sleep`，没有停止通道）；调用点
  [crates/engine/src/run.rs:358](../../crates/engine/src/run.rs#L358)。对照心跳线程在
  [run.rs:898-936](../../crates/engine/src/run.rs#L898-L936) 有 `hb_stop`，STUN 没有。
- **触发条件**：任何一次连接循环重启，即每次 `connect()` 成功之后都会调用 `start_stun_loop`。
  重启来源：面板 `/api/reconnect`（[panel/src/router.rs:384-389](../../crates/panel/src/router.rs#L384-L389)）、
  保存设置（[run.rs:477-486](../../crates/engine/src/run.rs#L477-L486)）、收到新凭据
  （[run.rs:566-568](../../crates/engine/src/run.rs#L566-L568)），以及 core 没在跑时每轮心跳
  （[run.rs:579-581](../../crates/engine/src/run.rs#L579-L581)）。
- **实际结果**：每次重连泄漏一个线程及其持有的 `Arc<StateCell>`、`Sender<Cmd>`；线程每 20 秒继续
  向引擎投递 `Cmd::Stun`。core 长期起不来（缺凭据、core 路径错、TUN 建不起来）时，心跳每 30 秒
  触发一次重启，即以 2 个/分钟的速度累积，一天约 2880 个线程；面板点几次「重新连接」每次也 +1。
  探针（`scripts/probe-stun-thread-leak.ps1`，20 次 `POST /api/reconnect`）实测线程数 20 → 40。
- **修复建议**：给 STUN 循环与心跳同样的 `Sender<()>` 停止通道（`stun_stop` 字段，`stop_stun_loop()`
  在连接循环重启与退出时调用）；更彻底的做法是去掉这个线程，把 STUN 刷新做成引擎
  `next_deadline()` 上的一个到期项——它本来就是「每 20 秒做一件小事」，不需要独立 actor。

### [P2][F2] core 就绪探测期间不接受 Stop：退出/更新被拖 15 秒，Unix 上会留下孤儿 core

- **位置**：core 线程只在主循环里取命令（[crates/core/src/thread.rs:65-82](../../crates/core/src/thread.rs#L65-L82)），
  而 `start()` 内联阻塞在就绪探测上（[thread.rs:116-176](../../crates/core/src/thread.rs#L116-L176)，
  探测调用在 [thread.rs:137-142](../../crates/core/src/thread.rs#L137-L142)），最长等
  `ready_timeout` = 25 秒（[crates/core/src/lib.rs:111-128](../../crates/core/src/lib.rs#L111-L128)，
  [crates/core/src/probe.rs:20-54](../../crates/core/src/probe.rs#L20-L54)）。
  引擎侧只等 15 秒：`STOP_REPLY_TIMEOUT`（[run.rs:38](../../crates/engine/src/run.rs#L38)）用于
  `stop_core`（[run.rs:772-801](../../crates/engine/src/run.rs#L772-L801)）与 `Shutdown` 的停 core 步骤
  （[run.rs:219-227](../../crates/engine/src/run.rs#L219-L227)）。
- **触发条件**：core 进程已经拉起、但 25 秒内没能就绪（`MESH_RPC_PORTAL` 指到一个错误地址、
  TUN 建不起来、core 卡在初始化），此时用户点退出、systemd 发 SIGTERM、或点「立即更新」。
- **实际结果**：Stop 命令在 core 线程的队列里排到探测结束，15 秒后引擎/`Shutdown` 超时放弃并
  `process::exit`。探针（`scripts/probe-shutdown-during-ready-probe.ps1`，fake core `mode=no-portal`）
  实测 `/api/quit` 回了 204（731 ms），但进程在请求后 **15.7 秒**才退出。
  Windows 上 Job 对象在进程退出时连带结束 core（探针里 core 已死），**Unix 上没有这层兜底**：
  `assign_to_job` 是空实现（[crates/core/src/process.rs:154-156](../../crates/core/src/process.rs#L154-L156)），
  core 由 `setsid()` 放进独立会话（[process.rs:139-152](../../crates/core/src/process.rs#L139-L152)），
  客户端 `process::exit` 不会给它发信号——留下一个占着 `tun` 与虚拟 IP 的孤儿 core，下一次启动撞地址。
  同一次 15 秒阻塞还会让设置页返回失败：`save_settings` 在 `stop_core()?` 处提前返回
  （[crates/engine/src/settings.rs:87](../../crates/engine/src/settings.rs#L87)），而此时注册已经成功、
  token 已被消耗（[settings.rs:69-75](../../crates/engine/src/settings.rs#L69-L75)）。
- **修复建议**：把探测移出 core 线程的命令循环（状态机里记「这一代正在探测，下一次探测在 X 秒后」），
  或在探测循环内部检查命令通道/停止标志，使 Stop 能在 100ms 级被响应；
  同时把 `STOP_REPLY_TIMEOUT` 提到探测上限之上（或让 Shutdown 在超时后仍尝试 `terminate`）。

### [P2][F3] 启动器 `install::run` 不先 `recover`：上次中断留下的 `bin.old` 被直接删除

- **位置**：[crates/launcher/src/install.rs:34-91](../../crates/launcher/src/install.rs#L34-L91)（`run` 里没有
  `recover` 调用）；`install_from_file` 在解包前无条件清掉 `bin.old`：
  [install.rs:147-151](../../crates/launcher/src/install.rs#L147-L151)。对照 `recover` 在
  [install.rs:95-105](../../crates/launcher/src/install.rs#L95-L105)，启动路径与 Linux 监督器都调了它
  （[launcher/src/main.rs:70-84](../../crates/launcher/src/main.rs#L70-L84)、
  [launcher/src/supervisor.rs:19-20](../../crates/launcher/src/supervisor.rs#L19-L20)），只有 `--file`/`--url` 安装没有。
- **触发条件**：`bin.old` 存在（= 上次安装没走完，[docs/contract/updates.md:226](../../../docs/contract/updates.md#L226) 明确这是可达状态），
  随后又执行一次 `mesh --file <包>`（手动安装、或运维手工推包）。
- **实际结果**：唯一一份好的 `bin.old` 被 `remove_dir_all` 删掉，当前的坏 `bin` 被改名成 `bin.old`；
  这次安装若再失败，`restore`（[install.rs:174-185](../../crates/launcher/src/install.rs#L174-L185)）把坏 `bin`
  换回 `bin/`，`reload_current` 再拉起这个坏版本——安装根既没有可用的新版本，也没有可回滚的旧版本。
  探针（`scripts/probe-launcher-destructive-paths.ps1` B 段：`bin`=坏、`bin.old`=好、包内清单摘要不符）
  实测：`bin.old` 不存在、`GOOD` 标记消失、`BROKEN` 回到了 `bin/`、`bin/version.json` 退回 2026.11.26-13。
- **修复建议**：`install::run` 在动任何东西之前调用 `install::recover(root)?`（与另外两个入口一致）；
  `install_from_file` 遇到已存在的 `bin.old` 应视为「必须回滚」而不是「可以删」。

### [P2][F4] `mesh --file` 安装失败会删掉用户给的包文件

- **位置**：[crates/launcher/src/install.rs:57-63](../../crates/launcher/src/install.rs#L57-L63)（摘要不符时
  `remove_file(&pkg)`）、[install.rs:66-68](../../crates/launcher/src/install.rs#L66-L68)（安装失败时同样删除）。
  而成功路径（[install.rs:82-90](../../crates/launcher/src/install.rs#L82-L90)）不删。
- **触发条件**：`mesh --file D:\pkg.tar.zst ...`，包摘要不符、解包失败或清单校验失败。
- **实际结果**：用户自己的包文件被删除；同一路径成功安装时反而保留，行为不对称、没有文档说明。
  客户端交接时这个路径是 `<root>/update.tar.zst`（删掉是对的），但 `Source::File` 同时被
  `--file` 手动安装复用。探针 A 段实测：`mesh --file <临时包> --sha256 000…` 退出码 1，包文件消失。
- **修复建议**：只在包路径等于 `<root>/update.tar.zst`（即下载下来的安装信号）时删除；
  `--file` 给的源文件一律不动。

### [P2][F5] Windows 启动标记在托盘初始化之前写出：托盘失败时更新已提交且无法回滚

- **位置**：`--boot-check` 标记在 [crates/client/src/main.rs:136-146](../../crates/client/src/main.rs#L136-L146) 写出，
  之后才进 `desktop_main`（[main.rs:148-149](../../crates/client/src/main.rs#L148-L149)）；
  托盘初始化任一失败都返回 `Err`（[crates/client/src/tray.rs:82-99](../../crates/client/src/tray.rs#L82-L99)、
  [119-124](../../crates/client/src/tray.rs#L119-L124)、[127-148](../../crates/client/src/tray.rs#L127-L148)、
  [150-154](../../crates/client/src/tray.rs#L150-L154)）。启动器一看到标记就 `commit`（删 `bin.old`）：
  [launcher/src/install.rs:190-208](../../crates/launcher/src/install.rs#L190-L208)。
- **触发条件**：`%TEMP%` 不可写或图标被安全软件拦下、`RegisterClassExW`/`CreateWindowExW` 失败等
  托盘初始化错误（`load_tray_icon` 还要写临时 `.ico`，见 [tray.rs:427-449](../../crates/client/src/tray.rs#L427-L449)）。
- **实际结果**：客户端写出启动标记 → 启动器认为新版本可用并删掉 `bin.old` → 客户端立即以失败退出。
  下一次登录同样失败，而回滚材料已经被删除，节点静默离线，只能人工推包。
  Linux 侧同类窗口小一些（`headless::run` 只在 `unconfigured` 时返回失败，
  [client/src/headless.rs:56-60](../../crates/client/src/headless.rs#L56-L60)），但道理相同。
- **修复建议**：标记在消息泵/托盘就绪之后再写（或让托盘失败降级为无托盘的 headless 常驻，
  只打日志不退出）——「能不能启动」应该包含进程形态本身，而不只是引擎与面板。

### [P3][F6] CI 从不编译启动器的 Windows 代码，Windows 上也不跑测试

- **位置**：[.github/workflows/build.yml:100-103](../../.github/workflows/build.yml#L100-L103) 的 Windows job
  只 `cargo build --release -p mesh-client`；`cargo test --all` 只出现在 Ubuntu 的 check job
  （[build.yml:28-68](../../.github/workflows/build.yml#L28-L68)）。
- **实际结果**：`crates/launcher` 里 `#[cfg(windows)]` 的安装/回滚/提权路径（
  [install.rs:189-219](../../crates/launcher/src/install.rs#L189-L219)、
  [launch.rs:25-128](../../crates/launcher/src/launch.rs#L25-L128)、
  [main.rs:70-84](../../crates/launcher/src/main.rs#L70-L84)）在 CI 里没有任何一次编译；
  Windows 专属测试（`engine` 的自启 XML、`core` 的 Job/进程组、`client` 的 CLI）也从不在 Windows 上跑。
  这些恰好是自更新与回滚的关键路径。
- **修复建议**：Windows job 增加 `cargo build --workspace --release`（或 `-p mesh-launcher`）与
  `cargo test --all`；Linux job 保持现状。

### [P3][F7] `publish` 每条消息读两次 `config.toml` 并算两次摘要

- **位置**：[crates/engine/src/views.rs:179-190](../../crates/engine/src/views.rs#L179-L190) 里
  `snapshot: self.snapshot()`（内部 [views.rs:114-115](../../crates/engine/src/views.rs#L114-L115) 调 `self.hash()`）
  与 `beat.config_hash: self.hash()` 各读一次文件、各算一次 sha256（
  [run.rs:764-769](../../crates/engine/src/run.rs#L764-L769)）。
- **实际结果**：每条命令处理完都多一次读盘 + 摘要；两次取样之间外部改文件时，快照里的
  `configHash` 与心跳上报的 `configHash` 会不同——同一事实两个来源。
- **修复建议**：`publish` 里算一次 `hash`，同时传给 `snapshot()` 与 `BeatInput`。

### [P3][F8] `core::stop` 失败时把 `Child` 与输出泵一起丢掉，之后可能起第二个 core

- **位置**：[crates/core/src/thread.rs:260-293](../../crates/core/src/thread.rs#L260-L293)：`stop` 先
  `self.running.take()`，强杀后仍等不到退出就在 [thread.rs:279](../../crates/core/src/thread.rs#L279)
  `return Err`——`Running`（`Child` 句柄 + 两个泵句柄）被丢弃，泵线程没有 join。
- **实际结果**：core 线程已不持有这个进程；引擎侧 `stop_core` 把 `CoreRun` 放回状态
  （[run.rs:789-799](../../crates/engine/src/run.rs#L789-L799)），之后任何一次 `Start` 都会再拉起一个
  core，与仍活着的那个抢同一个虚拟 IP（[thread.rs:161-162](../../crates/core/src/thread.rs#L161-L162) 的注释
  已经点明了这个后果，但错误路径没有实现对应的保护）。触发条件是强杀后 3 秒仍未退出，概率低。
- **修复建议**：`stop` 失败时保留 `Running`（不 `take`）或把 `Child`/泵交还调用方，并让引擎进入
  「core 状态未知、禁止再 Start」的状态，而不是当作已停止。

### [P3][F9] etrpc 分片重组只限片数与单帧大小，没有总字节上限

- **位置**：[crates/etrpc/src/client.rs:23-25](../../crates/etrpc/src/client.rs#L23-L25)（`MAX_PIECES`=4096）、
  [client.rs:55](../../crates/etrpc/src/client.rs#L55)（`BTreeMap<u32, Vec<u8>>`）、
  [client.rs:72-91](../../crates/etrpc/src/client.rs#L72-L91)；单帧上限 32 MiB
  （[crates/etrpc/src/frame.rs:20-23](../../crates/etrpc/src/frame.rs#L20-L23)）。
- **实际结果**：理论上限是 4096 × 32 MiB ≈ 128 GiB，重组期间没有任何总量检查。
  对端本来是本地 core，但如果 `free_loopback_addr` 释放端口后 core 没能重新绑上
  （[crates/etrpc/src/lib.rs:62-73](../../crates/etrpc/src/lib.rs#L62-L73) 有这段窗口），同一台机器上的
  任意进程可以占住那个端口喂数据。同用户进程本来就能杀客户端，所以危害有限，但「单条响应有界」
  这条应该由代码保证而不是靠对端可信。
- **修复建议**：加一个总字节上限（例如 8 MiB，与 `ListRoute` 实测 160 KB 留两个数量级余量），
  超了报 `Frame` 错误。

### [P3][F10] `Log::tail` 是只为单测存在的生产 API

- **位置**：[crates/engine/src/log.rs:99-102](../../crates/engine/src/log.rs#L99-L102)；生产代码只用
  `logs()`/`events()`（[log.rs:233-259](../../crates/engine/src/log.rs#L233-L259)），`tail` 的调用点全在
  `log.rs` 自己的单测里（350、371、398 行）。
- **实际结果**：`Log` 是库的公开类型（[lib.rs:26](../../crates/engine/src/lib.rs#L26) 重新导出），
  多出一个没有业务调用方的 pub 方法，且它读的是磁盘上的文件、与 `Log` 持有的句柄无关。
- **修复建议**：删掉，单测直接用 `logs(path, n)`；需要断言“刚写的行”时用 `logs` 亦可。

## 待确认

1. **core 派生子进程时输出泵可能永远不 EOF**。`reap()`/`stop()` 都在发 `Exited` 之前 `join` 两个泵
   （[thread.rs:101-103](../../crates/core/src/thread.rs#L101-L103)、[283-285](../../crates/core/src/thread.rs#L283-L285)），
   而泵读到 EOF 才返回。若真实 `easytier-core` 派生的子进程继承了 stdout/stderr 并长活，core 线程会卡在
   `join` 上，引擎再也收不到 `Exited`、`Stop` 也永远不回复。`fake_core` 的子进程把 stdio 设为 null，
   本地无法验证；需要看真机 core 的进程树（或给 `join` 加超时）。
2. **重新注册后旧的 `credential.json` 不清理**。`save_settings` 带 token 注册成功后只写
   `client.json` 与 `config.toml`（[settings.rs:53-81](../../crates/engine/src/settings.rs#L53-L81)），
   磁盘上仍留着上一台设备的凭据；core 会先带着它启动，直到服务端按冷却时间签发新的。
   旧凭据点是否在 mesh 里仍然有效取决于控制面，需确认这是否是有意行为。
3. **包内清单的查找方式与出包脚本的 tar 前缀**。`read_manifest` 要求条目名恰好是 `version.json`
   （[install.rs:237-255](../../crates/launcher/src/install.rs#L237-L255)），若 `build/release.ps1`
   打出的包以 `./` 开头就会报“包里没有 version.json”。出包脚本不在本仓库，未验证。
4. **启动器替换的时序**。Windows 上客户端可能在父启动器尚未退出时执行 `apply_launcher`
   （[update.rs:402-474](../../crates/engine/src/update.rs#L402-L474)）；重命名被占用只会记一行日志、
   并把该摘要记进 `launcher_tried`，要等下一个版本再试。窗口大小没有实测。
5. **`--wait-for` 用 `/proc/<pid>` 判断存活**（[launch.rs:151-164](../../crates/launcher/src/launch.rs#L151-L164)），
   pid 复用时会把别人当成旧客户端。注释已承认，仅手动安装路径受影响。
6. **`instance.lock` 的位置与文档措辞**。[docs/contract/client.md:23](../../../docs/contract/client.md#L23) 写「位于可执行文件同级」，
   而实现放在安装根（[crates/install/src/lock.rs:52-57](../../crates/install/src/lock.rs#L52-L57)，
   `Lock::path(root)` = `<root>/instance.lock`），与旧 Go 版 `main.go:96` 一致。放在根下是对的
   （`bin/` 每次更新都会被换掉），建议改文档措辞而不是改代码。
7. **RUST_LOG 的四个 target 与上游模块路径一致**（已对照 `.tmp/easytier` 的 `easytier-core/src`），
   但 `easytier-mini` 是否同样走 `easytier::common::log` 初始化没有逐行确认（补丁只改身份处理）。

## 构建与测试（实际执行）

工作目录 `S:\local\repos\mesh\client`，工具链 `cargo 1.100.0 / rustc 1.100.0`（`rust-toolchain.toml` 钉住）。

| 命令 | 结果 |
| --- | --- |
| `cargo test --all` | 通过：206 个测试，0 失败。分布：client 17、dependency_direction 2、core 8、managed_core 6、engine 63、etrpc 27、install 26、launcher 8、panel 26、remote 11、state 12 |
| `cargo clippy --all-targets -- -D warnings` | 通过（`Finished dev profile`，无警告） |
| `cargo fmt --all -- --check` | 通过（无输出，退出码 0） |
| `cargo build -p mesh-launcher` | 通过（探针需要 `target/debug/mesh.exe`） |

探针（都在 `scripts/`，只写临时目录、不需要管理员权限；运行结果记录在各条发现里）：

| 脚本 | 观测 |
| --- | --- |
| `probe-stun-thread-leak.ps1`（配合 `stub-204.ps1`） | 20 次 `POST /api/reconnect` 后客户端线程数 20 → 40 |
| `probe-shutdown-during-ready-probe.ps1` | 就绪探测期间 `/api/quit`：204 在 731 ms 返回，进程 15.7 s 后才退出 |
| `probe-launcher-destructive-paths.ps1` | A：摘要不符后用户包被删；B：`bin.old`（好副本）被删、坏 `bin` 被恢复、包被删 |

## 未验证项

- Windows 托盘的真机行为（`TaskbarCreated` 重建、UIPI、`explorer.exe` 转发、菜单交互）与 UAC 提权链路：
  需要人点 UAC，本轮未做；F5 的触发条件因此只有代码证明。
- Linux（musl / systemd）构建与运行：本机没有 musl 目标，未编译、未在 WSL 验证；F2 的孤儿 core 后果
  按代码（`assign_to_job` 空实现 + `setsid`）推断，Windows 探针只能证明 15 秒阻塞本身。
- `easytier/mini.patch` 的实际应用与内核构建：源码树在仓库外，本轮只核对了 `upstream.txt` 与 RUST_LOG 的
  target 名，未重跑 `build/easytier.ps1`。
- 控制面一侧的行为（凭据签发冷却、`appUpdateKnown` 的精简响应）：不在本仓库范围，按契约文档核对。
- 长跑内存/线程曲线：只做了单次 20 次重连的观测，没有做数小时的空转测量。