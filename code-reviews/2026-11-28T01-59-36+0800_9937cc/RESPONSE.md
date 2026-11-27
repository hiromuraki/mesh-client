# 客户端审查回应

- 对应审查：`code-reviews/2026-11-28T01-59-36+0800_9937cc/REVIEW.md`（审查提交 `9937cca`）
- 回应时间：2026-11-28T03:53:11+08:00
- 范围：本轮 12 项全部处理；另附带一处审查未列出、修复过程中发现的缺陷。

## 结论

| 编号 | 级别 | 结论 | 一句话 |
| --- | --- | --- | --- |
| F1 | P1 | 已修复 | 回滚前先落 `restoring`，`bin.old/` 不在只可能是换回已完成，不再退化成删 `bin/` |
| F2 | P1 | 已修复 | 验证/拉起/确认/停止/提交收口成 `boot_and_settle`；Windows 持有子进程句柄，停不下来就不回滚 |
| F3 | P1 | 已修复（两端） | `commit()` 无事务时只清安装产物；引擎只在「这份 offer 仍被接受」时把 `.part` 改成固定名，且永不删除它 |
| F4 | P1 | 已修复（两端） | `install.lock` 由启动器与客户端共用，写安装根的动作只在所有权内；常驻启动器只在改安装根的那一段持有 |
| F5 | P2 | 已修复 | offer 身份 = 版本+摘要，下载任务带代次；撤回、同版本重发都是显式迁移 |
| F6 | P2 | 已修复（进程内） | 替换结果回到引擎线程，盘上事实随更新；跨进程单调性明确不承诺，写进契约 |
| F7 | P2 | 已修复 | 清单/解包/下载共用一组预算，全部在动 `bin/` 之前判完；下载先落 `.part` |
| F8 | P2 | 已修复（客户端侧） | 凭据四态：只有 core 确认加载过才上报 id；磁盘有未确认与正式节点都报空串 |
| F9 | P2 | 已修复（收窄承诺） | 持久性明确只到「进程中断可恢复」，掉电与存储异常不在承诺内 |
| F10 | P2 | 已修复 | 已注册/待激活/已激活由磁盘推出；写配置与换 core 只有一处；换代次作废旧代次的凭据、配置与 offer |
| F11 | P3 | 已修复 | 缓存绑定 `(rpc 地址, core 代次)`，旧实例的晚到结果不写回；查询失败与真空拓扑分开（`null` / `[]`） |
| F12 | P3 | 已修复 | 路径推导用完整 `peer_id` 路由图，归并只在展示阶段；成环与悬空下一跳标记 `pathIncomplete` |

## 逐项回应

### [P1][F1] 已恢复的旧版本可能在重试回滚时被删掉

- 做法：`Tx` 增加 `restoring` 状态，回滚**先**落盘再动目录；顺序是「`bin/` → `bin.stale/`、`bin.old/` → `bin/`、删 `bin.stale/`」，每一步可重复。`bin.old/` 不存在因此只可能是换回已经做完，`undo` 不再把它当成「首次安装没有备份」而去删 `bin/`。`clear_backup` 统一清 `bin.old/` 与 `bin.stale/`；`abort()` 在没有事务或已提交时也清产物残留。
- 位置：`crates/launcher/src/install.rs`（`Tx`、`undo`、`restore`、`clear_backup`）、`crates/install/src/layout.rs`（`BIN_STALE`）。
- 验证：`a_restored_version_survives_a_second_rollback`、`a_half_finished_restore_only_cleans_up`。变异验证：去掉 `restoring` 落盘后前一用例立即失败。

### [P1][F2] `installed` 的启动确认与失败处理在入口之间不一致

- 做法：`Owner::boot_and_settle` 是唯一出口——验证、拉起、等确认、停止、提交或回滚都在里面；`abort()` 在没有在途安装时是空操作，因此「普通启动失败不删文件」由同一段代码自然满足，不再按入口分支。Windows 现在持有新版子进程句柄（`SHELLEXECUTEINFOW` 的 `hProcess` 存进 `OwnedHandle`）：确认超时先 `TerminateProcess` 再等它真的退出，**停不下来就不回滚**，把 `installed` 留给下一次启动。Windows 普通启动若发现客户端已在跑（与客户端同一个 `instance.lock` 判据），直接记提交，不再起第二个进程。
- 位置：`crates/launcher/src/install.rs`、`launch.rs`（`Client`、`Slot`、`runas`）、`supervisor.rs`、`main.rs`。
- 验证：`a_new_version_that_fails_verification_is_rolled_back`、`a_new_version_that_cannot_start_is_rolled_back`（Linux 上同样通过）。
- 未自动验证：确认超时（要真拉起客户端等 60 秒）与崩溃后重启恢复的整机路径，只有代码路径与契约。

### [P1][F3] 普通启动提交可能消费尚未安装的更新包

- 做法：启动器侧 `commit()` 在没有在途安装时只清安装产物（`bin.old/`、`bin.stale/`），不再调 `clear_package()`；包只由消耗它的安装事务删。引擎侧把下载拆成「落 `.part` 并核对摘要」与「提交」两步：`download_package` 只返回 `.part` 路径，改名成固定路径由引擎在「结果仍属于当前 offer 且任务代次有效」之后做，任何作废路径只删自己的 `.part`，**永不删除 `update.tar.zst`**。
- 位置：`crates/launcher/src/install.rs`（`commit`、`clear_package`）、`crates/engine/src/update.rs`（`download_package`、`commit_package`、`on_download`、`clean_update_dir`）。
- 验证：`commit_without_an_install_leaves_no_state_behind` 补上「包仍在」的断言；`the_engine_never_clears_the_pending_package`、`the_package_becomes_the_fixed_name_only_when_it_is_accepted`。
- 附带修掉一个既有缺陷：`clean_update_dir` 用 `remove_dir_all` 处理 `update/` 下的**文件**，两个平台都失败，临时文件从来没被清掉；改为按类型删除。

### [P1][F4] 安装根的排他所有权未覆盖启动器

- 做法：`install.lock`（`mesh_install::lock::owner_path`）是「谁在改安装根」的唯一判据，写 `install.state`、`bin/`、`bin.old/`、`bin.stale/`、`update.tar.zst` 的动作只在 `Owner` 令牌内执行。客户端侧两处写入（包的最终改名、替换启动器）也取这把锁，且只在改名那一刻持有，`drop` 之后才做交接。Linux 常驻启动器每轮只在「`recover()` → `apply_pending()` → `boot_and_settle()`」这一段持有，等客户端期间放锁——否则客户端永远提交不了更新。冲突不靠这把锁预防：客户端在跑时手工 `--file` 由 `client_running`（`instance.lock`）拒绝，除非带 `--wait-for`。
- 位置：`crates/install/src/lock.rs`、`layout.rs`、`crates/launcher/src/install.rs`（`Owner`、`own`）、`crates/launcher/src/supervisor.rs`、`crates/engine/src/update.rs`（`own_install_root`、`resume_commit`、启动器线程）。
- 验证：`a_second_launcher_cannot_touch_the_install_root`、`committing_a_download_waits_for_the_install_root`、`a_launcher_replacement_blocked_by_the_install_root_is_retried`。

### [P2][F5] 更新策略与实际下载任务缺少一致的 offer 标识

- 做法：判重、自动安装资格、撤回、同版本重发统一按「版本+摘要」成对判断；`auto_update` 从引擎字段挪进 offer，精简响应只改这份 offer 的授权，不再先改全局开关。下载完成消息带任务代次，`on_download` 要求代次与 offer 同时匹配，否则作废并按需接手新 offer。撤回即使正在下载也立刻清 offer，但保留在跑的任务（`.part` 只有一个），旧结果回来必然过期。`auto_tried` 改为按摘要去重。
- 位置：`crates/engine/src/update.rs`、`run.rs`。
- 验证：`withdrawing_an_offer_during_download_discards_the_task_result`、`a_republished_version_invalidates_the_download_in_flight`、`auto_apply_is_a_property_of_the_offer`、`a_republished_version_replaces_the_offer`、`the_switch_is_taken_as_given_with_a_local_veto`。

### [P2][F6] 启动器替换后版本记录和内存事实未更新

- 做法：替换结果经 `Cmd::Launcher{version, sha256, replaced, retry}` 回到引擎线程；成功则把 `launcher.sum`/`version` 更新为盘上那份的新事实，同一进程内多次发布按单调版本判断。启动时的读取收口为 `LauncherState::read`，代码与契约都写明：`sum`/`version` 描述**盘上**那份，正在运行的启动器只在本进程没替换过时与之相同；`launcher.json` 只由启动器自己写，引擎不写也不中途重读。
- 位置：`crates/engine/src/update.rs`、`run.rs`。
- 验证：`a_replaced_launcher_updates_the_facts_and_keeps_the_version_monotonic`、`a_failed_launcher_replacement_keeps_the_facts`、`a_fresh_process_reads_the_launcher_facts_from_the_install_root`、`a_launcher_newer_than_the_recorded_one_passes`。
- **未闭合（明确接受）**：跨进程单调性。盘上已换新、长驻的旧启动器仍可能在之后写回它自己的版本记录；彻底闭合要么让启动器每次启动校验/重写记录，要么允许引擎写 `launcher.json`（那要推翻「只有启动器自己写」的约定）。代价与收益不成比例，本轮不做，已在 `docs/contract/updates.md`「启动器」写明。

### [P2][F7] 启动器解压与手动下载缺乏统一资源预算

- 做法：一组预算集中在 `Budget`：条目数 `1024`、清单 `64 KiB`、解压后 `256 MiB`、读清单+解包 `120 秒`、包 `200 MiB`、下载 `300 秒`。全部在**动 `bin/` 之前**判完（含条目名必须是单层普通文件名、同名条目拒绝），解包时再按同一组预算逐块兜底。下载先落 `update.tar.zst.part`，字节与时限在流里判，成功才改名；Linux 的 curl 分支加 `--max-filesize/--max-time` 后再判一次尺寸。
- 位置：`crates/launcher/src/install.rs`（`Budget`、`read_manifest_with`、`unpack`、`download`、`copy_bounded`）、`crates/install/src/manifest.rs`（`is_plain_file_name`）。
- 验证：`a_package_with_too_many_entries_is_refused`、`a_package_with_an_oversized_manifest_is_refused`、`a_package_that_declares_too_much_is_refused`、`unpacking_stops_at_the_byte_budget`、`an_expired_budget_stops_the_scan`、`a_download_over_its_budget_leaves_no_part_file`、`a_download_that_misses_its_deadline_leaves_no_part_file`、`a_package_with_a_traversing_entry_is_refused`。
- 未验证：真实等满 120 秒/300 秒的时限，只用「已过期预算」验证判据。

### [P2][F8] core 尚未确认就绪时，磁盘凭据被误当作已加载凭据

- 做法：`CredentialState` 四态——`Formal`（配置里有 `network_secret`）、`Missing`、`Held(id)`（磁盘有、core 未确认）、`Loaded(id)`（core 已确认加载），由 `hints.formal()` + `core.started && core.cred_id` + 磁盘凭据现算，不缓存、不新增状态。上报值：`Loaded → id`、`Missing → none`、`Formal|Held → 空串`。`credential_stale` 改为「core 正在用 ≠ 磁盘那张」，磁盘没有而 core 还在用旧凭据也算过期。
- 位置：`crates/engine/src/sync.rs`。
- 验证：`credential_id_is_only_the_one_the_core_confirmed`、`a_missing_credential_is_reported_as_none`、`a_formal_node_reports_an_empty_credential_id`。
- **未做（有意）**：不新增 `credentialHeld` 协议字段。空串已经表达「既不确认也不签发」，覆盖正式节点与「拿到但没加载」两种情形；新增字段要同时改 `crates/remote`、`views.rs`、`device-api.md` 与控制面的冷却判据，而它解决的问题（设备卡在「拿到凭据但 core 起不来」）在现行语义下不产生错误状态：旧凭据要等新凭据被确认才撤销，没确认就一直有效。等真有这种设备再按需加。

### [P2][F9] 安装事务持久性承诺超出了文件系统操作的实际保证

- 做法：收窄承诺而不是补 `fsync`。契约明确只保证「进程中断（被杀、崩溃、重启）后可确定恢复」，掉电与存储异常不在承诺内（只 `flush` 后改名，不同步文件与目录）。理由：补平台专用目录同步要引入新的系统调用面与失败面，而安装是可「重下再装」的重试动作；同时把异常形状压到最小——回滚从不在旧版本回到 `bin/` 之前删掉它，状态与目录矛盾时按错误停机而不是猜。
- 位置：`docs/contract/updates.md`「持久性」、`crates/install/src/lib.rs`（`atomic_replace` 文档）。
- 验证：契约 + 状态机测试（`restoring` 相关用例覆盖每一步中断可重复）。

### [P2][F10] 注册后的落盘与激活不是同一次状态转换

- 做法：状态由磁盘推出，不加新标记——已注册（`client.json` 有 `server_url`+`device_id`）、待激活（已注册且 `config.toml` 不存在）、已激活（心跳把这一代配置交下来并写入）。注册（消费 token）之后只做「身份落盘 + 作废旧代次 + 交给连接循环」，本地任一步失败只记日志并返回成功，重试走同一条路。注册处理里**不写** `config.toml`：写配置与换 core 只有 `apply_locked` 一处。换代次（控制面地址或设备 id 变）作废上一代的 `credential.json`、`config.toml` 与更新 offer。
- 位置：`crates/engine/src/settings.rs`、`run.rs`。
- 验证：`registering_persists_the_identity_and_leaves_activation_to_the_loop`、`a_registered_identity_is_activated_by_a_plain_heartbeat`、`switching_the_control_plane_drops_the_previous_generation`、`re_registering_on_the_same_control_plane_keeps_the_local_state`、`changing_the_device_name_replaces_the_core`、`saving_without_a_token_needs_an_existing_identity`。

### [P3][F11] 面板查询缓存没有绑定 core 实例

- 做法：`Instance = (rpc 地址, core 代次)`，缓存命中要求实例相同且 2 秒内；结果回写前再读一次 `StateCell`，查询期间 core 换代就丢弃、不覆盖新缓存。查询失败与真空拓扑分开：core 没在跑 → `[]`；core 在跑但 `ListPeer`/`ListRoute` 任一失败 → `null`，且失败不进缓存。快照与节点表取自同一次 `cell.get()`。
- 位置：`crates/panel/src/router.rs`、`crates/engine/src/views.rs`（`Published.core_gen`）。
- 验证：`a_cached_topology_belongs_to_one_core_instance`、`a_late_result_from_a_dead_instance_never_overwrites_the_cache`、`a_result_older_than_the_ttl_is_not_served`、`dashboard_with_an_unreachable_core_reports_an_unknown_topology`、`dashboard_without_a_running_core_has_an_empty_node_list`。
- 前端已消费：`nodes` 保留 `null`（不再折成 `[]`），未知时指标显示 `—`、表格不渲染。改掉的旧断言 `dashboard_with_an_unreachable_core_still_lists_the_local_node` 正是 F11 否定的行为。

### [P3][F12] 路由归并丢弃了解析中继路径需要的 peer ID

- 做法：`PathResolver::new(live, all)`——展示行的按地址归并只取跳数与主机名，`peer_id → IP` 映射用完整路由表建，被归并掉的 `peer_id` 仍可作为下一跳解析。推导结果改为 `Path::Complete | Path::Incomplete`，悬空下一跳与成环都显式标为不完整（`pathIncomplete`），`path` 保留已走到的那一段；只 memo 完整路径。
- 位置：`crates/engine/src/views.rs`、`crates/panel/.../format.ts`、`DataView.vue`、`TopologyView.vue`。
- 验证：`a_merged_peer_id_still_resolves_as_a_next_hop`、`a_cycle_is_reported_as_an_incomplete_path`、`a_dangling_next_hop_is_reported_as_an_incomplete_path`（后两条老代码回空数组）、`relay_routes_carry_the_hop_chain` 增补 `!path_incomplete`。
- 前端：路径列 `a → b → …` / 「路径不完整」，拓扑把通向目标的那一段画成灰色点线并标注；`isRelay()` 改看 `pathKind`，避免空 `path` 的不完整行被当成直连。

## 文档与文案

| 文件 | 改了什么 |
| --- | --- |
| `docs/contract/updates.md` | 安装根所有权（三行持有窗口 + 「拿不到锁就失败、不丢包」）、持久性承诺收窄、启动器侧预算、`restoring` 状态、客户端 `.part` → 固定名的交接、启动器盘上事实的语义 |
| `docs/contract/client.md` | `pathIncomplete`、节点表三态（`[]` / `null` / 行数组）、归并只在展示阶段、凭据四态判据表、已注册/待激活/已激活与换代次规则 |
| `client/MODULES.md` | `§3.6` `Published.core_gen`、`§3.7` 缓存绑定实例、`§3.9` 启动器与事务状态、`§6` 并发模型（两个锁各回答什么） |
| `client/DESIGN.md` | 「路径完整性是数据，文案仍归前端」 |
| `crates/text/lang/{chinese,english}.lang` | 新增 `launcher.error.install_root_busy`：安装根被另一个启动器占着时的专用文案 |

## 验证

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 |
| `cargo test --workspace --locked --no-fail-fast` | **313 项通过、0 失败**（本轮前 `mesh-engine` 96 → 100、`mesh-launcher` 30 → 43、`mesh-install` 37 不变、`mesh-panel` 29） |
| `pnpm typecheck` / `pnpm build`（`C:\Users\11717\AppData\Roaming\npm\pnpm.cmd`） | 均通过；前端无测试脚本 |
| 前端真实渲染（临时 `uv --with playwright` + 系统 Chrome headless，拦截 `/api/*`） | `nodes: null` 只出未知提示、表格 0 个、指标全 `—`；`nodes: []` 只出「暂无节点数据」；行数组下路径列为 `直接可达` / `ali-seed` / `ali-seed → …` / `路径不完整`，拓扑虚线段 2 条、标注 2 处 |

未自动验证：Windows UAC 与任务计划程序、真实内核与虚拟网卡生命周期、管理员权限下的 ACL、Linux musl 与 systemd 的更新交接、物理掉电。Linux 侧 `cargo test -p mesh-launcher -p mesh-install` 已在 WSL 单独跑过（43 + 37 全绿），另抓出并修掉两个只在 Unix 分支出现的 clippy lint。

## 有意不做的项

1. **跨进程启动器版本单调性**（F6 残项）：见上，代价与收益不成比例，已在契约写明语义。
2. **`credentialHeld` 协议字段**（F8 残项）：见上，现行空串语义已足够，等真有卡住的设备再加。
3. **F3 的极小残留窗口**：包已被接受（改名）之后控制面才撤回时，文件仍会作为「请安装」留在盘上，下一次启动会按 `apply_pending` 的「不比当前新就丢弃」处理。要彻底闭合需要启动器在 `apply_pending` 里引入撤回判据（跨 launcher/server），本轮不做。
4. **拓扑图分层仍按 `path` 段数**：不完整路径的目标落在「已知段数 + 1」层，比真实跳数少。用虚线段 + 「路径不完整」表达断点；按 `hops` 定层会让「以它为下一跳」的节点与父节点重叠，留给以后连同 DTO 一起改。
