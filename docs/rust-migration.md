# 从 Go 迁到 Rust 的客户端

2026-11 把客户端从 Go + Wails3 重写成 Rust（九个 crate 的 workspace）。这里是重写过程与结果里
**不该进 README 的细则**：构建环境的坑、每个 crate 与 Go 侧的对应、与设计文档的偏差、真机验收记录。

- 方案与阶段划分：[../PLAN.md](../PLAN.md)
- 模块划分与并发模型：[../MODULES.md](../MODULES.md)
- 现在怎么构建、怎么跑：[../README.md](../README.md)
- 行为契约（客户端内部）：[contract/client.md](../../docs/contract/client.md)

旧的 Go 实现还在 `../client-prototype/`，只作参考、不再构建。
## 构建环境的细节

在 `client/` 根下（PowerShell 例子，bash 同形）：

```powershell
cargo build                     # 整个 workspace（面板要 frontend/dist，见下）
cargo build --examples -p mesh-core   # 引擎/core 的用例要它当假 core，先编一次
cargo test                      # 全部单测（当前 206 个）
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check

cargo build -p mesh-launcher    # 只出启动器：target/debug/mesh.exe
cargo build -p mesh-client      # 客户端：target/debug/mesh-client.exe
.\target\debug\mesh-client.exe --help
```

`mesh-panel` 用 `include_dir!` 在**编译期**嵌入 `frontend/dist`，所以那份构建产物必须在：
目录不在时编译直接失败（这是有意的，比产出一个打开白屏的二进制好）。改了前端源码要
`cd frontend && pnpm build` 再 `cargo build`——`include_dir` 看不到目录内容的变化。

Linux 产物（WSL，源码先 rsync 到 ext4；**不要**在 `/mnt` 下编：cargo 在 drvfs 上很慢，
WSL 3.x 还会为它弹一次系统通知）：

```bash
wsl -d Ubuntu -- bash -lc 'set -e
rsync -a --delete --exclude target --exclude frontend/dist /mnt/s/local/repos/mesh/client/ ~/mb/client/
rsync -a --delete /mnt/s/local/repos/mesh/client/frontend/dist/ ~/mb/client/frontend/dist/
export PATH=/opt/workbench/app/rust/bin:$PATH
cd ~/mb/client && cargo build --release --workspace --target x86_64-unknown-linux-musl'
```

产物在 `~/mb/client/target/x86_64-unknown-linux-musl/release/`：`mesh`（启动器）与
`mesh-client`。目录相对位置与仓库里一致（`crates/panel` 的 `../../frontend/dist` 正好指到
`~/mb/client/frontend/dist`），所以编出来的就是最终形态；这只用于编译验证，改动永远发生在
`S:\local\repos\mesh\client`。

`.cargo/config.toml` 把 musl 目标的 linker 指到 `musl-gcc`（搬自旧的启动器仓库）：
不指定的话 rustc 会用宿主机的 cc 去链 glibc，出来的是照样依赖 `GLIBC_2.39` 的“静态”二进制。
**这台 Windows 上没有 musl 目标**，Linux 产物只能在 WSL 里编。

Windows 资源（图标、`requireAdministrator` 清单、VERSIONINFO）由 `crates/client/build.rs`
生成，只在 **release** 里编：debug 的 `mesh-client.exe` 不需要 UAC，正好用来在临时目录里验证
安装根与面板。`MESH_VERSION` 没设时 VERSIONINFO 用 crate 版本（`--help` 最后一行能看到）。

**不要**在这台机器上直接跑 `target/debug/mesh.exe` 或不带参数的 `mesh-client.exe`：前者是
启动器，后者会去占安装根（release 还带 requireAdministrator，必然弹 UAC）。验证用 `--help`，
或只在临时目录里用 `--root` 跑（debug 构建没有提权清单，正好适合这种验证）。

## crate 一览

| crate | 目录 | 状态 | 职责 |
| --- | --- | --- | --- |
| `mesh-install` | `crates/install` | 完成 | 安装根形状、`bin/version.json` 清单、版本序比较、失败计数、单实例锁、平台名。**不依赖 workspace 内任何 crate** |
| `mesh-state` | `crates/state` | 完成 | `config/` 下的身份与配置：`client.json`、`client.key`、`secure.key`、`credential.json`、`config.toml`（字节 + 提示字段） |
| `mesh-remote` | `crates/remote` | 完成 | 设备面 HTTP：注册、心跳与 Ed25519 签名、错误码与 `Date` 头偏差、整包下载（带上限） |
| `mesh-etrpc` | `crates/etrpc` | 完成 | core rpc portal：帧格式、手写 protobuf、`ShowNodeInfo`/`ListPeer`/`ListRoute`、分片重组 |
| `mesh-core` | `crates/core` | 完成 | `easytier-core` 子进程托管（Job、输出泵、软停/强杀、就绪探测） |
| `mesh-engine` | `crates/engine` | 主干完成 | 状态机（唯一可变状态的拥有者）：连接循环、心跳、配置同步、core 启停与退避、401、更新交接、快照与节点表 |
| `mesh-panel` | `crates/panel` | 完成 | 本地面板：回环监听与端口顺延、`Host`/同源 guard、静态资源与 SPA 兜底、10 个 `/api` 路由、仪表盘 2 秒缓存、`config/ui.json` |
| `mesh-client` | `crates/client` | 完成（Windows 托盘待真机验证） | bin `mesh-client`：手写 CLI、安装根、单实例、线程装配、headless、Win32 托盘 |
| `mesh-launcher` | `crates/launcher` | 完成 | bin `mesh`：安装、回滚、监督（搬自原来的启动器仓库，已并入本仓库） |

依赖方向（硬规则，MODULES.md 第 2 节）由 `crates/client/tests/dependency_direction.rs` 盯着：
它读每个成员的 `Cargo.toml`，多出一条不在允许表里的 `mesh-*` 依赖就让 `cargo test` 红。
表里同时约束平台专属依赖（`[target.*.dependencies]`）与 `dev-dependencies`，
否则“只在 Windows 上加一条边”就能绕开规则。

## 测试数据

`crates/etrpc/testdata/*.bin` 是**从 `client-prototype/internal/easytierrpc/testdata/` 原样拷贝**的
真实 core 应答（`ShowNodeInfo`/`ListPeer`/`ListRoute` 各一份，抓法见那边的 `capture_test.go`）。
两边用同一份二进制数据、断言同一组数值，Rust 的解码结果才有可比性；重新抓包时两边一起换。
`client/` 下没有做任何修改。

## 依赖与镜像

**本次没有使用国内镜像**：`index.crates.io` 直连正常，`Cargo.lock` 现在是 174 个包
（含 `rustls`/`ring` 等只在 Linux 目标用到的包，会被下载进 `Cargo.lock` 但不参与 Windows 编译）。
本轮新增的只有面板的 `tiny_http` + `include_dir`、Unix 的 `signal-hook`，以及已有的 `windows-sys`
扩展（托盘与控制台事件）。

以后拉包慢或超时，可以在 `~/.cargo/config.toml` 里加镜像（不要写进仓库，换机器就失效）：

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
```

用了镜像就要把“哪一个、怎么配”写进这一节，否则下一个人拉包失败时无从判断是被墙还是包没了。

## 阶段 1 实现了什么

**`mesh-core`（完整）**

- 进程托管：`--config-file <配置> --rpc-portal <addr> [--rpc-portal-whitelist <列表>] --hostname <名>
  [--secure-mode]`；`ET_LOCAL_PRIVATE_KEY` / `ET_CREDENTIAL` 走环境变量，两把私钥都不进命令行。
- 平台细节：Windows 上 `CREATE_NO_WINDOW` + kill-on-close 的 Job 对象（客户端被强杀时内核
  连带结束 core）；Unix 上 `pre_exec` 里 `setsid()`，停止时 `kill(-pid)` 打整个进程组。
- 停止：先礼（Windows `taskkill /T` 不带 `/F`，Unix `SIGTERM` 到进程组），软停命令本身失败就
  **立刻**改强杀（无窗口的 core 上那是必然），不空等；强杀再等 3s。
- 输出泵：stdout/stderr 各一条线程，按行转交；单行上限 4MiB，超限报告一句后把管道读空再停
  （否则 core 写满缓冲会永久阻塞）；行尾空白去掉、空行丢弃、行首缩进保留。
- 就绪探测：每 500ms 查一次 `ShowNodeInfo`，最多 25s；判据是 hostname 优先、ipv4 兜底，
  且**虚拟地址非空**（DHCP/固定地址还没生效时不能算就绪）。时限到了就停掉进程并报
  `StartFailed`，不留孤儿。
- 事件带 `gen`，引擎据此忽略“我主动停掉的那一代”的迟到事件。

**`mesh-engine`（主干）**

- 连接循环：读 `client.json` → 一次心跳（服务端哈希不一致就随响应下发整份配置）→ 应用配置
  → 起 core → 订阅心跳。连接失败按 10s→×1.5→60s 退避，退避期间照常处理命令
  （`recv_timeout` 就是计时器，没有额外线程）。
- 配置同步：与磁盘逐字节相同或摘要相同就不重写、不重启；内容不同则“停旧 core → 原子写
  `config.toml` → 起新 core”，失败不回滚（与 Go 一致）。
- core 守卫：退出事件按代次归并；崩溃按 1s→2×→30s 重拉，退避在**每次尝试之前**等；上一代活
  不过 5s 就继续放大档位（短命 core 不会把退避冲掉）。
- 心跳：30s 一轮、20s 超时；**进入循环不立刻打**（避免“心跳→重连→心跳”零延迟回路）；成功
  后恢复状态；401 严格照 `sync.go` 三支（`revoked` 立即清身份、时钟偏差 >300s 只降级、其余
  连续 3 次才清）。
- 身份与凭据：上报 core **实际加载**的那张凭据（F1）；正式节点报空串；换凭据才重启 core。
- 更新：`offer_update` 校验版本号/不比当前新/失败计数；Windows 先预取到 `update.tar.zst.part`
  → 校验摘要 → 改名成 `update.tar.zst`；Unix 在用户点安装时下载再交接；**服务端没给摘要一律
  拒绝**。启动器另走一路（比摘要、只试一次）。
- 面板点「立即更新」会先让进程退出，请求在回复之前就断了：前端把「连不上」当成安装信号，
  显示「正在安装新版本」并每秒探活，面板回来后自动刷新页面（60 秒没回来才提示从托盘重开）。
- 下载失败会自动重试：失败态保留，`attempts` 计数，退避 30s 起翻倍（最多 10 分钟、共 5 次），
  到期由下一轮心跳驱动重下——不另排定时器，心跳就是节拍。次数用完就停下来等人处理，
  错误行留在界面上。面板上失败态的「重试」走 `apply_update`：本地没有包时它重新下一份、
  下完直接交接，所以“重试”在“包没下完”和“包已下好但安装失败”两种失败上都成立。

### 心跳里的更新载荷瘦身（`appUpdateKnown` / `unchanged`）

心跳响应里的 `appUpdate` 以前每轮都带 version/url/notes/sha256/autoUpdate，设备不升级时这份
数据每 30 秒重复一次。现在照 `configHash` 的老办法给它加了个指纹：

- **请求**：心跳体可选带 `"appUpdateKnown": "<64 位 sha256>"`，值是**内存里**最近一份
  `appUpdate.sha256`（不是版本号：同版本重新发布时内容会变，只有摘要能表达“我手上是哪一份”）。
  没收到过就整个字段省略。**不进签名串**——它只影响下发多少内容，不影响授权。
- **响应**：指纹与发布记录一致时服务端回精简形状 `{version, autoUpdate, unchanged: true}`；
  否则回完整形状（不带 `unchanged`）。`appUpdate` 仍然只在“设备版本更低”时出现。

客户端的落点：

- 记忆就是 `UpdateState.sha256`，**没有**单开一个字段：它本来就是“内存里这份 offer 的摘要”，
  与进度（`phase`）无关。`Published.beat.app_update_known` 在每次发布快照时从它派生，心跳线程
  只读。`clear_stale_update()` 清掉 offer 时它自然变成 `None`——不存在“清了 offer 忘了清 known”
  的分叉。重启后内存为空 → 不带该字段 → 服务端回完整内容。
- `offer_update` 收到 `unchanged`：只更新 `auto_update`（管理员可能刚改过设备级覆盖），**不校验
  url、不下载、不写日志、不动 phase/version/notes/path**。精简形状里本来就没有 url，走完整
  分支的任何一步都会被当成“新下发”，把已经就绪的包重置成待下载。
- 摘要也参与去重：同一版本号、不同摘要 = 重新发布过的新包，要换掉手里的 offer；同版本同摘要
  才按“手上就是它”跳过。
- `autoUpdate` 照单全收（它已经是控制面覆盖后的值），本地 `--disable-auto-update` 仍是唯一的
  一票否决。
- 退出：`Shutdown` 注册“停 core”一步，`run_and_exit` 幂等；引擎在收尾开始后不再处理命令。
- 快照与节点表：`Snapshot` 字段逐个对齐 `frontend/src/api/types.ts`；`build_nodes` 把
  `ListPeer` + `ListRoute` 拼成数据页的行（直连用实测延迟覆盖路由表的整数毫秒、中继按
  `next_hop` 还原整条链路）。
- 日志：`HH:MM:SS [app|core] …`、4MB/2MB 高低水位裁剪（只保留完整行，Windows 上 close →
  rename → reopen）、相邻重复行合并、`logs`/`events` 两个纯函数给面板用；core 行去掉自带时间戳
  并把 `local_private_key`/`network_secret` 打码。

**`mesh-panel`（完整）**

- 回环监听：从 11020 起顺延 20 个端口，全被占才报错。地址写进 `config/ui.json`（0600、两空格
  缩进，与 Go 的 `MarshalIndent` 同形），第二次启动靠它唤起已有实例。
- guard 只罩 `/api`：非回环 `Host` → 400 `bad_host`；跨源 `Sec-Fetch-Site` → 403 `cross_origin`；
  两个头都没有（curl、脚本、托盘新标签页）放行。判据逐条搬自 `internal/uits/server_test.go`。
- 静态资源：`include_dir!` 嵌 `frontend/dist`，任何未知路径回 `index.html`；带
  `Cache-Control: no-store` 与 `X-Frame-Options: DENY`（防透明 iframe 骗点“退出”“立即更新”）。
- 路由 10 个 `/api` + 静态兜底；`GET` 的注册同时接受 `HEAD`，方法不对回 405 + `Allow`。
- 会改状态的动作全部转成 `Cmd` 并等引擎回复；`/api/quit` 先回 204、再跑退出动作。
- 仪表盘：`Published.rpc` 是 `Some` 才查 core（`list_peer`/`list_route` 并行、各 4 秒超时），
  结果缓存 2 秒；core 没在跑时 `nodes` 是 `[]` 而不是 `null`。
- 日志页与最近事件：`lines` 缺省用引擎的默认值（200 / 12），非法值回 400 `bad_lines`。

### 面板路由对齐表

| Go（`internal/uits/server.go` 的 `routes()`） | Rust（`crates/panel/src/router.rs`） | 测试覆盖（`crates/panel/src/tests.rs`） |
| --- | --- | --- |
| `GET /api/snapshot` | `GET/HEAD /api/snapshot` → 快照 JSON | `snapshot_uses_the_frontend_field_names`、`api_rejects_non_loopback_host`、`api_rejects_cross_origin_requests`、`unknown_method_is_405_with_allow_header` |
| `GET /api/dashboard` | `GET/HEAD /api/dashboard` → `{snapshot, nodes}` | `dashboard_without_a_running_core_has_an_empty_node_list`、`dashboard_with_an_unreachable_core_still_lists_the_local_node` |
| `GET /api/logs` | `GET/HEAD /api/logs?lines=N` | `logs_rejects_invalid_line_counts`、`logs_returns_the_file_tail_and_events_filter_app_lines` |
| `GET /api/events` | `GET/HEAD /api/events?lines=N` | `logs_returns_the_file_tail_and_events_filter_app_lines` |
| `PUT /api/settings` | `PUT /api/settings`（64KiB 上限） | `settings_are_forwarded_to_the_engine`、`settings_rejects_bad_json_and_too_large_bodies`、`settings_report_the_engine_error_as_400` |
| `PUT /api/autostart` | `PUT /api/autostart`（4KiB 上限） | `autostart_is_forwarded_and_errors_are_400`（引擎的拒绝在非 Windows 上，见偏差 18） |
| `POST /api/reset` | `POST /api/reset`（引擎失败 → 500） | `reset_maps_engine_failure_to_500` |
| `POST /api/update` | `POST /api/update`（等回复 ≤1s，见偏差 24） | `update_returns_the_snapshot_when_the_engine_exits_before_replying`、`update_reports_an_immediate_engine_error` |
| `POST /api/reconnect` | `POST /api/reconnect` → 204 | `reconnect_is_204_and_sends_the_command` |
| `POST /api/quit` | `POST /api/quit` → 204，响应发出后退出 | `quit_answers_204_then_runs_the_callback` |
| `GET /`（含 SPA 兜底） | `GET/HEAD /` 与其余 GET 路径 | `root_serves_the_built_page_and_refuses_framing`、`unknown_paths_fall_back_to_the_spa`、`path_traversal_is_not_served_as_a_file`、`static_is_not_guarded_like_go`、`content_types_follow_the_extension` |
| ——（Go 没有单独的监听测试） | 端口顺延 `http::listen(11020, 20)` | 无自动化用例（本机 Go 客户端常占 11020，绑固定端口会互相干扰）；用下面这条冒烟命令验证 |

冒烟命令（临时安装根，不碰真实安装、不需要管理员权限）：

```powershell
$root = Join-Path $env:TEMP 'mesh-smoke'
New-Item -ItemType Directory -Force "$root\bin","$root\config" | Out-Null
# 这份清单没有签名：只能直接跑 mesh-client，过不了启动器的验签。
'{"version":"2026.11.26-99","slot":"client","platform":"windows/amd64","files":{},"signature":""}' | Set-Content "$root\bin\version.json"
'{"server_url":"http://127.0.0.1:9","device_id":"00000000000000000000000000000000"}' | Set-Content "$root\config\client.json"
Start-Process .\target\debug\mesh-client.exe -ArgumentList '--headless',"--root",$root -PassThru
Start-Sleep 3; Get-Content "$root\config\ui.json"
# 用 ui.json 里的地址：/api/snapshot 200；Host 换成 10.126.126.100:11020 → 400；
# Sec-Fetch-Site: cross-site → 403；/logs?lines=abc → bad_lines；POST /api/quit → 204 且进程退 0。
```

**`mesh-client`（完整；Windows 托盘待真机验证）**

- 手写 CLI：`--root` `--headless` `--server` `--token` `--name` `--boot-check`
  `--disable-auto-update` `--version`；支持 `--x=y`、`-x` 与布尔值（`--headless=false`），
  多余的位置参数报“多余的参数: …”。
- 安装根：`--root`（相对路径按当前目录绝对化）；否则 `bin/` 的上一级。读不到
  `bin/version.json` 直接失败（与 Go 同一句话）。
- 单实例：`instance.lock` 拿不到时不报错退出——读 `config/ui.json` 唤起已有实例的面板；
  没有记录才回“已有实例在运行”。
- 线程装配：engine + panel + headless/桌面；`Shutdown` 里加“停面板”一步。启动失败（比如
  headless 未注册）也会先跑一遍收尾再以失败退出，Unix 上不会留下孤儿 core。
- `--boot-check` 标记：引擎与面板就位后写 `.boot-ok-<标记>`，写不进去只打 stderr。
- headless：每 300ms 把日志新增的完整行抄到控制台；SIGTERM/SIGINT 正常退出；身份失效
  （状态停在 `unconfigured`）按 Go 的同一句话报错退出。
- Windows 桌面：手写 Win32 托盘（`Shell_NotifyIconW` + 隐藏窗口消息泵），菜单六项
  （应用可更新 / 本机地址 / 打开面板 / 打开应用目录 / 开机自启 / 退出）。explorer 重启后按
  `TaskbarCreated` 重建图标；开机时客户端常比 explorer 先起来，那时 `NIM_ADD` 会返回 FALSE，
  所以加图标失败要每 500ms 重试（最多一分钟）。`CALLBACK_MESSAGE` 与 `TaskbarCreated` 经
  `ChangeWindowMessageFilterEx` 放行：客户端是最高权限进程，explorer 是中等完整性，不放行
  就会「图标看得见、点下去没反应」。图标是 PNG 压缩的 .ico，用 `LoadImageW` 读临时文件
  （与 Go 的 systray 同一做法）。浏览器与资源管理器都经 `explorer.exe` 转发（见偏差 27）。
- 开机自启（仅 Windows）：任务计划程序里的 `Mesh Client`——登录触发、`HighestAvailable`、
  `ExecutionTimeLimit=PT0S`，UTF-16 XML 交给 `schtasks /Create`；目标是安装根的 `mesh.exe`，
  不是 `bin/` 里的主程序。状态以系统里有没有这条任务为准，缓存在引擎里，只在启动、用户改动
  与每轮心跳之后刷新（查询要起一个进程，快照 2 秒一次扛不住）。其它平台没有这个能力，
  快照的 `autostartSupported` 恒为 `false`，面板与托盘都不显示入口。

**这一轮没做**：Windows 桌面形态的真机验证（托盘、UAC、自启任务只能人工核对）；
发布脚本（`build/*.ps1`）里的 `cargo` 分支。

## 阶段 1 真机验收（wsl）

在 wsl 上开了一份独立安装根 `/opt/mesh-rust`，用新注册的设备 `rust-probe`（`10.126.126.5`）
把 Linux headless 路径跑了一遍：

- **首次注册**：`--server --token --name` 写出 `config/client.json`、`client.key`，下一轮心跳拿到凭据与配置。
- **配置下发**：`config/config.toml` 落盘，内容与服务端一致（`ipv4 = "10.126.126.5/24"`）。
- **core 托管**：`easytier-core` 起在 `tun0`，日志报 `core 已就绪 pid=… ip=10.126.126.5`。
- **组网**：连上三个种子的 udp 与 tcp 通道，另有两条对端直连；`ping 10.126.126.1` 通。
- **面板**：`/api/snapshot` 返回 `status=online`、`coreRunning=true`、`localIp=10.126.126.5`；
  伪造 `Host` 被 400 挡下。
- **自更新**：客户端包与启动器都按控制面下发的版本完成了替换。

**这一轮发现并修掉的 bug**：headless 收到 SIGTERM 后 `serve` 是**正常返回**的，而 `main` 只在
`outcome.is_err()` 时调 `shutdown.run()`。于是每次 `systemctl stop` 都把 `easytier-core`
留在它自己的会话里，`tun0` 也跟着残留——下一次启动就会撞地址。现在两条路径都收尾
（`Shutdown::run` 幂等，重复调用是空转）。

**已知不一致**：控制面 `linux/amd64` 的启动器槽位还是 Go 版，Rust 客户端会照着下载并覆盖安装根里的
`mesh`。验收时要么把编译出来的启动器再覆盖回去，要么等阶段 3 换过发布槽位。

## 与 MODULES.md 的偏差

逐条写理由；没列出的都按文档实现。

1. **`mesh-install` 的依赖不是“只有 fs4”**。`Failures` 与 `bin/version.json` 都是 JSON，
   清单解析与失败计数既然合并进这个 crate，就必须带 `serde`/`serde_json`；错误枚举用
   `thiserror`（声明式实现，无运行时代码）。MODULES.md 第 8 节的依赖表把这三项漏了。
2. **`mesh-etrpc` 多一个 `thiserror`**。第 6 节要求每层用自己的错误枚举，`Error` 有五个变体，
   手写 `Display` 只会更容易写错；除此之外仍然只有 `std`（帧与 protobuf 都是手写）。
3. **`mesh-remote` 不引 `sha2`、引入 `httpdate`**。摘要（`sha256`）由启动器与引擎算，
   心跳/注册/下载都不需要它；`Date` 头要解析成 `SystemTime` 才能算时钟偏差，而 RFC 7231 的
   三种日期格式自己解析要再写一遍公历换算，算错的表现是“把正常的 401 判成时钟偏差”。
   `httpdate` 无依赖、只做这件事。
4. **`mesh-install` 多了 `manifest` 模块**。MODULES.md 只列了 `layout::read_version`，但启动器
   还要读**包内**的 `version.json`（`--file`/`--url` 与 Linux 常驻启动器的预检查）并按清单校验
   每个文件的摘要。这两处必须用同一份解析逻辑，否则“什么算合法清单”会出现两个答案。
   `layout::read_version` 在它之上再叠字符集校验。
5. **`layout::find_root(exe)` 收的是 exe 路径，Go 的 `layout.FindRoot` 收的是目录**。
   MODULES.md 的签名写的是 `exe`，这里按签名实现（父目录的父目录）；两者对同一个安装得到
   同一个根，但调用方不能把 Go 的调用方式照搬过来。`std` 没有 `filepath.Abs`，所以函数不做
   绝对化，文档要求传 `current_exe()`。
6. **`mesh-state` 增加了 `Error::EmptyRoot` 与 `public_key_base64()`**。前者是“根目录为空”的
   输入错误，塞进 `Io`/`Parse` 都会让错误文案说谎；后者对应 Go 的 `state.PublicKeyBase64`，
   注册要用，放在拥有身份的 crate 里比让每个消费者自己 base64 公钥更不容易错。
   另外补了 Go 有、MODULES.md 没列的路径访问器（`dir`/`log_dir`/`client_path`/`key_path`/
   `secure_key_path`/`credential_path`）。
7. **`Shutdown` 放在 `mesh-engine`**。MODULES.md 3.6 的 `spawn` 签名要求注入 `Arc<Shutdown>`，
   3.8 又把 `Shutdown` 算在 `mesh-client` 名下。阶段 1 的结论是留在 engine：引擎要注册“停 core”，
   client 只是构造它并追加“停面板”一步；挪到 client 会让 engine 反过来依赖 client 的退出流程。
8. **阶段 1 收口后的依赖表**：`mesh-panel` 是 `mesh-engine` + `mesh-etrpc` + `tiny_http` +
   `include_dir` + `serde`/`serde_json`；`mesh-client` 是 `mesh-install`/`mesh-state`/`mesh-engine`/
   `mesh-panel`，Windows 上再加 `windows-sys`（托盘、控制台事件）、Unix 上加 `signal-hook`，
   build 侧 `winresource`。骨架阶段为了让依赖图的边先钉死而留下的 `use ... as _;` 已经去掉
   （每条边都有真实调用）。
9. **`mesh-remote` 没有移植 Go 的 `DownloadPath`**（把相对地址解析成绝对 URL）：契约里
   `appUpdate.url`/`launcher.url` 一律是绝对地址，MODULES.md 的 API 里也没有它。阶段 1 若
   真遇到相对地址再补，而不是现在为它引一个 URL 库。
10. **启动器的 `installed_version` 仍是“宽松解析”**：只取版本号、不校验字符集，值不合法就
    当“没装过”，与搬进 workspace 之前的行为一致（字符集校验在 `layout::read_version` 里，
    那是给“当前版本”这个判据用的）。这是有意保留的行为差异，不是漏改。
11. **失败计数的落盘用 `atomic_replace`（0600）**，原来启动器是 `fs::write` + `rename`。
    语义不变（写失败静默），只是临时文件不再叫 `.json.tmp`、权限更紧。
12. **`rust-version = 1.83` 是硬底线**：`crates/install/src/lock.rs` 里必须写
    `FileExt::unlock(&file)` 而不是 `file.unlock()`——`std` 从 1.89 起给 `File` 加了同名固有
    方法，直接调用会悄悄把 MSRV 抬上去（clippy 的 `incompatible_msrv` 会报）。
13. **`mesh-core::StartSpec` 多三个字段、`spawn` 多一个 `Options`**：`gen`（引擎分配的代号）、
    `expect`（就绪判据：配置里的 hostname 与 ipv4）、`rpc_whitelist`（`MESH_RPC_WHITELIST` 由
    引擎读，core 线程不碰环境变量）。就绪时限做成 `Options`（默认 25s/500ms/3s，与 Go 一致）：
    它既是可调参数，也让“探测超时”能在几百毫秒内被测到。
14. **`gen` 由引擎分配，core 线程原样回带**。MODULES.md 说“gen 归 core 线程所有”，但引擎要
    判断一次 Start 的结果，还要在 Start/Stop 交错时丢掉旧代的事件；只有引擎自己发号，
    “我停掉的那一代”和“我正要启动的那一代”才在两边严格对得上。
15. **`mesh-engine::spawn` 返回 `(Sender<Cmd>, JoinHandle)`**，自己建 channel。引擎要给自己
    投递心跳结果与 core 事件（heartbeat 线程、core 事件回调），必须持有 `Sender`；
    MODULES.md 的签名只交出了 `Receiver`。
16. **`mesh-core::spawn` 收事件回调，不是 `Sender<Event>`**：引擎要把 `Event` 包成
    `Cmd::Core` 才能进自己的 inbox，为这个适配再起一个搬运线程不值得；回调里发的是无界
    channel，不会阻塞 core 线程。
17. **引擎的依赖表**：`sha2`/`hex`（自己算更新包摘要）、`rand`（新设备 id）、
    `windows-sys`（`GetLocalTime`）/`libc`（`localtime_r`）——日志时间列是**本地时间**，
    与 `time.Now().Format("15:04:05")` 一致；引 `chrono` 会拖进时区数据库。
    反过来 `serde_json`/`thiserror` 删掉了：失败计数与清单解析都在 `mesh-install`，引擎的
    错误一律是给用户看的一句话。
18. **开机自启走 `schtasks` 子进程，不接 Task Scheduler 的 COM 接口**：一共三条命令
    （建、删、查），`schtasks` 是系统自带、行为稳定的那个入口；COM 要多一整层 vtable 声明
    与内存管理，换不来可见收益。任务名、触发器、`RunLevel`、`ExecutionTimeLimit` 是现场
    排障时的事实来源，`schtasks /Query` 直接能看。
    `schtasks` 按控制台代码页输出（中文系统是 GBK），报错文本用 `MultiByteToWideChar(CP_ACP)`
    解回来再进日志与消息框，否则整段都是替换符。
19. **`build_nodes` 的 `path` 用空数组而不是 Go 的 `null`**（直连时 Go 的 nil slice 序列化成
    `null`，而 `types.ts` 写的是 `Hop[]`）；日志的 `count` 与 Go 一致：单条是 `1`，只省略 `0`
    ——MODULES.md 3.7 说“count 为 1 时必须省略”，与 Go 和前端注释都对不上，按后两者实现。
20. **`apply_update` 在 Unix 上先回 `Ok(())` 再后台下载**（Go 是在面板请求里同步下载到结束）。
    面板的 HTTP 请求不该为一次 20 分钟的下载挂着；下载完成后由引擎交接、进程退出，用户看到
    的结果一样。
21. **`mesh-core` 的输出泵不打时间戳**：时间列由 `mesh-engine::log` 在落盘时加（与 Go 的
    `logbook.Add` 一致）。所谓“环形缓冲”在本项目里其实是“日志文件 + 尾部读取 + 相邻行合并”，
    没有内存环形缓冲——Go 也没有，面板读的是文件尾部。
22. **`mesh-panel::start` 多一个 `quit` 参数**（MODULES.md 3.7 是三参数）。`/api/quit`
    必须真的能结束客户端，而 `Shutdown` 归 client 所有；在构造时就给全，避免“监听已起、
    退出动作还没装上”之间有一个不可用的 `/api/quit`。
23. **面板的 HTTP 层与路由层拆开**：`Router::handle(Request) -> Response` 不认 tiny_http，
    协议转换只在 `http.rs`。这是为了在没有托盘、没有管理员权限的机器上用 `StateCell` 与
    引擎替身覆盖全部路由（本轮 26 个面板用例）。
24. **`/api/update` 最多等引擎 1 秒**：Windows 的交接路径会在回复之前直接结束进程
    （`update.rs` 的 `handoff` → `Shutdown::run_and_exit`），面板不能一直等。1 秒内没回复就
    按“正在退出”回当前快照；能在 1 秒内回的失败仍然原样翻成 400。
25. **帮助文本走 stdout、退出码 0**。Go 的 `flag` 包把 usage 写到 stderr、`-h` 退 2；Rust 侧
    沿用启动器 `args.rs` 的约定。参数名、缺值/未知/多余参数的报错大意与 Go 一致。
26. **`--headless` 的信号源两平台不同**：Unix 用 `signal-hook`（SIGTERM/SIGINT），Windows 用
    `SetConsoleCtrlHandler`。Go 是 `os/signal` 一统；Rust 侧不引第三方跨平台信号库。
27. **打开浏览器/目录用 `explorer.exe` 转发**，不是 MODULES.md 列的 `windows` crate +
    `IShellDispatch`。explorer 是中等完整性的 shell 代理，效果与 COM 路径相同，少一个重量级
    依赖与一套 vtable 绑定。真机验证时若 UIPI 仍然挡，再补 COM 实现。
28. **托盘的自启勾选项不做乐观更新**：点击后在后台线程等引擎回复，成功了才改勾选状态；
    失败由引擎的 `Notice` 弹框，勾选最终由 2 秒一次的同步线程按快照对齐。Go 是同步调用
    `SetAutostart` 成功后立刻 `Check()`，用户看到的结果一样。
29. **Windows 资源只在 release 里编**（图标、`requireAdministrator` 清单、VERSIONINFO）：
    debug 的 `mesh-client.exe` 不需要 UAC，测试二进制也不会因为 SxS 起不来——与启动器
    `build.rs` 同一取舍。运行时版本始终来自 `bin/version.json`；`MESH_VERSION` 只进
    VERSIONINFO 与 `--help` 最后一行。
30. **`mesh-client` 多依赖 `mesh-state`**（MODULES.md 第 8 节的依赖表漏了）：`main` 要
    `Store` 读 `client.json`、取日志路径。`dependency_direction` 的允许表已同步。
31. **`Shutdown` 多了 `run()`**（跑收尾但不 `process::exit`）：client 在启动失败时要先停 core
    再以失败退出，否则 Unix 上会留下占着虚拟 IP 的孤儿 core。`run_and_exit` 现在是
    `run() + process::exit`。
32. **收尾顺序是“停 core → 停面板”**（engine 在 `spawn` 时先注册自己的步骤），与 Go 的
    “停面板 → 停 core”相反。两者都在进程退出前完成，没有可观察差异。
33. **`config/ui.json` 的读写收在面板 crate**（`write_url`/`read_url`），client 只调用；
    Go 是在 `main.go` 里散着做。格式与权限（0600、两空格缩进）不变。
34. **静态资源的路径只接受简单相对路径**：带 `..`、反斜杠或空段的请求按“不存在”处理，回
    `index.html`，不交给文件系统解析（Go 的 `http.Dir` 会做自己的清理）。
35. **headless 没有“安装更新，正在退出…”这句话**：Rust 的引擎在交接后直接走
    `Shutdown::run_and_exit(0)`，不存在 Go 那种“先 close(quitCh) 再等主循环返回”的窗口。
    用户看到的结果一样：进程正常退出、启动器接手。

## 与其它文档不一致的观察

以下是读文档与 Go 实现时发现的出入，**没有改文档**（按约定先报告）：

1. `client-prototype/internal/layout/layout.go` 的包注释与 `MaxVersionLen` 注释仍写着 `packages/`
   （“下载好的更新包，回滚材料”），但当前实现是下到安装根的 `update.tar.zst` + `.part`
   （`engine/update.go` 的 `downloadUpdate`），跑起来的安装根里没有 `packages/`。
   `docs/contract/updates.md` 与 PLAN.md 描述的布局才是对的，Rust 侧不建这个目录。
2. `docs/contract/client.md` 首段说“`config/` 目录无法创建时整棵树回退到
   `os.UserConfigDir()/mesh-client/`”，但 Go 的 `main.go` 里没有这段：`resolveRoot`
   只处理 `--root` 与 `findRoot(exe)`。Rust 侧按实现走（没有回退），阶段 1 若确实需要
   回退，得先定“回退后的日志与锁放哪”这类问题，不能只挪配置目录。
3. MODULES.md 第 8 节 `install` 的依赖表、3.6/3.8 对 `Shutdown` 归属的写法，见上一节
   第 1、7 条。
4. MODULES.md 3.7 说日志 `count` 为 1 时必须省略、3.5 说 `spawn(events: Sender<Event>)`，
   都与 Go 实现/前端类型不符，见上一节第 19、16 条。

## 下一轮

**已经就位的接口**（以下都由 client 用起来了）：

| 接口 | 用途 |
| --- | --- |
| `mesh_engine::spawn(Config, Arc<StateCell>, Sender<Notice>, Arc<Shutdown>)` | 起引擎线程；`Config` 要 `root`、`self_version`、`hostname`、`auto_update_disabled` |
| `StateCell::get() -> Arc<Published>` | 只读视图：`snapshot`、`beat`、`rpc`（`Some` 表示本代 core 在跑） |
| `Cmd::Settings` / `Reset` / `SetAutostart` / `ApplyUpdate` / `Reconnect` | 面板与托盘会改状态的动作，回复是 `Result<Snapshot, String>` 或 `Result<(), String>` |
| `Notice::ActionFailed(String)` | 用户动作失败，Windows 上 client 弹一次消息框 |
| `logs(path, n)` / `events(path, n)` | 日志页与“最近事件”，`path` = `<root>/log/client.log` |
| `build_nodes(peers, routes, LocalNode)` | 面板仪表盘拼行；查询与 2 秒缓存由面板自己做 |
| `Shutdown::register(fn)` / `run()` / `run_and_exit(code)` | 唯一的退出路径；托盘退出、`/api/quit`、SIGTERM、更新交接都走它 |

**剩下的**：

1. **Windows 桌面形态的回归**：托盘交互、`TaskbarCreated` 重建、提权进程下点击是否有效、
   `explorer.exe` 转发打开浏览器、自启任务的创建与删除、`/api/quit` 与托盘退出都能带走 core。
   只能人工核对。`schtasks /Create` 需要管理员（任务是 `HighestAvailable`），这与「客户端必须
   以管理员运行」一致，不算缺陷。
2. **Linux 发布链路**：代码能编 musl 静态版，但控制面上 `linux/amd64` 的记录已经删了，包与
   脚本还没接回。Windows 的出包脚本在 mesh 根下的 `build/`（不入库）。