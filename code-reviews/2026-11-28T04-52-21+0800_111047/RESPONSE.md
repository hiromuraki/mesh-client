# 客户端审查回应

- 对应审查：`code-reviews/2026-11-28T04-52-21+0800_111047/REVIEW.md`（审查提交 `111047f`）
- 回应时间：2026-11-28T05:55:00+08:00
- 范围：F1、F2、F3 与 F7 的两条实现项已处理；F4、F5、F6 与 F7 的其余三条留待后续。

## 结论

| 编号 | 级别 | 结论 | 一句话 |
| --- | --- | --- | --- |
| F1 | P2 | 已修复 | 只留一个事件循环，控制面与 core 各有自己的期限；`Status` 单点推导，心跳首次与周期走同一处理函数 |
| F2 | P2 | 已修复 | 本地身份代次 + `CoreSpec`：`core.spec == desired_spec` 是唯一「要不要换进程」判据；换绑先删旧工件再原子写 `client.json` |
| F3 | P2 | 已修复 | `UpdateState` 拆成 offer / 任务 / 工件 / 意图四个正交字段，意图只在真正授权的交接里提交 |
| F4 | P3 | 未修复 | A/B 安装槽会改 `bin/` 与运维契约，等裁决 |
| F5 | P3 | 未修复 | 删除 `appUpdateKnown`/`unchanged` 要同时改两端，另开一轮 |
| F6 | P3 | 未修复 | 拓扑视图迁 `mesh-panel` 属独立一项，下一轮 |
| F7 | P3 | 部分修复 | 收掉 2 条（`Failures::exhausted`、摘要实现进 `mesh-install`，engine 侧）；`core/thread.rs` 进程枚举、`panel/router.rs` 方法分组、launcher 侧摘要待下一轮 |

## 逐项回应

### [P2][F1] core 监督仍受控制面连接循环约束，运行状态存在多重来源

- 做法：删掉「先 `connect()` 再进内层循环」的两段结构，只剩一个事件循环（`run.rs` 的 `pump` / `next_deadline` / `on_deadline`）。`Control { gen, failures, due }` 只表达「下一次什么时候联系控制面」：请求在途时 `due` 为 `None`，例行与重试是同一个期限；core 重拉保留自己的 `Revive.due`。二者各到期各自推进，取最早者唤醒——控制面退避再也挡不住 core 重拉。
- 心跳首次与周期走同一个 `on_heartbeat` → `on_beat`；心跳与 STUN 的结果都按代次归属，旧代次直接丢弃，不再「晚到即应用」。
- `Status` 不再由各处理器写入，改为 `status()` 单点推导（输入：`fault`、有无客户端配置、最近心跳结果、core 是否就绪）；`Started` 不再借 `api.is_some()` 判在线，`api` 只剩「下载与启动器替换用连接」一个含义。
- 验证：`a_control_outage_does_not_stop_the_core_revival`、`the_first_and_periodic_heartbeats_share_one_response_handler`、`a_stale_heartbeat_result_is_ignored`、`a_stale_stun_result_is_ignored`、`a_heartbeat_without_a_running_core_starts_it`。

### [P2][F2] 身份代次、落盘配置与已应用规格缺少统一关系

- 做法：新增 `CoreSpec { generation, config_hash, hostname, auth }`（`auth` 二选一：`Formal` 或 `Credential(id)`），运行实例 `CoreRun.spec` 记录它**实际应用的那一份**。`ensure_core` 只比一次：期望规格相等就不动，不等就停旧起新。`credential_stale()`、`replace` 标志、改名专用停止路径全部删除——配置同步、改名、凭据轮换与重新绑定走同一条判据。
- 换绑按「先删旧一代的 `config.toml` 与 `credential.json`，再原子写新 `client.json`」排序，删除失败即保存失败（不再只记日志）。因此任何中断点读到的只能是旧一代、空、或新一代，不会出现「新身份配旧配置」。
- 取舍：**没有**采用 `config/` 下的代次目录，磁盘布局不变。理由是先删后绑加「删除失败即失败」已经给出确定性，而代次目录要改固定路径，牵动部署与运维操作，收益只是省掉一次删除。
- 验证：`the_same_spec_is_never_started_twice`、`a_credential_rotation_replaces_the_core`、`changing_the_device_name_replaces_the_core`、`an_unchanged_config_neither_rewrites_nor_restarts`、`a_changed_config_stops_the_old_core_before_starting_the_new_one`、`switching_the_control_plane_drops_the_previous_generation`、`a_failed_generation_cleanup_keeps_the_old_identity`、`an_interrupted_generation_switch_yields_no_core`。

### [P2][F3] 更新模型混合 offer、工件准备、下载任务与安装意图

- 做法：`UpdateState` 只剩四个正交字段——`offer: Option<Offer>`、`task: Option<DownloadTask>`、`artifact: Artifact`（`None` / `Staged(part)` / `Failed{…}`）、`intent: Option<Intent>`（`Manual` / `Auto`）。意图不复制 offer 身份：offer 变化或撤回时由同一次迁移清空，因此它永远只指向当前这一份。
- 固定名 `update.tar.zst` 只在**真正执行被授权的交接**时、在 `install.lock` 内由 `.part` 改名产生；其余时候磁盘上只有 `.part`。安装根被占只延期，意图与已验工件都保留，下一轮复用而不重下。自动意图在交接前重新校验当前策略；手动点击作废失败退避。
- 启动器槽位复用同一 `DownloadTask` 身份与 `mesh_install::artifact` 校验；原来的 `AtomicBool` 占位删除——任务本身就是单飞判据。
- 验证：`a_verified_artifact_waits_for_the_install_root_without_downloading`、`a_downloaded_package_is_not_committed_without_an_intent`、`an_auto_intent_is_dropped_when_the_policy_turns_off`、`a_manual_intent_survives_a_policy_change`、`a_new_offer_drops_the_intent_and_the_staged_artifact`、`a_verified_launcher_artifact_is_reused_without_downloading`、`withdrawing_an_offer_during_download_discards_the_task_result`、`a_republished_version_invalidates_the_download_in_flight`、`downloading_gives_up_after_the_attempt_limit`、`a_missing_package_is_downloaded_again_when_the_user_asks_to_apply`。

### [P3][F7] 多处局部实现仍重复表达同一规则

| 位置 | 本轮 |
| --- | --- |
| `update.rs` 的失败阈值 | 删除本地 `FAILURE_LIMIT`，改用 `Failures::load(...).exhausted(version)`，计数只用于日志 |
| `sync::file_digest` 与 `launcher/install.rs::sha256_file` | 抽进 `crates/install/src/artifact.rs`（`is_full_digest` / `digest_file` / `matches`），engine 与 install 侧已全部改用它 |
| `core/thread.rs` 的 `running`/`pending` 两个 Option | 未做，属 `crates/core`，下一轮 |
| `panel/router.rs` 的 `accepts()`/`allow()` | 未做，下一轮 |
| launcher 侧那份摘要 | 未做，下一轮（它带两条不同的用户可见错误文案，换成统一实现要一并决定文案，不能顺手改） |

- 验证：`a_version_with_two_install_failures_is_skipped`、install 的 `a_file_matches_its_own_digest_only`、`only_64_hex_digits_are_full_digests`、engine 的 `the_package_becomes_the_fixed_name_only_when_it_is_accepted`、`a_fresh_process_reads_the_launcher_facts_from_the_install_root`。

## 裁决答复

| 议题 | 决定 |
| --- | --- |
| 首次连接失败报 `degraded` 而非 `error` | 接受。契约本来就写「非 401 降级为 degraded」，为保留「首次 error」再加一个判据等于用状态换状态。 |
| 换代次清理失败使已消耗的注册令牌作废 | 接受。用一次人工重注册换「磁盘上永不出现新身份配旧配置」；失败文案要写清该怎么做（删 `config/` 重注册）。 |
| 磁盘布局仍平铺，无代次目录 | 接受，理由见 F2。 |
| 手动安装时若正在下载，改为记录意图、下载完成即交接 | 接受。这是「唯一转移」的一部分，旧行为返回「还在下载」等于多一条外部可观察的分支。 |

## 净变化

| 项 | 之前 | 之后 |
| --- | --- | --- |
| `UpdateState` 字段 | 15 | 4 |
| `Ready` 一词承载的语义 | 1 个枚举值兼表「收到 offer」「下载完等锁」「已提交」 | 拆成三条正交轴（offer / 工件 / 意图），不再存储 |
| `Status` 写入点 | 19 处文本写入 | 0（`status()` 单点推导），另有 5 处只写失败槽 `fault` |
| 「要不要换 core」的判据 | 7（同 hash、同内容、是否在跑、凭据是否过期、`replace` 标志、改名分支、启动分支） | 1（`core.spec == desired_spec`） |
| `CoreRun` 字段 | 6 | 5（`cred_id` 并入 `spec`，`started` 由 `pid != 0` 推出） |
| `Engine` 字段 | 38 | 35 |
| `update.rs` 顶层方法 | 23 | 20 |
| `update.rs` 行数 | 1062 | 889 |
| 删掉的分支/特例 | — | 约 16 处（两层连接循环、`Flow::Restart`、`wait()`、`closed`、`ConnectError`、按连接启停的心跳与 STUN 线程、`start_core(replace)`、`busy`、`apply_after_download`、`phase/path/sum/part` 字段、三条补充路径、`FAILURE_LIMIT`、启动器 `AtomicBool` 占位等） |
| 测试 | 引擎 100 | 引擎 112、install 39 |

## 文档

| 文件 | 改了什么 |
| --- | --- |
| `docs/contract/client.md` | 换代次「先删后绑」的本地状态约定；配置热更新改为「字节相同才写、规格相同才不动」；看守的两个独立期限；`Status` 推导表；心跳首次与周期同一路径、旧代次结果丢弃 |
| `docs/contract/updates.md` | 客户端「offer / 工件 / 授权」三件事的落点；Windows 与 Linux 交接改为「暂存不是信号，授权交接才提交，锁争用延期复用、不重下」 |

## 验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过（工作区全部 crate，含 `mesh-panel`、`mesh-client`、`mesh-launcher`） |
| `cargo test --workspace` | 全部通过（engine 112、install 39，其余 crate 照旧） |

未验证：真实 core 与 TUN、Windows UAC 与安装根占用、systemd 交接、进程被强杀后的中断恢复。下一版客户端发出去后按设备观察。

## 未修复项

| 编号 | 事项 | 原因 |
| --- | --- | --- |
| F4 | A/B 安装槽 | 会改固定 `bin/` 与部署契约，等裁决 |
| F5 | 删除 `appUpdateKnown` / `unchanged` | 同时改两端与服务端字段，另开跨端一轮 |
| F6 | 拓扑视图迁 `mesh-panel` 的 `topology.rs` | 独立一项，下一轮 |
| F7 余项 | `core/thread.rs` 进程生命周期枚举、`panel/router.rs` 方法分组、launcher 侧唯一摘要实现 | 下一轮 |
