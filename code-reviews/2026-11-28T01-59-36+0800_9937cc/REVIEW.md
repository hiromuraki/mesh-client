# 客户端代码审查（安全与工程质量）

- 审查提交：`9937ccad1367ec8968d4254aa6842bba4aa7e166`
- 分支：`main`
- 审查开始：2026-11-28T01:59:36+08:00（Asia/Hong_Kong，首次记录的本地审查时间）
- 工作区：开始时 `git status --porcelain` 无输出。审查方仅新增本文件，未修改生产代码或测试代码，未提交。
- 范围：Rust workspace、客户端前端、EasyTier 补丁及构建配置，重点检查启动、安装事务、更新、身份和凭据、core 生命周期与本地面板。契约按 mesh 根目录 `DESIGN.md`、`docs/contract/`、`docs/security.md` 及仓库内 `DESIGN.md`、`README.md`、`MODULES.md` 核对。
- 对端：`server/code-reviews/2026-11-28T01-59-36+0800_7827a1/REVIEW.md`。

## 结论

本轮发现 12 项。优先处理安装事务的所有权、失败恢复及启动器版本事实源；这些问题会影响普通更新、手工安装、进程中断与远程无人值守恢复。其余问题集中在更新决策、资源上限和会话切换。

| 编号 | 级别 | 摘要 |
| --- | --- | --- |
| F1 | P1 | 已恢复的旧版本可能在重试回滚时被删掉 |
| F2 | P1 | `installed` 的启动确认与失败处理在入口之间不一致 |
| F3 | P1 | 普通启动提交可能消费尚未安装的更新包 |
| F4 | P1 | 安装根的排他所有权未覆盖启动器 |
| F5 | P2 | 更新策略与实际下载任务缺少一致的 offer 标识 |
| F6 | P2 | 启动器替换后版本记录和内存事实未更新 |
| F7 | P2 | 启动器解压与手动下载缺乏统一资源预算 |
| F8 | P2 | core 尚未确认就绪时，磁盘凭据被误当作已加载凭据 |
| F9 | P2 | 安装事务持久性承诺超出了文件系统操作的实际保证 |
| F10 | P2 | 注册后的落盘与激活不是同一次状态转换 |
| F11 | P3 | 面板查询缓存没有绑定 core 实例 |
| F12 | P3 | 路由归并丢弃了解析中继路径需要的 peer ID |

## 1. 既往发现复核

### 1.1 本地交叉审查档案

上一轮对应 `code-reviews/2026-11-27T14-26-00+0800_74ce4f8/`，修复提交为 `ad99f37`，随后 `a511a91` 加入安装根加固与启动校验。本轮检查 `a511a91..9937cca` 的 15 个提交，重点是清单合法域、core 路径收束、安装事务、版本记录、待装包清理、心跳代次及 TLS 后端。

| 上轮编号 | 结论 | 依据 |
| --- | --- | --- |
| F1 旧格式身份 | 接受用户裁定 | 已移除兼容分支，`state/src/lib.rs` 的旧格式拒绝用例仍在；早期人工重置、重新注册属于明确的发布取舍，不追加迁移要求。 |
| F2 精简 offer 重试 | 成立 | `engine/src/update.rs:148-156` 在精简响应时执行失败重试，已有对应测试。 |
| F3 空签名发布记录 | 接受原回应 | 服务端 `api/device_api.go:264-275,323` 已过滤空签名；客户端正常验签路径即可，无须引入重复日志去重状态。 |
| F4 配置目录 ACL 错误 | 实现成立，安全边界待确认 | `state/src/lib.rs` 保留加固错误，`engine/src/run.rs:221-234` 记录；安装根与身份保护的失败策略属于不同权限边界，见待确认项。 |
| F5 ACL 对齐 | 成立 | `state/src/win.rs:37-43` 使用对齐的 `u64` 数组。 |
| F6 文案 ID 生成 | 成立 | `text/build.rs` 从清单生成常量；源代码不再维护手写枚举。 |
| F7 英文清单仅测试使用 | 成立 | `text/src/lib.rs` 中英语资源受 `cfg(test)` 约束。 |

### 1.2 当前代码核查

当前提交中，完整文件集合、平台和槽位校验集中在 `crates/launcher/src/install.rs:517-622`，遍历错误显式传播；`crates/client/src/main.rs:89-101` 根据主程序所在目录定位内核；心跳消息在 `crates/engine/src/run.rs:593-604` 校验连接代次；`crates/engine/src/update.rs:78-106` 将启动器后台互斥状态独立于客户端包 offer。安装状态与凭据确认尚存的问题及其代码依据见本报告 F1–F4、F6、F8、F9。

## 2. 本轮发现

### [P1][F1] 已恢复的旧版本可能在重试回滚时被删掉

- 位置：[launcher/src/install.rs:232-261](../../crates/launcher/src/install.rs#L232-L261)、[install.rs:186-196](../../crates/launcher/src/install.rs#L186-L196)。
- 条件：替换安装进入 `Installed`，回滚的目录恢复已经成功，但事务文件的清理失败或进程在清理前中止。
- 结果：状态文件仍为 `Installed`，备份已经改名回 `bin/`。再次 `abort()` 时，`undo()` 无从区分已经恢复的旧版本和首次安装留下的新版本，可能删除当前唯一可启动目录。当前 `Replaced` 的幂等规则不能覆盖该状态。
- 修复：保存安装前是否有旧版本及回滚进度，并建立恢复完成后的持久判据。每步目录变更与状态迁移均应可重复。修复方应验证旧版本恢复后，清理失败、再次恢复及再次安装均保持原版本完整。

### [P1][F2] `installed` 的启动确认与失败处理在入口之间不一致

- 位置：[launcher/src/main.rs:101-124](../../crates/launcher/src/main.rs#L101-L124)、[install.rs:207-226](../../crates/launcher/src/install.rs#L207-L226)、[install.rs:414-451](../../crates/launcher/src/install.rs#L414-L451)、[supervisor.rs:28-70](../../crates/launcher/src/supervisor.rs#L28-L70)、[launch.rs:45-66](../../crates/launcher/src/launch.rs#L45-L66)。
- 条件：已有完整的 `Installed` 事务，应用在确认前中断，或新版在启动校验、进程创建、等待确认阶段失败。
- 结果：恢复保留 `Installed`。Windows 普通启动不传启动确认 token，也没有提交事务；Windows 安装和 Linux 监督路径只对部分启动失败执行回滚。Windows 确认超时时没有持有新版子进程句柄，不能确定其已经退出。安装事务因此可以长时间停在未确认态，或者恢复旧版本时新版仍占用资源。
- 修复：将 `Installed` 的验证、启动、确认、停止、提交/回滚收口为同一事务出口，按平台封装进程句柄，而非拆分判据。验证失败、创建失败、确认超时和重启恢复均须有明确结果。Windows 与 Linux 的差异只需体现在进程托管方式。

### [P1][F3] 普通启动提交可能消费尚未安装的更新包

- 位置：[launcher/src/install.rs:397-408](../../crates/launcher/src/install.rs#L397-L408)、[supervisor.rs:58-83](../../crates/launcher/src/supervisor.rs#L58-L83)、[engine/src/update.rs:740-776](../../crates/engine/src/update.rs#L740-L776)。
- 条件：Linux 普通启动完成确认时，客户端同时完成一份已经请求安装的下载。
- 结果：`commit()` 在没有安装事务时仍调用 `clear_package()`。新包尚未由启动器消费，固定路径本身是交接信号；此时清理会丢失信号。相应单测 `commit_without_an_install_leaves_no_state_behind` 没有约束包的保留。
- 修复：只由消耗包的安装事务负责清理该包。更明确地将“下载完成”和“请求安装”关联至一次交接，普通启动确认只清理确认属于此前事务的残留。验证无事务提交不删除待装包。

### [P1][F4] 安装根的排他所有权未覆盖启动器

- 位置：[client/src/main.rs:74-84](../../crates/client/src/main.rs#L74-L84)、[install/src/lock.rs:17-31](../../crates/install/src/lock.rs#L17-L31)、[launcher/src/main.rs:46-63](../../crates/launcher/src/main.rs#L46-L63)、[launcher/src/install.rs:37-57](../../crates/launcher/src/install.rs#L37-L57)。
- 条件：操作者重复启动，或手工安装与正在安装/退出的客户端发生重叠。
- 结果：现存文件锁只约束客户端实例；启动器对 `install.state`、`bin.old`、`bin/`、待装包的检查、恢复与修改不受同一个排他约束。`--wait-for` 只等待某个进程退出，不建立事务所有权。当前状态机依赖单写者假设，但入口没有保证这一假设。
- 修复：明确安装根互斥协议及客户端与启动器的交接顺序。安装、恢复、验证和启动确认的相关写操作获得所有权后执行，释放时机应覆盖事务完整生命周期。以两个正常启动器实例的并发测试验证只有一个可以改变事务状态。

### [P2][F5] 更新策略与实际下载任务缺少一致的 offer 标识

- 位置：[engine/src/update.rs:140-156](../../crates/engine/src/update.rs#L140-L156)、[update.rs:228-241](../../crates/engine/src/update.rs#L228-L241)、[update.rs:275-299](../../crates/engine/src/update.rs#L275-L299)、[update.rs:383-408](../../crates/engine/src/update.rs#L383-L408)、[update.rs:513-522](../../crates/engine/src/update.rs#L513-L522)。
- 条件：下载尚在运行时发布记录或设备策略发生调整；同一版本号重发内容也是受支持的业务操作。
- 结果：全局 `auto_update` 先更新，而忙状态使新 offer 被忽略；撤回时 `clear_stale_update()` 也拒绝修改下载态。下载完成只能按版本匹配，可能按失效的授权及后来收到的策略安装旧 offer。`auto_tried` 同样仅按版本去重，与版本加摘要的 offer 判据不同。
- 修复：让 offer 身份及自动安装资格绑定到同一个状态，用版本、摘要及任务代次标识完成消息。撤回、同版本重发、策略变更均应有显式状态迁移，旧任务完成不得错误消费新状态。补下载中撤回及同版本重发的业务回归测试。

### [P2][F6] 启动器替换后版本记录和内存事实未更新

- 位置：[engine/src/run.rs:260-265](../../crates/engine/src/run.rs#L260-L265)、[engine/src/update.rs:553-574](../../crates/engine/src/update.rs#L553-L574)、[update.rs:647-670](../../crates/engine/src/update.rs#L647-L670)、[launcher/src/main.rs:45-53](../../crates/launcher/src/main.rs#L45-L53)、[main.rs:86-103](../../crates/launcher/src/main.rs#L86-L103)。
- 条件：同一个长时间运行的客户端完成了启动器替换，但尚未再次通过新启动器启动。
- 结果：启动器摘要与版本仅在引擎创建时读一次，替换成功只写日志。后续判断仍以旧版本为基础，无法保证版本单调递增，`launcher.json` 也不能同步表达刚完成的替换。旧的仍在运行的启动器还可能记录自身版本，进一步产生歧义。
- 修复：把安装目标版本、摘要与替换结果作为同一个事实管理，成功后更新引擎状态与持久记录；明确正在执行的启动器与盘上启动器何时相同。测试一次进程生命周期内多次发布、替换失败、重启恢复以及单调版本保证。

### [P2][F7] 启动器解压与手动下载缺乏统一资源预算

- 位置：[launcher/src/install.rs:472-510](../../crates/launcher/src/install.rs#L472-L510)、[install.rs:679-708](../../crates/launcher/src/install.rs#L679-L708)、[engine/src/update.rs:21-26](../../crates/engine/src/update.rs#L21-L26)。
- 条件：处理异常大、损坏、包含大量条目或传输中断的安装包，或者手工选择 URL 安装。
- 结果：启动器在验清单签名前扫描并读取清单，清单字符串、解压输出、条目数无明确上限；实际解包同样无预算。两种手工下载分支没有客户端所用的总字节/总时间约束，且直接落正式文件名。资源失败可能发生在旧版本移动之后，增大回滚负担。
- 修复：统一安装工件的字节预算、解压预算、清单和条目上限、总时限及临时文件提交规则，检查失败时保有已安装版本。以正常包、资源上限边界及文件写入失败为回归判据。

### [P2][F8] core 尚未确认就绪时，磁盘凭据被误当作已加载凭据

- 位置：[engine/src/sync.rs:357-376](../../crates/engine/src/sync.rs#L357-L376)、[sync.rs:400-434](../../crates/engine/src/sync.rs#L400-L434)。对端见 [server/api/device_api.go:328-337](../../../server/api/device_api.go#L328-L337)。
- 条件：已持久化续期凭据，随后 core 正处于启动探测、停止或启动失败状态。
- 结果：只要磁盘文件存在，`credential_id()` 就回退为它的 id。控制面将此解释为已成功加载并确认切换，违背根设计中旧凭据撤销的前提。现有测试 `credential_id_is_the_one_the_core_loaded`（`engine/src/tests.rs:315-332`）还明确断言未运行时回退磁盘值，修复时应同步改变该断言。
- 修复：协议区分已加载确认、持有但未确认、缺失与正式节点这几个实际状态；控制面只在真正确认后收敛旧凭据。两端同步修改契约及测试。不得把新私钥是否已经写盘当作 core 已成功运行的证明。

### [P2][F9] 安装事务持久性承诺超出了文件系统操作的实际保证

- 位置：[install/src/lib.rs:47-88](../../crates/install/src/lib.rs#L47-L88)、[launcher/src/install.rs:180-184](../../crates/launcher/src/install.rs#L180-L184)、[install.rs:358-389](../../crates/launcher/src/install.rs#L358-L389)。
- 条件：系统掉电或存储设备异常结束，事务状态与目录变更未同时持久化。
- 结果：`atomic_replace` 调用 `flush` 后 rename，但没有同步文件内容及目录元数据；安装代码据此承诺掉电后可确定恢复。进程级原子替换与持久化的保证不同，当前恢复规则依赖操作顺序已经持久化。
- 修复：确定想提供的持久性级别，使用符合目标平台的文件和目录同步边界，并按实际保证更新契约。若明确只保证进程中断恢复，应相应限制设计承诺，并确保异常磁盘形状安全停机。

### [P2][F10] 注册后的落盘与激活不是同一次状态转换

- 位置：[engine/src/settings.rs:54-97](../../crates/engine/src/settings.rs#L54-L97)、[run.rs:541-552](../../crates/engine/src/run.rs#L541-L552)。
- 条件：控制面接受注册并消耗令牌，客户端写入新身份后，删除旧凭据、停止 core 或写入新配置中的任一步失败。
- 结果：保存的 `client.json` 代表新状态，内存 `self.client` 和实际 core 却仍可能保持此前的连接或已停机；`handle` 仅在 `save_settings` 返回成功时触发重连。身份和配置不一致的状态没有恢复判据，用户看到失败后重新提交也可能因令牌已消耗而失败。切换控制面时只比较设备 ID 也不足以确定原凭据是否属于当前控制面。
- 修复：分别表达已注册、待激活、已激活的状态，使注册成功的身份能在后续本地操作失败时进入确定的恢复路径，新的连接与凭据/配置来自同一代次。验证本地任一步失败之后的重试、退出再启动及控制面切换，不要求再次消费注册令牌。

### [P3][F11] 面板查询缓存没有绑定 core 实例

- 位置：[panel/src/router.rs:177-183](../../crates/panel/src/router.rs#L177-L183)、[router.rs:253-299](../../crates/panel/src/router.rs#L253-L299)。
- 条件：core 重启、切换网络，或两个查询的完成顺序与发出顺序不同。
- 结果：两秒缓存只按时间判断有效，缺少 RPC 地址及实例代次。新 core 的快照可以与旧 core 节点和路由表组合，而且旧查询较晚完成也可覆盖新缓存。
- 修复：缓存关联实际 RPC 实例及查询时间，拒绝已失效实例的结果；查询失败与真实空拓扑应具有不同表示。验证两次 core 切换后的首屏属于当前实例。

### [P3][F12] 路由归并丢弃了解析中继路径需要的 peer ID

- 位置：[engine/src/views.rs:238-289](../../crates/engine/src/views.rs#L238-L289)、[views.rs:302-328](../../crates/engine/src/views.rs#L302-L328)、[views.rs:346-397](../../crates/engine/src/views.rs#L346-L397)。
- 条件：同一地址有两个 peer ID 的路由，由其它节点的下一跳仍引用其中未被选为展示行的 ID。
- 结果：按 IP 选择展示行后才建立路径解析器，映射中缺失未选中的 peer ID，路径解析可能提前结束。现有归并单测约束了展示行数量与优先选择，未覆盖其它路由引用被归并 ID 的情况。
- 修复：保留完整 peer ID 路由图用于路径推导，将按地址聚合限定在展示阶段。环或缺失下一跳应明确表达路径不完整，避免把部分路径当成完整路径。

## 3. 待确认与已接受边界

- 本地面板的授权模型仍只有回环及浏览器同源检查，代码明确允许本机原生客户端调用状态修改接口。需要确认普通本地账户、同账户非提权进程是否被视为可信操作者，依据是 `panel/src/guard.rs:3-6,24-35` 与 `router.rs:211-243`。若需隔离，建议设计明确本地操作者授权及特权操作的 OS 边界。
- 安装根加固失败目前只写日志，`state/src/win.rs:125-149` 对已有对象加固也没有完整路径的安全属性复核；Unix 按部署目录所有权作为前提。应明确安装目录与祖先目录的权限前提及验证失败时行为。生产环境 ACL、所有权、重解析点等保证尚未实测，列为待确认。
- 依据当前契约，明文 HTTP 可显式用于受信任内网（设置省略协议仍默认 HTTP），未把无迁移及内网 HTTP 的既有取舍单列为漏洞。对公网地址的默认 HTTPS 和用户提示可以单独确定策略。
- EasyTier 是独立上游内核，检查范围包括本仓库记录的构建与补丁接口；其完整协议、加密和转发实现需独立验证。未开展真实种子或设备环境的测试。

## 4. 验证

实际执行情况见下述验证记录，均在本地工作区运行，未启动线上客户端、未访问线上控制面或种子。注册、下载和 RPC 测试使用仓库内既有测试夹具。安全及事务问题经代码路径和契约复核，未编写攻击性复现脚本；修复方按各项不变量设计回归用例。

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `frontend: pnpm typecheck`、`pnpm build` | 通过 |
| `cargo test --workspace --locked` | 失败，首个失败目标为 `mesh-core` 的 `managed_core`（2 通过、5 失败）。 |
| `cargo test --workspace --locked --no-fail-fast -- --test-threads=1` | 280 项，255 通过、25 失败。`mesh-core/managed_core` 5 项、`mesh-engine` 4 项、`mesh-etrpc` 6 项、`mesh-remote` 10 项。多处明确返回回环 socket `os error 10013`，另有 core 子进程停止失败；该环境无法完成本地网络/进程托管集成验证。全部 30 项 launcher、37 项 install、18 项 state 测试通过。 |
| `cargo build --workspace --locked` | 通过，已使用本轮重新生成的前端资源构建 Windows 开发版。 |

未自动验证：Windows UAC/任务计划、真实内核与虚拟网卡生命周期、管理员权限下的 ACL、Linux musl 与 systemd 更新交接、物理掉电持久性。用户与修复方可依据这些限定独立做真机验收。
