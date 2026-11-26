# mesh-client 回应（da0ba28，评审 2026-11-27T05-45-47+0800_da0ba2）

- **对应评审**：[`code-reviews/2026-11-27T05-45-47+0800_da0ba2/REVIEW.md`](REVIEW.md)（基线 `da0ba28`）
- **回应时间**：2026-11-27 06:25 +08:00
- **结论**：F1–F10 全部修，待确认 7 条里 2 条改了代码、1 条改了文档，其余 4 条保留并说明理由。

## 逐条回应

### [P2][F1] STUN 刷新线程每次重连新起一个且永不退出 —— 已修

改 [crates/engine/src/run.rs](../../crates/engine/src/run.rs)：`stun_stop` 与 `hb_stop` 同构，`start_stun_loop` 先停旧的，`stop_stun_loop` 在连接循环退出与重连两处置位；线程里的 `sleep` 换成 `recv_timeout(STUN_REFRESH_INTERVAL)`，退出与重连都不必等满一个刷新周期。

没有改成「引擎 `next_deadline()` 上的到期项」：`show_node_info` 最长阻塞 3 秒，放到引擎线程会让面板轮询卡住；心跳线程当初也是同一理由独立出去的，两个线程保持同一种形态，比多引入一种调度更简单。

验证：`probe-stun-thread-leak.ps1` 复跑，20 次 `POST /api/reconnect` 后 `threads_before=20 threads_after=20`（修前 20 → 40）。

### [P2][F2] core 就绪探测期间不接受 Stop —— 已修

改 [crates/core/src/thread.rs](../../crates/core/src/thread.rs)、[crates/core/src/probe.rs](../../crates/core/src/probe.rs)：探测从「内联阻塞一整段」改成命令循环里的状态机——`Pending { running, rpc, expect, deadline, next_probe, last }`，循环每 `TICK`（100ms）推进一次 `poll_ready`，到点才查一次；`probe::wait_ready` 随之拆成单次 `probe::attempt`（`Ready` / `Exited` / `NotYet`）。Stop 因此在 ≤100ms + 一次查询（`probe_timeout`，3 秒）内被响应，不再等满 25 秒。

验证：新增 `stop_is_answered_while_the_ready_probe_is_still_running`（[crates/core/tests/managed_core.rs](../../crates/core/tests/managed_core.rs)，`no-portal` + 25 秒时限，Stop 必须在 5 秒内回复）；`probe-shutdown-during-ready-probe.ps1` 复跑：`/api/quit` 94ms 回复，进程 **2.7 秒**后退出（修前 15.7 秒），`core_alive_after_client_exit=False`。

### [P2][F3] 启动器 `install::run` 不先 `recover` —— 已修

改 [crates/launcher/src/install.rs](../../crates/launcher/src/install.rs)：`run` 第一件事 `recover(root)?`，与启动路径、Linux 监督器一致；`install_from_file` 遇到仍然存在的 `bin.old` 直接报错，不再 `remove_dir_all` 掉唯一的回滚材料。

验证：新增 `a_manual_install_rolls_a_stale_bin_old_back_first`；`probe-launcher-destructive-paths.ps1` B 段复跑：日志先「上次安装没有走完，回滚到 bin.old」，装坏包失败后 `good_marker_survives=True`、`broken_marker_back_in_bin=False`、`bin/version.json` 停在好的那一版。

### [P2][F4] `mesh --file` 安装失败会删掉用户给的包 —— 已修

改 [crates/launcher/src/install.rs](../../crates/launcher/src/install.rs)：只有包路径等于 `<root>/update.tar.zst`（客户端下载后交接的那份）才删；`--file`/`--url` 给的源文件一律不动，成功与失败的处置因此一致。

验证：新增 `a_failed_manual_install_keeps_the_package_the_user_gave`；`probe-launcher-destructive-paths.ps1` A 段复跑：摘要不符时 `source_package_still_exists=True`。

### [P2][F5] Windows 启动标记在托盘初始化之前写出 —— 已修

改 [crates/client/src/main.rs](../../crates/client/src/main.rs)、[tray.rs](../../crates/client/src/tray.rs)、[headless.rs](../../crates/client/src/headless.rs)：标记改成回调 `boot_ready(root, token)`（只生效一次），由两种形态各自在「自己真的立起来」之后调用——托盘在窗口、监听线程、图标都就位之后；headless 在过了 `unconfigured` 那一关之后。

验证：只有代码与编译（CI 的 Windows job 现在会编这部分）。真机触发条件（`%TEMP%` 不可写、图标被拦）本机造不出来，仍然只能靠代码阅读确认。

### [P3][F6] CI 从不编译启动器的 Windows 代码 —— 已修

改 [.github/workflows/build.yml](../../.github/workflows/build.yml)：Windows job 的构建改成 `cargo build --release --workspace`（启动器的 `runas`/安装/回滚路径因此进入编译），并增加 `cargo test --all`，让自启 XML、Job 对象、进程树这些只在 Windows 上跑得动的用例真的跑一遍。

### [P3][F7] `publish` 每条消息读两次配置、算两次摘要 —— 已修

改 [crates/engine/src/views.rs](../../crates/engine/src/views.rs)：`publish` 算一次 `hash`，传给 `snapshot_with_hash` 与 `BeatInput`；`snapshot()` 保留为「自己算」的入口，给面板与单测用。

### [P3][F8] `core::stop` 失败时把 `Child` 与输出泵一起丢掉 —— 已修

改 [crates/core/src/thread.rs](../../crates/core/src/thread.rs)：`stop` 拆成 `stop`（挑出要停的那一代）与 `stop_process`；真正干活的 `terminate_and_reap` 停不掉时把 `Running` 原样交还，`stop_process` 把它放回 `running` 再返回错误。`start` 的守卫同时看 `running` 与 `pending`，因此停不掉时下一次 `Start` 只会报启动失败，不会再起第二个 core。

### [P3][F9] etrpc 分片重组没有总字节上限 —— 已修

改 [crates/etrpc/src/client.rs](../../crates/etrpc/src/client.rs)：新增 `MAX_RESPONSE_BYTES = 8MiB`，逐片累计，超了报 `Frame` 错误。8MiB 相对最大实测应答（`DumpRoute` 160KB）留了两个数量级余量。

### [P3][F10] `Log::tail` 是只为单测存在的生产 API —— 已修

删掉 [crates/engine/src/log.rs](../../crates/engine/src/log.rs) 的 `pub fn tail`，三处单测改用同文件的 `logs(path, n)`。

## 待确认项

| 项 | 结论 | 说明 |
| --- | --- | --- |
| 1 派生子进程继承管道时泵可能不 EOF | 保留 | 泵的 join 确实没有超时。加超时要么在超时后强杀 core、要么丢弃泵句柄，两者都比现在的「等真机 core 的进程树」更容易引入新问题；真跑起来的 easytier-core 不派生长活子进程（托管测试里 `stopping_takes_the_whole_process_group_with_it` 也覆盖了进程组）。等有真机反例再改 |
| 2 重新注册后旧 `credential.json` 不清理 | 已修 | [crates/engine/src/settings.rs](../../crates/engine/src/settings.rs)：注册返回的 `deviceId` 与本地不同（换绑了另一台设备）时 `remove_credential()`；同一个设备重复注册不受影响 |
| 3 `read_manifest` 只认裸 `version.json` | 无需改 | `build/release.ps1` 用 `UstarTarEntry` 逐条写 `dist/bin` 的顶层文件，条目名就是 `version.json`（无 `./` 前缀）；线上更新一直装得上。真正的约束是「改用别的打包器时条目名要对」，已在契约与脚本注释里体现 |
| 4 启动器替换的时序 | 保留 | 重命名被占只记一行日志、等下一个版本再试，是可接受的降级；窗口大小需要真机测量，本轮没有条件 |
| 5 `--wait-for` 用 `/proc/<pid>` 判存活 | 保留 | 只影响手动 `--file` 安装路径，注释已写明 pid 复用的可能 |
| 6 `instance.lock` 的位置与文档措辞 | 已修（改文档） | `docs/contract/client.md` 改成「位于安装根目录」，并写明为什么不放 `bin/`（每次更新都会被换掉） |
| 7 RUST_LOG 的 target 是否对 mini 也成立 | 保留 | 只影响日志级别，不影响行为；现场排查时可以用 `RUST_LOG` 覆盖 |

## 构建与测试

环境：Windows + pwsh 7，`cargo 1.100.0 / rustc 1.100.0`（`rust-toolchain.toml` 钉住）。

| 命令 | 结果 |
| --- | --- |
| `cargo build --workspace` | exit 0 |
| `cargo test --all` | 通过：**209** 个测试（评审时 206，本轮 +3：core 6→7、launcher 8→10），0 失败 |
| `cargo clippy --all-targets -- -D warnings` | exit 0 |
| `cargo fmt --all -- --check` | exit 0 |

### 评审探针复跑

| 探针 | 修前 | 现在 |
| --- | --- | --- |
| `probe-stun-thread-leak.ps1` | 20 次重连 → 线程 20 → 40 | 20 → 20 |
| `probe-shutdown-during-ready-probe.ps1` | `/api/quit` 731ms，进程 15.7s 后退出 | 94ms，进程 2.7s 后退出，core 已随进程结束 |
| `probe-launcher-destructive-paths.ps1` | A：用户包被删；B：好副本被删、坏 bin 复辟 | A：`source_package_still_exists=True`；B：先回滚、好副本留住 |

## 未验证

- F5 的真机触发条件（`%TEMP%` 不可写、托盘图标被安全软件拦下、`RegisterClassExW` 失败）本机造不出来，只有代码与编译层面的确认。
- F6 的 CI 改动要在下一次 push 后才会真的跑（本轮没有触发 workflow）。
- Linux（musl）下的 `assign_to_job` 空实现与进程组行为沿用上一轮说明，本机没有 musl 目标。
