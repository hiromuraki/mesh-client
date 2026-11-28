# 客户端修复回应

- 对应审查：[REVIEW.md](REVIEW.md)（提交 `3c47e13`，2026-11-28T14:20:46+08:00）
- 回应时间：2026-11-28T15:00:00+08:00
- 修复提交：见本轮 `git log -1`

## 逐项

### F1 身份切换后旧数据面继续运行（P1）——已修

`ensure_core` 现在按完整收敛规则判：期望规格为 `None` 时同样要求没有运行实例，先 `stop_core`；停不掉就按重拉档位排一次有界重试，错误写成完整一句（启动失败与停止失败各有措辞），`ensure_core_or_report` 只负责让它可见（`engine/src/run.rs` `ensure_core`、`ensure_core_or_report`）。

换绑把「停旧数据面」放在第一步（`engine/src/settings.rs` `bind`）：停不掉整个换绑失败、旧一代原样保留；随后删旧配置与凭据、清理旧一代发布授权、重置认证失败计数。身份被服务端判失效时同理（`engine/src/sync.rs`）：停不掉不再是 `let _ =` 静默，而是记日志并按档重试。

验证：`a_rebind_stops_the_old_core_before_the_identity_changes`、`a_rebind_that_cannot_stop_the_old_core_keeps_the_old_identity`、`a_core_that_will_not_stop_is_retried_until_it_does`、`a_removed_configuration_stops_the_running_core`（原「删配置不产生 core 命令」的断言方向相反，已改写）。

### F2 双槽确认可能复用旧 current（P2）——已修

守住「开始试运行前 `current` 不得等于本次候选」这条不变量：`install` 在清空候选槽之前显式判它，撞上就拒绝；`prepare` 修复写失败不再吞掉，这次流程失败、有效槽原样保留；两个槽都无效时删掉遗留的 `current`，否则首次安装写 `alice` 的那次确认会被旧值冒充。`wait_current` 的文档写明确认判据的前提就是这条不变量（`crates/launcher/src/install.rs`）。

验证：`a_stale_current_does_not_confirm_the_next_install`、`an_install_refuses_a_candidate_that_current_already_names`、`an_install_fails_when_bin_current_cannot_be_repaired`、`an_unbootable_install_drops_a_stale_current`。

### F3 候选启动与停止的结果未完整传播（P2）——已修

候选启动结果改成三态 `BootFailure`（`Create` / `Confirm` / `NotStopped`）：创建失败与「确认失败且已停止」按版本计次；`NotStopped` 保留进程事实，不计数、不回退、不宣告完成。Windows 只在 `Create`/`Confirm` 时把当前槽拉回来；Linux 手动安装确认后临时客户端停不掉改为明确失败，systemd 接管不会撞上仍持锁的进程（`crates/launcher/src/install.rs`、`supervisor.rs`）。

验证：`a_candidate_that_cannot_be_started_counts_a_failure_and_keeps_current`、`a_candidate_that_exits_without_confirming_is_counted_and_keeps_current`、`a_candidate_that_cannot_be_stopped_is_not_counted`（断言 `NotStopped` 与不计数）。

### F4 重试次数在下载前被清空、同一工件不刷新（P2）——已修

失败次数移到 `UpdateState::attempts`，属于整份 offer：`prepare` 起任务时不再清，`note_download_failure` 成为唯一失败落点（加一、退避、一行日志），只有手动重试与换 offer 归零。版本与摘要相同的完整下发刷新 `url`、`notes`、`auto`，不动已暂存件与在跑任务（`engine/src/update.rs`）。

验证：`download_failures_accumulate_to_the_limit`、`the_same_artifact_refreshes_its_address_without_downloading_again`、`a_manual_retry_clears_the_failure_count`。

### F5 交接失败后工件与授权丢失（P2）——已修

`Artifact` 增 `Committed{error}`，把「工件已就绪」与「安装事务已接收」分开：提交成功即 `Committed`，`handoff` 返回「已接收」才清空 `intent` 与 `artifact` 并退出；交接失败保留工件与原因、把 `auto_tried` 放回去，下一轮只重试交接、不重下。启动器替换任务在跑时延期客户端交接，两件事不同时抢安装根。撤回或换 offer 时删掉引擎自己放上去、尚未被接收的固定包，避免下一轮装上不再授权的版本（`engine/src/update.rs`）。

验证：`a_failed_handoff_keeps_the_package_and_the_authorisation`、`an_engine_waits_for_the_launcher_replacement_before_handing_off`。

### F6 归档与已安装目录的资源预算（P2）——已修

所有进入安装的包走同一道字节闸（200 MiB），`--file` 在求摘要之前就拒绝；读清单与解包共用一个 120 秒截止时刻；解压器加显式窗口预算（`UNPACK_WINDOW_LOG = 26`，64 MiB），与解压总量 256 MiB 分开；日常选槽读已安装清单也受 64 KiB 上限（`crates/launcher/src/install.rs`、`crates/install/src/manifest.rs` 的 `MANIFEST_BYTES`）。

验证：`a_package_over_the_byte_budget_is_refused_before_its_digest`、`the_decoder_refuses_a_frame_window_over_its_budget`、`read_refuses_a_manifest_over_the_byte_budget`、`read_accepts_a_manifest_at_the_byte_budget`。

### F7 `--boot-check` 参数未限定（P2）——已限定，入口保留

过渡期不删入口（仍有设备在扁平布局，见 `PENDING.md`），但参数只接受旧启动器生成的小写十六进制标识（1–8 位），字符集与长度不合即拒绝；标记路径在启动时一次算好，断言父目录就是安装根（`crates/client/src/cli.rs`、`main.rs`）。

验证：`boot_check_accepts_only_the_old_launcher_marker_shape`、`a_boot_marker_token_cannot_leave_the_install_root`。

### F8 正常无变化心跳进入配置缺失路径（P3）——已修

`apply_remote` 只由响应确实带配置正文触发；204、只带发布信息或只带凭据的心跳不记「配置变化但未带正文」，也不重复收敛。`on_beat` 末尾的 `ensure_core_or_report` 是唯一收敛入口（`engine/src/run.rs`、`sync.rs`）。

验证：`a_heartbeat_without_a_body_leaves_the_config_alone`、`a_heartbeat_with_a_body_applies_it_once`。

### 跨端：nonce 饱和改成 503

客户端只有 401 计入认证失败；5xx 走控制面退避，`auth_fails` 与本地身份都不动（`engine/src/run.rs` 的 `beat`、`sync.rs` 的边界注释）。验证：`a_service_unavailable_does_not_clear_the_identity`（连续 5 次 503：`device_id` 不变、`auth_fails = 0`、仍有下一次心跳期限，随后 204 恢复）。

## 已知安全事项

C1、C2、C3、C4、C6 本轮未动，与审查结论一致：它们不在本轮范围内，优先级见 `PENDING.md` §3（C2 最优先）。F7 的入口在迁移完成后按 `PENDING.md` §1 删除。

## 验证

| 命令 | 结果 |
| --- | --- |
| `cargo test --workspace --no-fail-fast --offline`（Windows） | 全绿；engine 108、launcher 56、install 44、client 21 + 2 |
| `cargo test --workspace --no-fail-fast --offline`（WSL，`x86_64-unknown-linux-gnu`） | 全绿（`cfg(unix)` 的 supervisor 与手动安装路径参与编译与运行） |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy -p mesh-launcher -p mesh-install -p mesh-client --all-targets` | 无告警 |

## 未自动验证与剩余风险

1. 「停止未确认」无法在测试里确定性制造（Windows 强杀、Linux SIGTERM 后 kill 都会成功）：只覆盖到决策层——`NotStopped` 不计数、调用方不回退；真实杀不掉的进程路径靠代码审查。
2. Windows 的 UAC 路径（`ShellExecuteEx runas`）未在真机验证；新增的 `Create` 失败用例覆盖的是非 UAC 的创建失败。
3. 交接失败后每轮心跳都会重试并各记一行日志：启动器长期缺失时会一直刷，属可见性代价。

## 新发现（留给下一轮评审）

1. 空配置下发的形状：服务端在配置变化时回 `configB64`，但空配置的 `configB64` 被 `omitempty` 掉，客户端按「只认正文」的规则会忽略它——「把设备配置清空」这类下发因此到不了设备。需要契约给出空配置的显式形状。
2. `commit_package` 用改名覆盖固定路径：盘上若已有未被消费的 `update.tar.zst`，Windows 上改名失败 → 记失败 → 重下 → 再失败，5 次后放弃。修法要先定契约（引擎能否清掉固定名上的包）。
