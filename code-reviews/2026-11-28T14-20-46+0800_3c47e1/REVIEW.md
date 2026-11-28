# 客户端代码审查

- 审查提交：`3c47e133f93f781bd9f5a7c47dad2bac13e54080`
- 分支：`main`
- 开始时间：2026-11-28T14:20:46+08:00
- 工作区：开始时干净。
- 范围：安全、工程质量、新双槽启动链、状态模型、资源管理及上轮修复复核。
- 对照：`DESIGN.md`、`README.md`、`MODULES.md`、`docs/states.md`、上级 `docs/contract/` 与 `docs/security.md`。

## 上轮复核

复核区间为 `111047f..3c47e13`。检查 `git log -- code-reviews`、提交历史、差异统计、[上轮回应](../2026-11-28T04-52-21+0800_111047/RESPONSE.md) 及现有实现。下表为静态结论，测试限制见文末。

| 上轮编号 | 结论 | 依据 |
| --- | --- | --- |
| F1 | 接受 | 单事件循环、独立期限、心跳统一入口与 STUN 代次均已实现。 |
| F2 | 部分接受 | 先删后绑与 `CoreSpec` 已统一磁盘代次及规格比较，运行实例在无期望规格时仍未停止，见本轮 F1。接受保持平铺磁盘的取舍。 |
| F3 | 部分接受 | offer、task、artifact、intent 已独立，暂存与交接分开。失败计数和交接结果仍有遗漏，见本轮 F4、F5。 |
| F4 | 接受方案 | `bin/alice`、`bin/bob`、`bin/current` 已实现，旧五态目录事务删除。确认及失败路径有新的边界问题，见本轮 F2、F3。 |
| F5 | 接受 | 完整 offer 已取代增量协议，需补同一工件的元数据刷新，见本轮 F4。 |
| F6 | 接受 | 拓扑构造及相关测试已迁入 `mesh-panel`。 |
| F7 | 接受 | core 生命周期枚举、失败停止判据、摘要实现及路由方法集合均已收口。 |

## 审查发现

### [P1][F1] 身份切换后无期望 core 规格，旧数据面仍继续运行

位置：[engine/settings.rs:95](../../crates/engine/src/settings.rs#L95)、[engine/settings.rs:113](../../crates/engine/src/settings.rs#L113)、[engine/run.rs:985](../../crates/engine/src/run.rs#L985)、[engine/run.rs:1035](../../crates/engine/src/run.rs#L1035)、[engine/sync.rs:262](../../crates/engine/src/sync.rs#L262)。

换绑时 `bind()` 删除旧配置和凭据，保存新身份后调用 `ensure_core_or_report()`。由于新绑定尚未取得配置，`desired_spec()` 返回 `Ok(None)`，`ensure_core()` 随即成功返回，没有停止已经存在的旧 core。

结果是控制面身份已经切换，数据面仍保持上一代连接。新控制面不可达时该状态可一直存在。凭据报告也可能取到尚在运行的旧实例规格。旧数据面与当前身份的关系不能只由磁盘文件的清理保证。

现有测试 [tests.rs:1965](../../crates/engine/src/tests.rs#L1965) 设有已运行 core，模拟配置与凭据被删除后反而要求不产生 core 命令。这把「没有可启动规格」误当成「不需要改变当前实例」。

建议将期望规格比较定义为完整的收敛规则，`None → 已运行` 应有明确停止动作。换绑及本地身份清除须确认旧数据面停止，无法确认时保留可观测过渡状态并重试停止。控制面身份代次变化时同步重置认证失败计数及其所属授权。补充运行中换绑、控制面不可达和部分清理失败的状态测试。

### [P2][F2] 双槽启动确认可能把原有 current 当成新一次确认

位置：[launcher/install.rs:118](../../crates/launcher/src/install.rs#L118)、[launcher/install.rs:143](../../crates/launcher/src/install.rs#L143)、[launcher/install.rs:321](../../crates/launcher/src/install.rs#L321)、[launcher/install.rs:399](../../crates/launcher/src/install.rs#L399)、[launcher/install.rs:912](../../crates/launcher/src/install.rs#L912)。

`prepare()` 在修复指针失败时仍返回选中槽。两个槽都无效时直接返回 `None`，也不清除现存指针。后续安装可能使原来无效的目标槽成为候选槽。`wait_current()` 只判断值是否等于候选槽名，无法区分安装之前的同值与客户端成功启动后的写入。

因此在指针修复失败或旧指针遗留的恢复形状中，启动器可能在候选客户端实际完成初始化前认定成功，并执行旧槽清理。A/B 模型需要持续保持「开始试运行前，current 不得已经等于本次候选」这一不变量。

建议在启动候选之前明确检查安装选择记录与目标槽的关系。必要的指针修复失败必须使安装失败并保留有效槽。首次安装及无有效槽恢复要明确处置过期指针。保留客户端提交权，但只有确定属于本次试运行的提交才允许清理回滚材料。覆盖指针缺失、指向无效槽、修复写失败及候选提前退出等正常故障形状。

### [P2][F3] 候选启动失败、停止失败及 Linux 手动安装收尾的结果未完整传播

位置：[launcher/install.rs:321](../../crates/launcher/src/install.rs#L321)、[launcher/install.rs:356](../../crates/launcher/src/install.rs#L356)、[launcher/install.rs:493](../../crates/launcher/src/install.rs#L493)、[launcher/install.rs:528](../../crates/launcher/src/install.rs#L528)、[launcher/supervisor.rs:60](../../crates/launcher/src/supervisor.rs#L60)。

`boot_candidate()` 的 `self.start(...) ?` 在进程创建失败时直接返回，未记入版本失败计数。重复获取同一版本时会反复尝试一次必然失败的启动。确认失败后的 `candidate_failed()` 只记停止错误，却仍返回与已经成功停止相同的错误结果，调用方可能继续按普通恢复路径拉起旧槽。

Linux 手动安装的成功分支在确认候选后尝试停止临时客户端，停止失败只记日志，最终返回 `Ok(())`。systemd 接管时可能遇到仍持有实例锁的临时进程。该问题与进程资源所有权和启动器停止语义直接相关。

建议候选启动结果区分「创建失败」「确认失败且已停止」「停止未确认」和「成功」。前两种按版本记失败，停止未确认则保留进程事实并禁止自动回退或宣称安装完成。Linux 手动确认后的临时进程停不掉应返回明确失败。保持现有 A/B 目录布局，不需要恢复旧五态事务。

### [P2][F4] 更新重试次数在实际下载前被清空，同一工件元数据又未刷新

位置：[engine/update.rs:272](../../crates/engine/src/update.rs#L272)、[engine/update.rs:430](../../crates/engine/src/update.rs#L430)、[engine/update.rs:444](../../crates/engine/src/update.rs#L444)、[engine/update.rs:470](../../crates/engine/src/update.rs#L470)、[engine/update.rs:536](../../crates/engine/src/update.rs#L536)、[engine/tests.rs:1052](../../crates/engine/src/tests.rs#L1052)。

`prepare()` 从 `Artifact::Failed` 取出已有 attempts，起下载任务时把 artifact 设为 `None`。实际任务失败后 `on_download()` 从当前 artifact 重新计算次数，此时通常只能得到 1。因此真正的连续下载失败不会稳定累积到 `MAX_DOWNLOAD_ATTEMPTS`，退避也反复回到第一档。

现有次数上限测试直接构造已失败 5 次的状态，覆盖了上限判断，未覆盖失败次数实际累积的迁移。另一个问题是完整 offer 的版本和摘要相同，即只更新 `auto`，不刷新 `url`、`notes`。发布方搬迁同一工件地址后，客户端仍可能尝试旧地址。

建议失败次数随同一 offer 或任务明确传递，整个下载完成与重试迁移维持计数，手动重试或 offer 更换才清零。相同工件的完整 offer 更新 URL、说明和授权，已验证暂存件及在途任务保持独立。加入连续失败至上限和同一工件地址变更的回归测试。

### [P2][F5] 客户端包交接失败后，已授权工件和意图的状态丢失

位置：[engine/update.rs:370](../../crates/engine/src/update.rs#L370)、[engine/update.rs:392](../../crates/engine/src/update.rs#L392)、[engine/update.rs:407](../../crates/engine/src/update.rs#L407)、[engine/update.rs:566](../../crates/engine/src/update.rs#L566)、[engine/update.rs:611](../../crates/engine/src/update.rs#L611)、[launcher/install.rs:432](../../crates/launcher/src/install.rs#L432)。

`try_apply()` 提交固定待装包并清空 artifact、intent，随后才调用 `handoff()`。Windows 启动器不存在、进程创建失败等情形使交接返回错误，但引擎已经丢失上述状态，固定待装包留存。下一轮无法从状态判断是否应恢复原意图或重新下载，自动尝试标记也已消耗。

启动器自身替换任务与客户端交接还可以同时进行。其磁盘写操作受同一安装锁保护，但 Windows 客户端派生启动器之后立即退出，尚未确认新进程已接收安装任务。启动器此时若遇到安装锁争用，可能无法接收交接。

建议明确区分「工件准备完成」与「安装事务已接收」。交接失败保持可恢复的工件和失败事实，避免重新下载。客户端包交接与启动器替换的并发决策应收口至引擎，在替换任务尚未完成时延期客户端交接，或采用明确的接收确认。现有进程间排他锁保持不变。

### [P2][F6] 启动器归档与已安装目录的资源预算尚未闭合

位置：[launcher/install.rs:30](../../crates/launcher/src/install.rs#L30)、[launcher/install.rs:51](../../crates/launcher/src/install.rs#L51)、[launcher/install.rs:456](../../crates/launcher/src/install.rs#L456)、[launcher/install.rs:629](../../crates/launcher/src/install.rs#L629)、[launcher/install.rs:711](../../crates/launcher/src/install.rs#L711)、[install/manifest.rs:114](../../crates/install/src/manifest.rs#L114)。

当前 200 MiB 压缩包上限主要用于下载，手动 `--file` 路径先对文件完整求摘要，再读归档，未使用同一文件大小检查。归档读清单和解包各自新建 120 秒预算，解压器没有显式窗口或内存预算。日常选槽的 `manifest::read()` 则直接读取整个清单文件，未采用安装时 64 KiB 上限。

控制面与启动器的归档条目及解压总量上限也不同，已在服务端本轮报告列出。应明确同一工件格式和资源预算的契约，所有进入安装的包执行相同大小检查，解压器约束与解压总预算分别设置，已安装清单读取也受同样的上限约束。避免为格式合规而重复进行不受统一期限控制的完整扫描。

### [P2][F7] 迁移兼容的启动确认参数未经限定

位置：[client/cli.rs:92](../../crates/client/src/cli.rs#L92)、[client/main.rs:178](../../crates/client/src/main.rs#L178)。

一次性 `--boot-check` 兼容分支直接使用外部参数构造标记文件路径，入口未校验标记的合法字符及长度。客户端通常以高权限执行，兼容标记应严格限定在预期安装根内。

建议在迁移完成后按现有 `PENDING.md` 约定删除该入口。过渡期间限定为旧启动器实际生成的固定形状标识，并验证生成的标记路径作用域。此项为静态边界审查，未开展主动验证。

### [P3][F8] 正常无变化心跳进入配置正文缺失路径

位置：[engine/run.rs:611](../../crates/engine/src/run.rs#L611)、[engine/run.rs:622](../../crates/engine/src/run.rs#L622)、[engine/sync.rs:105](../../crates/engine/src/sync.rs#L105)。

`on_beat()` 无条件调用 `apply_remote`。正常 204 或仅带发布信息的响应没有配置正文，仍进入 `toml.is_empty()` 分支并记录「配置变化但未带正文」。已存在配置时还会重复执行 `ensure_core_or_report()`。

建议以配置下发是否存在为唯一入口条件，只有明确存在配置交付时才执行解码和应用。正常空响应不应记录异常。规格比较和状态汇合保留一个最终入口。

## 已知安全事项复核

上级 `PENDING.md` 中以下未排期事项仍可在当前实现找到对应行为。保持原有编号，避免另建重复整改任务。

| 编号 | 本轮结论 |
| --- | --- |
| C2 | 未关闭。[run.rs:268](../../crates/engine/src/run.rs#L268) 的权限收紧失败仍继续运行，Unix 安装根权限未形成 fail-closed 检查。维持最高整改优先级。 |
| C1 | 未关闭。[panel/guard.rs](../../crates/panel/src/guard.rs) 仍以回环及浏览器同源条件为主，建议实现或明确界定本地管理身份边界。 |
| C3 | 未关闭。完整工件签名与启动器交接之间的身份信息仍未持久绑定，Linux 交接仍仅依赖包名与内层清单。 |
| C4 | 未关闭。[settings.rs:176](../../crates/engine/src/settings.rs#L176) 默认补 HTTP，设备面的签发数据依赖传输完整性及机密性。建议默认并要求 HTTPS，开发环境如确需明文则显式限定。 |
| C6 | 未关闭。[run.rs:1013](../../crates/engine/src/run.rs#L1013) 的凭据启动规格未包含有效性判断，Held 状态仍缺少失效后的确定恢复语义。 |

## 待确认项

- 启动器自身更新在接收完整 offer 后是否允许在其撤回或绑定变化后继续完成替换。当前后台任务已直接持有提交能力，结果代次只在完成后更新内存状态。应先确定产品语义，再决定是否将替换授权统一移至引擎。
- 双槽一次性迁移的实际结束条件。确认所有在管设备已完成迁移后，移除兼容入口及旧磁盘布局清理路径。
- 真实管理员权限、Windows UAC、systemd 和进程停止失败路径仍需整机验证。

## 验证

| 命令或检查 | 本轮结果 |
| --- | --- |
| `git log -- code-reviews`、`git diff --stat 111047f..3c47e13` | 已复核。 |
| `cargo fmt --all --check` | 通过。 |
| `cargo test --workspace --locked --offline` | 编译通过，测试在 `mesh-core/managed_core` 因本机网络权限错误停止，2 通过、5 失败。 |
| `cargo test --workspace --locked --offline --no-fail-fast` | 继续运行各目标，失败目标为 `mesh-core/managed_core`、`mesh-engine`、`mesh-etrpc`、`mesh-remote`。输出出现回环 RPC/HTTP 的 Windows 10013 访问权限错误。 |
| `cargo test -p mesh-engine --lib --locked --offline --quiet` | 91 通过、8 失败。失败路径包括回环 HTTP 注册、心跳和下载，日志显示 10013，不能据此认定代码回归。 |

本轮未执行 clippy、前端构建、实际网络设备操作、主动安全验证或额外故障探针。上述 F1–F8 来自可定位的静态代码路径，测试未在具备完整回环访问能力的环境复跑。审查未修改生产及测试代码。
