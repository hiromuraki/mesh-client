# 用 Rust 重写客户端：总体方案

依据：`contract/{README,client,updates,device-api}.md`、`client/`（Go 实现）、`client-launcher/`（Rust 启动器）、`docs/client-flows.md`。
模块划分、并发模型与错误分层见 [MODULES.md](MODULES.md)。本文只写结论与理由。

## 1. 目标与非目标

### 目标

1. 用 Rust 重实现 `client/`：Windows 桌面形态（托盘常驻 + 浏览器里的本地面板）与 Linux headless 形态（systemd 拉起）。
2. 行为逐条对齐现有契约：`config/` 下六个文件、`/api/*` 面板接口、心跳与 Ed25519 签名、配置热更新、core 身份二选一、更新下载与交接、开机自启、日志格式与 4MB/2MB 裁剪、错误与 401 语义。
3. 与现有 Rust 启动器同处一个 cargo workspace：安装、回滚、监督流程一行不改，只把 `version`/`manifest`/`failures` 抽成共享 crate。
4. 彻底去掉 Wails3：图标与 `requireAdministrator` 清单由 `build.rs`（`winresource`）生成，不再有 `wails3 generate syso` 这一步。

### 非目标（明确不重写）

- **前端不重写**：`client/frontend`（pnpm + Vue 3 + Vite）保持原样，`client/frontend/dist` 原样嵌入二进制；`pnpm build` 仍是唯一前端构建入口。面板 JSON 的字段名、类型、可空性一字不改，Vue 侧零改动。
- **`client/cmd/pack` 留在 Go**：可复现 tar.zst（固定 `mode=0755`、`epoch` mtime、单线程 zstd、先写 `version.json`）与发布脚本不动。
- **启动器的安装/回滚/监督语义不动**：只搬进 workspace 并复用共享 crate。
- **不改契约**：协议字段、端口（11020 起顺延 20 个）、路径与文件名（`bin/version.json`、`update.tar.zst`、`bin.old/`、`.update-failures.json`、`instance.lock`、`config/ui.json`）、安装布局、更新交接方式、自启实现（任务计划程序 `Mesh Client`）都不变。
- **不加面板接口、不加 UI 形态**：不做 WebView 窗口（现有实现没有窗口，托盘只是菜单）。
- 不动 `server/`、`contract/`、`client/`、`build/easytier-mini.patch`。

## 2. 与现有 Go 实现的对应

一句话级别，逐个模块。

| Go | Rust | 说明 |
|---|---|---|
| `main.go`（参数、两种形态、托盘装配、退出） | `crates/client`（`main`/`cli`/`instance`/`desktop`/`headless`/`shutdown`） | 参数名、根目录规则、`--boot-check` 标记、退出码不变 |
| `web.go`（embed 前端产物） | `crates/panel/src/assets.rs` | 编译期嵌入 `client/frontend/dist` |
| `engine/engine.go` | `mesh-engine::{state,spawn}` | 状态枚举、常量、身份摘要、快照发布 |
| `engine/lifecycle.go` | `mesh-engine::lifecycle` | 连接循环、core 代次、连接/重拉退避 |
| `engine/heartbeat.go` | `mesh-engine::heartbeat` | 每轮心跳后要做的事（凭据、配置、更新、自启、core 补启动） |
| `engine/sync.go` | `mesh-engine::sync` | 配置应用与 401 分类处理 |
| `engine/core.go` | `mesh-engine::core` + `mesh-core` | 拆两半：状态机侧（何时起停）与进程侧（怎么起停） |
| `engine/settings.go` | `mesh-engine::settings` | 注册、保存设置、删除本地配置 |
| `engine/update.go` | `mesh-engine::update` | 下载、交接、启动器摘要替换 |
| `engine/views.go` | `mesh-engine::views` | 快照、仪表盘拼装、日志折叠与噪声过滤 |
| `engine/toml.go` | `mesh-state::hints` | `config.toml` 的六个提示字段 |
| `engine/autostart.go` + `internal/autostart` | `mesh-engine::autostart` | 自启缓存与 schtasks（`cfg(windows)`） |
| `coreproc/`（含 `job_windows`/`platform_windows`） | `mesh-core` | 定位、启动、输出泵、软停/强杀、Job 对象、就绪探测 |
| `easytierrpc/`（`wire`/`frame`/`client`/`pb`/`node`/`peer`/`route`） | `mesh-etrpc` | 帧编解码、手写 protobuf、三个查询、分片重组 |
| `state/` | `mesh-state` | `config/` 下五个文件与原子写 |
| `logbook/` | `mesh-engine::log` | 行式日志、高低水位裁剪、尾部解析 |
| `layout/` | `mesh-install::layout` | 安装根形状、`bin/version.json` |
| `lockfile/` | `mesh-install::lock` | `instance.lock` |
| `platform/` | `mesh-install::platform` | 文件名与 `GOOS/GOARCH` 平台标识 |
| `tray/` | `crates/client/src/tray`（Windows） | 手写 Win32 托盘，不再用 fyne/systray |
| `uits/` | `mesh-panel` | 面板 HTTP、guard、静态资源与 SPA 兜底 |
| `remote/` | `mesh-remote` | 设备面 HTTP、签名、`Date` 头偏差、下载 |
| `version/` + `buildinfo/` | `mesh-install::version` + `build.rs` | 版本序比较；版本号编译期注入 |
| `client-launcher/` | `crates/launcher`（bin `mesh`） | 原样搬迁，`version`/`failures`/`manifest` 换成 `mesh-install` |

## 3. 关键技术选型

| 项 | 候选 | 推荐 | 理由 |
|---|---|---|---|
| 异步运行时 | tokio 多线程 / smol / 不引 | **不引**（std 线程 + `mpsc`） | 并发点只有六个（心跳、core 托管、STUN 刷新、面板 HTTP、下载、托盘消息泵），全部是「等一个阻塞结果」；Go 版本来就是 goroutine + mutex/channel。引 tokio 会把 `Child::wait`、`schtasks`、zstd 全部推去 `spawn_blocking`，而托盘永远只能是同步 Win32 消息泵，两种模型并存 |
| 本地 HTTP | `tiny_http` / `axum`（要 tokio）/ 自建 hyper accept 循环 | **`tiny_http`** + 4 线程池 | 11 个路由（10 个 `/api` + 静态兜底）、回环访问、无 SSE/WebSocket；工作量在 guard 与静态兜底，不在 HTTP 协议。`tiny_http` 已停止演进，但它是写完的 HTTP/1.1 服务端，风险不在功能而在安全修复，回环面板可接受 |
| HTTP 客户端 | `ureq`（阻塞）/ `reqwest::blocking` | **`ureq`** | `reqwest::blocking` 内部会起一个 tokio 运行时（隐性异步）；`ureq` 的 agent 超时正好对应 Go 的 `context.WithTimeout`（心跳 20s、下载 20 分钟） |
| TLS | 系统 TLS（schannel）/ rustls + 系统根 / rustls + webpki-roots | **Windows: native-tls；Linux: rustls + 系统根证书** | Windows 用 schannel 不增体积；Linux musl 静态下链 openssl 是硬伤，rustls 只需 cc。内网自签 CA 走系统根，与 Go 的默认行为一致 |
| JSON | serde_json | **serde + serde_json** | 线上 camelCase 与磁盘 snake_case 两套格式分别用 `rename_all` 声明，正好对应契约「两套独立格式」；`skip_serializing_if` 复现 Go 的 `omitempty` |
| TOML | `toml` / `toml_edit` | **`toml`（只读）** | 客户端从不生成 `config.toml`，只读六个提示字段；`ipv4` 可能是字符串也可能是数组，用 `toml::Value` 取值 |
| 托盘 | `tray-icon`+`muda` / 手写 `Shell_NotifyIconW` / `tao` | **手写 Win32（windows-sys）** | 菜单只有六项、无窗口、无自绘；手写约 400 行且消息泵完全可控。`tray-icon`/`muda` 绑 winit 式事件循环，要先让主线程与初始化顺序给它，和「后端在线程里跑」冲突。卡在 DPI/深色模式上再换，接口只有「6 个菜单项 + 3 个可变状态」 |
| 静态资源嵌入 | `include_dir` / `rust-embed` | **`include_dir`** | 编译期目录树，路径不存在直接编译失败（错误尽早暴露）；前端未构建时用与 `wails3 task stub:frontend` 等价的占位脚本，不引入 debug 期文件系统回退 |
| 子进程托管 | std + windows-sys Job / `tokio::process` / `jobobject` | **std + windows-sys** | `CreateJobObject`/`SetInformationJobObject(KILL_ON_JOB_CLOSE)`/`AssignProcessToJobObject` 三个调用与 `job_windows.go` 一一对应；`tokio::process` 会拖进运行时时 |
| protobuf 编解码 | 手写 `pb.rs` / `prost` 的 `encoding` 原语 / `prost-build` 代码生成 | **手写** | 只有五个消息、字段号已由 `wire.rs` 固定、仓库里没有 `.proto`；手写约 120 行带单测，可与 Go 的 `pb.go` 逐行对照。Go 用的 `protowire` 在 Rust 没有对等物，这是 Rust 更麻烦的一处；退路是 `prost::encoding`（不跑 protoc） |
| 日志 | 手写 logbook / `tracing` + `tracing-appender` | **手写** | `HH:MM:SS [app|core] 文本` 是面板解析与用户可见的格式；core 输出与客户端事件同文件、超 4MB 裁到 2MB 且只保留完整行，`tracing-appender` 做不到「按字节裁剪 + 整行」 |
| 单实例锁 | `fs4` / `fd-lock` / 手写 `LockFileEx`+`flock` | **`fs4::try_lock_exclusive`** | OS 锁（进程崩溃自动释放），语义必须与 Go 相同，不能用「文件存在即运行中」；一行覆盖两个平台，省掉 libc 依赖 |
| 自更新（客户端侧） | `self_update` 类库 / 手写 | **手写** | 交接协议是本项目自己的（`--file/--wait-for/--sha256`、`update.tar.zst` 本身就是信号）；客户端只做「下载 → sha256 → 交接」，解包与切换在启动器 |
| 开机自启 | schtasks + UTF-16 XML / COM `ITaskService` / `HKCU\...\Run` | **schtasks（同 Go）** | 任务名、触发器、`RunLevel`、`ExecutionTimeLimit=PT0S` 都是用户可见的事实来源，现场排障靠 `schtasks /Query`；COM 版多一堆接口且错误路径行为不同 |
| 提权 | 清单 `requireAdministrator` / 运行期自我 `runas` | **清单** | 启动器已经按「当前是否已提权」分两条路拉起客户端（`CreateProcess` vs `ShellExecuteEx(runas)`），清单是它判断的依据；客户端自己重新 `runas` 会出现两个进程抢 `instance.lock` |
| COM（打开浏览器） | `windows` crate 的 `IShellDispatch` / 自写 IDispatch vtable / 让启动器代开 | **`windows` crate 的 `IShellDispatch`** | 客户端是 `requireAdministrator` 的高完整性进程，UIPI 会拦住把 URL 交给已运行的浏览器；`Shell.Application` 由 explorer 托管，天然是中完整性。自写 vtable 太脆；让启动器代开要给启动器加 `--open-url`，等于改启动器契约 |
| CLI 解析 | `clap` / 手写 | **手写** | 9 个开关、无子命令、无补全；沿用启动器 `args.rs` 的风格（支持 `--x=y`）可保证两个二进制的解析行为一致，clap 换来的是数百 KB 与一个 proc-macro |
| 信号（Linux） | `signal-hook` / `libc::sigaction` | **`signal-hook`** | SIGTERM 是 systemd 停客户端的唯一入口，需要一个能安全唤醒主循环的迭代器线程，10 行代码 |
| Windows 资源（图标/清单/版本信息） | `winresource` / `embed-resource` / `mt.exe` | **`winresource`** | 启动器已经在用它，两个二进制共用一套 `build.rs` 写法；顺带把 Wails3 彻底删掉 |
| 版本号注入 | `build.rs` 读环境变量 / `include_str!` 读文件 | **`build.rs` 读 `MESH_VERSION`，缺失即编译失败** | 不能默认 `0.0.0`——心跳上报错版本会让控制面把设备钉在「有更新」上；`build/version.ps1` 照旧递增并把值传进来 |

## 4. 两个平台的差异

| | Windows | Linux |
|---|---|---|
| 形态 | 默认桌面（托盘常驻）；`--headless` 也可 | 只有 headless，`--headless` 恒真 |
| 清单 | `requireAdministrator`，`-H windowsgui` | 无 |
| 提权 | 启动器按已提权/未提权分 `CreateProcess` 与 `ShellExecuteEx(runas)` | 无 |
| 自启 | 任务计划程序，目标 = 安装根的 `mesh.exe` | 不支持，`autostartSupported=false` |
| core 停止 | `taskkill /PID x /T`，命令本身失败立即改用 `/F`（无窗口 console 程序软停必失败）；成功才等 5s | 先对整个进程组 SIGTERM（`pre_exec` 里 `setsid()`，与 Go 的 `Setsid` 一致，不能用 `process_group(0)`），5s 不退再 SIGKILL，再等 3s |
| core 生命周期 | kill-on-close Job 对象兜底 | 客户端自己 `Shutdown` 停 core；启动器回滚时用 SIGTERM 而非 SIGKILL（SIGKILL 会留下孤儿 core 抢同一虚拟 IP） |
| 托盘 | 六项菜单 + 勾选 + 更新行 | 无 |
| 打开面板/目录 | `Shell.Application` COM（UIPI）/ `explorer.exe` | 不打开（面板地址写 `config/ui.json`，`ssh -L` 或 `curl`） |
| 崩溃可见性 | 无控制台 → `MessageBoxW` 报启动失败 | stderr 直出，退出码非零 |
| 构建 | `cargo build --release`，`build.rs` 嵌资源 | `--target x86_64-unknown-linux-musl`，`.cargo/config.toml` 指定 `musl-gcc`（沿用启动器那份） |
| 平台标识 | `windows/amd64` | `linux/amd64` |
| 更新交接 | 拉 `mesh.exe --file --wait-for --sha256` 后退出 | 写 `update.tar.zst` 后正常退出，常驻启动器接手 |

`cfg` 只出现在五个地方：`install::platform` 的文件名与平台标识、`client::tray`、`mesh-core` 的 Job 与停止方式、`engine::autostart`、`client::shutdown` 的退出握手。其余代码两个平台共用，不做 `cfg` 转发层。

## 5. 分阶段迁移

### 能否与 Go 版共存

- **协议层可以**：两端读的是同一份 `contract/`，同一个控制面可以把 Go 客户端和 Rust 客户端当成两台不同设备。
- **安装根不能**：`instance.lock` 与 `bin/` 是唯一的。所以共存的实际形态只有两种：另一台设备 / 另一个安装根（`--root dist-next`），或按平台分开发布——发布槽位是按 `GOOS/GOARCH` 的，可以先给 `linux/amd64` 发 Rust 包、`windows/amd64` 继续发 Go 包。
- **回滚**：把上一版 Go 包用**新的版本号**重新发布。同号重发会被两道闸挡住（客户端的「不比当前新不装」与 `.update-failures.json` 的连续失败 2 次）。

### 阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| 0 | workspace + 四个叶子 crate（`install`/`state`/`remote`/`etrpc`，各有单测）；把启动器搬进 workspace，`version.rs`/`failures.rs`/`manifest.rs` 换成 `mesh-install` | 启动器行为零变化，`cargo test` 覆盖版本序、失败计数、清单解析（这三处现在 Go 与 Rust 各有一份实现，正好被这一步合并） |
| 1 | `mesh-core` + `mesh-engine` + `mesh-panel` + `mesh-client` 的 **Linux headless** 路径 | 在 `wsl` 或 `front-server` 上换掉发布包：心跳、配置下发、core 托管与就绪、日志、自更新交接、`systemctl restart` 全通 |
| 2 | Windows 桌面路径：托盘、清单、COM 开浏览器、schtasks | 在本机 `dist-next/` 装一份，与在跑的 Go 版并行观察；逐项核对托盘的四个动作（打开面板 / 打开应用目录 / 开机自启 / 退出）与 UAC 行为 |
| 3 | Go 版下线，`client/` 只留 `frontend/`、`cmd/pack`、`build/` 脚本 | —— |

**先换哪一块**：模块上先换纯逻辑（`install`/`version`/`etrpc`/`remote`，可单测、可与 Go 逐行对读），状态机最后换；**平台**上先 Linux headless——那里没有托盘、没有 UAC、没有 COM，验收面最小，而心跳/配置/core/更新/日志这五条主干一个不少。

**面板要早换，别最后换**：它是验收手段。Rust 面板起来后，可以让 Go 客户端与 Rust 客户端各注册一台设备，逐字段 diff `/api/snapshot`、`/api/dashboard`，这是最省力的等价性证明。

## 6. 风险与代价

### 三个最大风险

1. **状态机语义在重写中悄悄丢失**。`engine` 里至少有七处是过去几轮 focused review 修出来的、且从代码表面看不出来的约定：凭据上报必须报 core 实际加载的那张（F1）、`coreGen` 必须在 `Stop` 之前递增、revive 退避要在每次尝试**之前**等、`autoTried`/`launcherTried` 的「同一版本只试一次」、401 的「时钟偏差只降级 / 连续 3 次才算身份失效」、`update` 失败态保留到换版本或用户重试、`stop`/`quit` 的先后。这些没有测试就只剩注释。缓解：这些点全部写成黑盒用例（见第 7 节），并在 Rust 侧对应位置保留同样的注释与单测。
2. **托盘 + Win32 + UAC 的组合只能真机验证**。消息泵、`TaskbarCreated` 重建图标、`TrackPopupMenu` 前必须 `SetForegroundWindow`、`requireAdministrator` 下用 COM 打开浏览器、schtasks 的 UTF-16 XML——CI 覆盖不到，出错的表现是「图标没了」「菜单点不掉」「什么都没发生」。缓解：先用一个只有图标+退出的小程序验证消息泵与资源嵌入，再挂菜单；托盘逻辑与引擎只通过 channel 通信，托盘坏掉不影响网络。
3. **手写 protobuf 解析的错误是静默的**。字段号或 wire type 错一位，表现是「节点表空白」或「IP 显示 0.0.0.0」而不是一条错误。缓解：从运行中的 core 抓一份真实响应做 fixture 单测（`ShowNodeInfo`/`ListPeer`/`ListRoute` 各一份，含分片的大响应），并保留 Go 实现作为对照 oracle；`maxPieces`、32MiB 载荷上限、`total_pieces` 缺省为 1 这些边界照搬。

### Rust 反而更麻烦的地方

- **protobuf**：Go 有 `google.golang.org/protobuf/encoding/protowire`（官方、零反射、无 codegen）；Rust 没有对等的小原语库，要么手写 varint/tag/length 解析，要么引 `prost` 的半个生态。这是唯一一处「Go 一行依赖、Rust 一百行代码」的地方。
- **托盘**：Go 是 `systray.Run(...)` 一行；Rust 要自己写 `Shell_NotifyIconW` + 隐藏窗口 + `WM_COMMAND` 分发 + `WM_APP` 唤醒 + 图标资源加载（`LoadImageW` 取 exe 里嵌的 `RT_GROUP_ICON`），还要处理 explorer 重启与 DPI。
- **托盘事件循环与 async 运行时的坑**：Win32 菜单只能在创建它的线程上改，而那个线程必须长期阻塞在消息循环里。若选了 tokio，托盘回调就得 `spawn_blocking` 回主线程、状态更新要走 `PostThreadMessage` 或框架的 `run_on_main_thread`，两边互相等；这是选同步线程模型最直接的动机。反过来说，这个模型下所有「等待」都必须有明确超时（socket 超时、`recv_timeout`、`process::exit` 前的手工停止），不能靠取消一个 future。
- **提权进程打开浏览器**：Go 有 `go-ole` 的 `oleutil.CallMethod(shell, "ShellExecute", ...)`，二十行；Rust 要引 `windows` crate 的 COM 支持并用 `IShellDispatch` 的五个 `VARIANT` 参数。
- **行式读子进程输出**：Go 的 `bufio.Scanner` 可以直接设 4MiB 上限；Rust 的 `read_until`/`read_line` 无上限，必须自己写「按块读 + 截断超长行 + 继续排空管道」，否则 core 的 panic backtrace 会让管道写满、进程卡死（Go 的 `pump` 里那段注释就是这个坑）。
- **Windows 上替换被打开的文件**：日志裁剪、`rename` 覆盖都在这里踩过；Rust 一样要 close → rename → reopen，不能指望 `std::fs::rename` 帮我们绕开。
- **musl 静态链接**：客户端的硬约束是**不能链 openssl**（Linux 侧用 rustls），`ring` 需要 cc，`musl-gcc` 已配好（启动器在用）；`zstd` 的 C 源码只在启动器侧编。目标机最低 glibc 2.34 这条约束不变。
- **体积**：目标与 Go 版同量级（内嵌前端与 6MB 字体是主要部分，两者都要带）；Rust 侧多出来的是 TLS 栈（Windows 走 schannel 可忽略）与托盘代码，少掉的是 Go 运行时。沿用启动器的 `opt-level=z`/`lto`/`panic=abort`/`strip`。实际数字以首次出包为准，不作为选型依据。
- **双实现维护期**：阶段 1–2 之间两份实现并存，`contract/` 的任何改动要改两处。这段时间要尽量短，并用第 7 节的 diff 手段而不是靠人记。

## 7. 验收与 review 手段

1. **假控制面复用**：`client/testdata/stubserver`（Go）与 `testdata/e2echeck` 都通过 HTTP/CLI 交互，与语言无关，直接拿来当 Rust 客户端的对端。
2. **黑盒用例清单**（每条都可脚本化，用 curl 打面板 + 读 `log/client.log` + 看进程树）：
   - 无 `client.json` 启动 → 面板停在设置页（`deviceId` 为空）；填 `--server/--token` → 注册、落盘、写 `config.toml`、core 起来、`status=online`。
   - 注册成功但 core 起不来（没有凭据）→ 心跳继续跑，`status=error` 且 `lastError` 写明原因；下一轮心跳拿到凭据后 core 自动补起。
   - 配置下发：服务端换 `configHash` → 旧 core 停 → 新 `config.toml` 落盘 → 新 core 在 25s 内就绪；内容与磁盘逐字节相同时不重启 core。
   - `credentialId` 上报：新凭据落盘但 core 未重启时，上报的仍是旧 id（F1 的回归用例）。
   - core 被外部杀掉 → `status=degraded` → 按 1s→30s 退避重拉 → 成功回 `online`；日志两行。
   - 401 `revoked` → 清 `device_id`、停 core、回设置页；其它 401 且 `Date` 偏差 > 300s → 只降级并提示对时。
   - 更新：`autoUpdate=false` 只提示；同版本失败两次后不再自动试；Windows 交接后 `mesh.exe` 等旧 pid 退出并装成 `bin/`，失败回 `bin.old`。
   - `bin.old` 存在时启动 → 无条件回滚；`--boot-check` 标记在 60s 内写出（引擎与面板就绪之后，不等网络）。
   - 第二次启动（`instance.lock` 被占）→ 用 `config/ui.json` 里的地址打开浏览器；headless 则把地址打到 stderr。
   - 面板 guard：非回环 `Host` → 400 `bad_host`；跨源 `Sec-Fetch-Site` → 403 `cross_origin`；未知路径回 `index.html`。
3. **等价性 diff**：Go 版与 Rust 版各注册一台设备，轮询同一个控制面，脚本逐字段比较 `/api/snapshot` 与 `/api/dashboard` 的 JSON（忽略时间戳与在本机不同的事实，如 pid）。
4. **静态检查**：`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`、`cargo test`；`wire.rs` 的常量表逐条与 `client/internal/easytierrpc/wire.go` 对读（这是升级 EasyTier 时唯一要改的地方，必须一眼能对上）。
5. **机器上的现场核对**：本机是 Windows + UAC（需要人点），Linux 节点（`wsl`/`front-server`）跑 systemd 重启与自更新；这两条只能手动，写进发布检查单。
6. **复核归档沿用现有惯例**：`client/code-reviews/<时间戳>_<sha>/`（`REVIEW.md` + `scripts/` 里的探针）；Rust 版放 `client-rust/code-reviews/.../scripts/`，探针尽量用 pwsh/sh 写（打面板 API、读日志、看进程树），不绑定实现语言。

## 8. 未决事项

- 切换完成后 `client-rust/` 是否改名并顶替 `client/`（保留 `client/frontend`、`client/cmd/pack`、`client/build`），在阶段 3 决定，不在本次范围。
- `client/build/*.ps1` 需要新增 `cargo` 分支（版本号注入 + 产物拷贝到 `dist/bin`），具体命令在阶段 0 落地时确定。
