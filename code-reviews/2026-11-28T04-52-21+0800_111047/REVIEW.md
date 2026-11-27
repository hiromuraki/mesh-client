# 客户端代码审查

- 审查提交：`111047f955600e330d10741076a9fdfd35743919`
- 分支：`main`
- 时间：2026-11-28T04:52:21+08:00（本次规范化复核开始，承接此前同一提交的静态审查）
- 工作区：审查开始时干净。
- 范围：全项目工程质量、状态模型简化、职责边界、资源管理与次要安全审查。
- 对照文档：`DESIGN.md`、`README.md`、`MODULES.md`、上级 `DESIGN.md` 与 `docs/contract/`。

## 上轮复核

复核区间为 `9937cca..111047f`，通过 `git log -- code-reviews` 确定上轮基线，核对 `git diff --stat`、[上轮回应](../2026-11-28T01-59-36+0800_9937cc/RESPONSE.md) 与当前源码。下列结论均为静态复核，本轮没有重跑其回归测试。

| 上轮编号 | 结论 | 依据及后续事项 |
| --- | --- | --- |
| F1 | 接受 | `undo()` 在目录换回前写 `restoring`，对已完成的回滚有独立判据。 |
| F2 | 接受 | `boot_and_settle()` 收口验证、启动确认和失败恢复，停止失败保留事务。仍需整机确认超时验证。 |
| F3 | 接受 | `commit()` 无事务时不消费待装包，下载任务先写临时文件再由引擎接受。接受已登记的撤回窗口，预取与安装意图的职责归本轮 F3。 |
| F4 | 接受 | `Owner` 与引擎提交共用安装根锁，Linux 监督器在等待客户端期间释放锁。 |
| F5 | 接受 | offer 版本及摘要与任务代次均参与结果接收判据。安装意图的独立状态仍有简化空间，见本轮 F3。 |
| F6 | 接受 | 进程内替换结果更新摘要和版本，跨进程单调性已明确排除在当前承诺之外。 |
| F7 | 接受 | 归档读取、解包、下载具备字节数、条目数及时间预算。真实长时间阻塞路径未复测。 |
| F8 | 接受 | 凭据状态由实际加载事实及磁盘事实推导，`Held` 上报空串符合当前契约。接受暂不新增字段的取舍。 |
| F9 | 接受 | 持久性承诺明确收窄为进程中断恢复。 |
| F10 | 部分接受 | 注册与配置应用入口已收口，身份与配置/凭据的持久化切换仍跨多个文件，见本轮 F2。 |
| F11 | 接受 | 缓存按 core 实例绑定，过时结果不写回缓存，未知拓扑与空拓扑分开。 |
| F12 | 接受 | 路由解析采用完整 peer ID 图，地址归并仅用于展示，保留路径不完整标记。接受现行拓扑分层取舍。 |

## 审查发现

### [P2][F1] core 监督仍受控制面连接循环约束，运行状态存在多重来源

位置：[run.rs:362](../../crates/engine/src/run.rs#L362)、[run.rs:450](../../crates/engine/src/run.rs#L450)、[run.rs:500](../../crates/engine/src/run.rs#L500)、[run.rs:615](../../crates/engine/src/run.rs#L615)、[run.rs:695](../../crates/engine/src/run.rs#L695)、[sync.rs:90](../../crates/engine/src/sync.rs#L90)、[views.rs:123](../../crates/engine/src/views.rs#L123)。

`connection_loop` 先同步执行 `connect()`，只有连接成功后才进入按 `next_deadline()` 驱动 core 重拉的内层循环。连接失败退避期间，`wait()` 处理命令，但不调度 `revive.due`。当已配置设备的控制面暂时不可达、core 同时退出时，core 的独立监督期限无法正常推进。此行为与数据面应独立存活的设计目标不一致。

首次心跳在 `connect()` 内解释配置与凭据，后续心跳在 `on_heartbeat()` 内另行解释配置、凭据及更新。`api` 在首次请求成功之前已经赋值，`Started` 事件却以 `api.is_some()` 判定在线。连接可用性和 core 就绪事实之间缺少明确的组合判据。`Status` 由多个处理器写入，快照再修正部分不一致组合。

建议保留单线程引擎、邮箱与只读快照，改成统一事件循环，分别推进 `ControlState` 与 `CoreState`。连接重试、定期心跳、core 重拉各拥有自己的期限，下一次唤醒取最早期限。所有成功心跳走同一响应处理函数，状态由两个生命周期推导。控制面请求和 STUN 结果均关联其有效代次，替代无条件应用晚到结果的行为。

验收要点：控制面持续不可达时 core 仍按自身退避重拉，首次及周期心跳具备相同处理语义，过时代次的结果不修改当前状态。此项无需引入异步运行时或通用任务框架。

### [P2][F2] 身份代次、落盘配置与已应用规格缺少统一关系

位置：[settings.rs:36](../../crates/engine/src/settings.rs#L36)、[settings.rs:108](../../crates/engine/src/settings.rs#L108)、[sync.rs:175](../../crates/engine/src/sync.rs#L175)、[run.rs:115](../../crates/engine/src/run.rs#L115)、[run.rs:942](../../crates/engine/src/run.rs#L942)、[run.rs:971](../../crates/engine/src/run.rs#L971)。

`save_settings` 保存新 `client.json` 后才由 `activate` 删除旧配置与凭据，且删除失败仅记录日志。进程在多次文件操作之间中断或文件删除失败时，磁盘仍可能出现新身份与旧代次数据的组合。启动路径重新从这些彼此独立的文件读取状态。

`CoreRun` 只含进程代次、凭据 ID、RPC 地址及就绪信息。当前 `apply_locked` 主要根据磁盘内容及 `credential_stale()` 决定是否重启，改名又走独立停止路径。因此运行中的 core 是否符合当前期望只能通过多个局部判据判断。

建议建立本地身份代次及 `CoreSpec`，其中规格包括身份代次、配置摘要、实际 hostname、认证模式及凭据标识。当前绑定记录原子选定对应配置与凭据的生效范围，旧代次文件随后作为回收清理。可采用 `config/` 下代次目录，避免 TOML 需要额外解析包装。长期签名密钥保留。运行实例保存它实际应用的规格，配置同步、改名、凭据轮换和重新绑定统一由期望规格变化驱动替换。

验收要点：在写数据、切换绑定及回收之间中断后，本地读取只能得到确定的一代数据。实际就绪的 core 才能确认凭据，规格相同不重复拉起。磁盘布局变化需先修改上级 `docs/contract/client.md` 与 `updates.md`。

### [P2][F3] 更新模型混合 offer、工件准备、下载任务与安装意图

位置：[update.rs:78](../../crates/engine/src/update.rs#L78)、[update.rs:351](../../crates/engine/src/update.rs#L351)、[update.rs:384](../../crates/engine/src/update.rs#L384)、[update.rs:455](../../crates/engine/src/update.rs#L455)、[update.rs:925](../../crates/engine/src/update.rs#L925)、[update.rs:968](../../crates/engine/src/update.rs#L968)。

`UpdateState` 有 16 个字段。`Ready` 同时表示 Linux 已收到 offer、下载完成等待安装根写权、已提交下载包。多个布尔值和可选路径共同表达进度。若用户触发安装时 `resume_commit()` 仍未取得写权，`apply_update()` 可能进入重新下载路径，丢弃已校验的暂存状态。自动安装和用户安装共用 `apply_after_download`，下载途中策略变化后该字段不能表明安装意图来自哪种授权。

建议将更新模型拆成 `Option<VerifiedOffer>`、`Option<DownloadTask>`、工件状态及 `InstallIntent`。工件状态明确区分未准备、已校验暂存、失败。安装意图区分手动确认、自动策略与无意图，并关联具体 offer。仅在真正授权交接时提交待装记录，预取使用独立暂存路径。安装根争用只延期提交，复用已经校验的工件。自动意图在实际交接前重新校验当前策略。

启动器槽位也有任务占位、摘要和版本状态，见 [update.rs:134](../../crates/engine/src/update.rs#L134) 与 [update.rs:678](../../crates/engine/src/update.rs#L678)。建议统一工件身份和下载验证能力，后台 worker 返回验证后的临时工件及任务结果，由引擎拥有任务接收及提交决策。客户端重启安装与启动器原地替换仍各有明确的提交路径。

验收要点：撤回、重发、策略变化、下载失败、安装锁争用和手动操作具有唯一转移。已验工件不会因写权暂时不可用而重下。预取、用户授权和启动器交接在磁盘上有各自明确的语义。

### [P3][F4] 安装事务复杂度主要来自固定 `bin/` 目录的原地替换

位置：[install.rs:128](../../crates/launcher/src/install.rs#L128)、[install.rs:193](../../crates/launcher/src/install.rs#L193)、[install.rs:274](../../crates/launcher/src/install.rs#L274)、[install.rs:401](../../crates/launcher/src/install.rs#L401)、[install.rs:683](../../crates/launcher/src/install.rs#L683)、[supervisor.rs:84](../../crates/launcher/src/supervisor.rs#L84)。

安装过程维护 `fresh`、`replaced`、`installed`、`restoring`、`committed` 五种事务状态及无事务状态，恢复还依赖三个目录的存在关系。原因是当前确认版本在解包前即被移走。Linux 待装路径还要单独扫描包版本，随后安装再扫描归档及解包，重复读取相同资源。

建议评估固定两个安装槽位，使用一个原子记录保存已确认槽与可选候选槽。新工件完整解包并校验后才成为待确认槽，启动成功提交选择，失败停止候选并清除待确认记录。原确认槽在试运行中保持完整，减少回滚目录换名、清理状态及恢复分支。首次安装对应无已确认槽。固定安装根、启动器和配置目录仍可维持，但程序目录及部署契约会变化。

若固定 `bin/` 属于硬约束，则建议继续使用现有事务，先完整解包校验至独立 staging 目录，再执行目录交换。待装入口共享已解析工件信息，减少归档重复扫描。在确认 A/B 可在 Windows、Linux 均维持原有启动、确认、回滚及失败计数语义前，保持现行布局。

### [P3][F5] 增量 offer 协议引入不必要的缓存形状与分支

位置：[remote/lib.rs:111](../../crates/remote/src/lib.rs#L111)、[remote/lib.rs:237](../../crates/remote/src/lib.rs#L237)、[update.rs:179](../../crates/engine/src/update.rs#L179)、[update.rs:196](../../crates/engine/src/update.rs#L196)、[views.rs:56](../../crates/engine/src/views.rs#L56)。

`appUpdateKnown` 与 `unchanged` 要求首次完整 offer 保留在内存中，后续走另一类响应。精简形状与完整形状独立处理，曾导致第二轮 F2 的重试语义遗漏。目前元数据和下载进度依赖这两条路径保持一致。

建议每次心跳有可更新发布时直接返回完整 offer，缺失即无可升级发布。客户端用已经验证的版本及摘要识别同一工件，只在必要时启动下载或提示。按服务端 200 台设备上限、每 30 秒心跳和假设 2 KiB 的说明估算，全部设备都有更新时额外说明流量约 13.3 KiB/s。8 台约 0.53 KiB/s，其余字段及字符编码开销另计。当前规模适合删除这一优化协议，配置正文仍使用摘要判断变化。控制面报告有对应建议。

### [P3][F6] 拓扑展示职责位于引擎，实际由面板独占消费

位置：[views.rs:204](../../crates/engine/src/views.rs#L204)、[views.rs:248](../../crates/engine/src/views.rs#L248)、[engine/lib.rs:29](../../crates/engine/src/lib.rs#L29)、[panel/router.rs:303](../../crates/panel/src/router.rs#L303)。

节点展示行、路由归并与路径解析占据 `views.rs` 的大部分代码。实际生产调用者是面板，面板已经直接查询 ET RPC 并拥有拓扑缓存。这些视图构造与引擎的状态推进无直接关系。

建议移动 `NodeRow`、`LocalNode`、`Hop`、`build_nodes` 及相关测试至 `mesh-panel` 内部 `topology.rs`。引擎仅发布运行事实、自身状态快照及 core 实例标识。`mesh-etrpc` 继续负责协议结构。此项可以独立实施，无需新增 crate。

### [P3][F7] 多处局部实现仍重复表达同一规则

| 位置 | 当前事实 | 建议 |
| --- | --- | --- |
| [core/thread.rs:50](../../crates/core/src/thread.rs#L50) | `running`、`pending` 两个 Option 通过调用顺序保持互斥。 | 由一个进程生命周期枚举表达空闲、探测中、就绪及未能停止的进程，保留句柄所有权。 |
| [update.rs:33](../../crates/engine/src/update.rs#L33)、[failures.rs:41](../../crates/install/src/failures.rs#L41) | 客户端独立定义失败阈值，安装 crate 已提供 `exhausted()`。 | 复用 `Failures::exhausted`，删除重复阈值。 |
| [sync.rs:514](../../crates/engine/src/sync.rs#L514)、[install.rs:968](../../crates/launcher/src/install.rs#L968) | 文件完整摘要存在两套实现，且客户端摘要位于控制面同步模块。 | 移至安装工件验证能力中复用。 |
| [panel/router.rs:155](../../crates/panel/src/router.rs#L155) | `accepts()` 和 `allow()` 分别重复 HTTP 方法分组。 | 从唯一方法定义推导接受条件和响应头。 |

## 次要安全审查

本轮仍以现有 `PENDING.md` 中已经登记的安全取舍为基线。权限收紧失败仍可继续启动（[run.rs:233](../../crates/engine/src/run.rs#L233)），控制面地址仍支持明文 HTTP（[settings.rs:195](../../crates/engine/src/settings.rs#L195)）。建议保留既定优先级，采用可观测、明确的安全失败行为及经过认证的加密传输。凭据有效性与失效恢复可随 F2 收口。F3 的工件身份与交接记录应保持完整性校验。

## 待确认项

- A/B 安装槽是否值得改变固定 `bin/` 路径及相关运维契约。现有事务已具备回归测试，先确认等价行为及实际简化收益。
- 本地身份代次采用目录或结构化持久化记录的具体布局。需核对使用固定文件路径的部署操作。
- 本轮不改变已明确接受的兼容性、启动器跨进程版本记录及凭据上报语义取舍。

## 验证

| 检查 | 本轮结果 |
| --- | --- |
| `git log -- code-reviews` | 已检查历史评审提交。 |
| `git diff --stat 9937cca..111047f` | 已检查上轮修复范围。 |
| `git status --short` | 审查开始时工作区干净。 |
| 源码及契约 | 手工静态核对，发现均有对应源码位置。 |
| 编译、clippy、测试与前端构建 | 本轮未执行。上一轮通过情况见其回应记录。 |

未验证真实 core、TUN、Windows UAC、systemd、崩溃中断及候选重构的实际实现。审查阶段未修改生产代码、测试代码或线上设备。
