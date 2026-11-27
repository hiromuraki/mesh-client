# 客户端状态模型

本文只写客户端的**状态空间**：有哪些状态、谁能进入、允许的迁移、跨状态必须成立的不变量。线格式与磁盘文件形状不在这里重复，见 [契约/updates.md](../../docs/contract/updates.md)（安装布局、回滚、槽位）与 [契约/client.md](../../docs/contract/client.md)（本地状态、启动、凭据上报、401）。

本文覆盖六个模型：安装事务、凭据身份、启动器监督与启动确认、连接失败与 401、引擎的期限模型与状态推导、更新（offer / 下载任务 / 工件 / 意图）与启动器替换。不覆盖：线格式与磁盘文件字段（见上述契约），以及面板与托盘的呈现细节。流程图与时序见 [flows.md](flows.md)。

## 1. 安装事务

一次安装是一个事务，事务状态记在 `<安装根>/install.state`，取值是 `fresh`、`replaced`、`installed`、`restoring`、`committed`；**没有这个文件就等于没有在途的安装**。这个文件只由启动器读写，客户端与它的交接走 `update.tar.zst` 与 `.update-failures.json`。

```mermaid
stateDiagram-v2
    state "无 install.state" as Idle
    [*] --> Idle
    Idle --> Fresh: 开始安装，安装前没有 bin/
    Idle --> Replaced: 开始安装，bin/ 已改名到 bin.old/
    Fresh --> Installed: 解包与清单校验通过
    Replaced --> Installed: 解包与清单校验通过
    Fresh --> Idle: 失败，删掉本次的 bin/ 并清状态
    Replaced --> Restoring: 回滚开始，先落盘 restoring
    Installed --> Restoring: 启动确认失败或超时，有备份
    Installed --> Idle: 启动确认失败，首次安装，删 bin/
    Restoring --> Idle: 目录换回后清状态
    Installed --> Committed: 启动确认成功
    Committed --> Idle: 清 bin.old/、bin.stale/ 与待装包后清状态
```

| 状态 | 谁写它 | `bin/` 是什么 | 恢复到这一状态时做什么 |
| --- | --- | --- | --- |
| 无该文件 | `commit`、回滚收尾 | 已确认能启动的那一份 | 清掉 `bin.old/`、`bin.stale/` 残留，不碰 `bin/` 与待装包 |
| `fresh` | 安装开始（安装前无 `bin/`） | 本次解出来的，可能只解了一半 | 删掉 `bin/`，回到「没装过」 |
| `replaced` | 安装开始（有 `bin/`，已改名到 `bin.old/`） | 本次解出来的，可能只解了一半 | 删掉 `bin/`，把 `bin.old/` 改回来 |
| `installed` | 解包与校验通过 | 完整的新版本，未确认启动 | 保留，由这一轮启动去确认；确认失败再退 |
| `restoring` | 换回旧版本之前 | 正在被换出 | 接着换回，每一步可重复 |
| `committed` | 启动确认成功 | 已确认启动的新版本 | 不回滚，只清残留与状态文件 |

`bin.old/` 是上一个版本，只在一个安装窗口里存在：装之前把 `bin/` 改名到这里，确认成功就删掉，失败就改名回 `bin/`。`bin.stale/` 是回滚时被丢下的**新**版本：换回旧版本要先把新版本挪开，「改名」与「删除」不是一次操作，中间断掉时旧版本必须还在。两个目录都不承担判据——判据只有 `install.state`。

两把锁回答两个问题，互不替代：

| 锁文件 | 回答的问题 | 谁持有 |
| --- | --- | --- |
| `install.lock` | 谁在改安装根（`install.state`、`bin/`、`bin.old/`、`bin.stale/`、待装包） | 启动器覆盖整个事务；客户端只在提交更新包、替换启动器的那一段 |
| `instance.lock` | 这份 `config/` 是不是正被一个客户端使用 | 客户端进程从启动到退出 |

两者都是操作系统的文件锁，进程退出（含被杀）即释放，不留下需要人工删除的文件。冲突不靠 `install.lock` 预防：客户端在跑时手工安装由 `client_running` 判据拒绝。

不变量：

1. 判据唯一：`install.state` 决定「这次安装走到哪一步」，`bin.old/` 是否存在只说明有备份。
2. 先落盘再动手：`fresh`/`replaced` 写在改动 `bin/` 之前，`restoring` 写在换回目录之前，`committed` 写在清理之前。
3. 旧版本在任何一刻都不被删除：回滚顺序是 `bin/` → `bin.stale/`、`bin.old/` → `bin/`、最后删 `bin.stale/`。
4. 认不出的状态值按错误停机，不猜：猜一个状态只会拿错的前提去删文件。
5. 安装包只由安装事务删除：它是「请安装」的信号，普通启动确认只清 `bin.old/` 与 `bin.stale/`。
6. 每次安装先 `abort` 上一份没确认启动的事务：唯一的回滚材料不能被一次新安装覆盖。
7. 提交之后不回滚：清理失败只影响残留，`bin/` 的含义不再改变。

依据：`client/crates/launcher/src/install.rs` 的 `Tx`、`Owner::recover`、`Owner::abort`、`Owner::commit`、`Owner::undo`、`Owner::install_signed_by`、`client_running`；`client/crates/install/src/layout.rs` 的 `INSTALL_STATE`、`BIN_OLD`、`BIN_STALE`、`UPDATE_FILE`、`LOCK`、`INSTALL_LOCK`；`client/crates/install/src/lock.rs` 的 `Lock::path`、`Lock::owner_path`。

## 2. 凭据身份四态

「磁盘上有私钥」与「core 已经加载并确认它」是两件事，分成两个状态表达。控制面把上报的 id 当作「设备正在用这张」的证据，只有 core 确认加载过的那张才能上报。

```mermaid
stateDiagram-v2
    [*] --> Formal: 配置里有 network_secret
    [*] --> Missing: 磁盘上没有凭据
    Formal --> Missing: 配置不再带 network_secret，且磁盘无凭据
    Formal --> Held: 配置不再带 network_secret，磁盘有凭据
    Missing --> Held: 心跳带回凭据，落盘 credential.json
    Held --> Loaded: core 以这张凭据就绪，上报进程号
    Loaded --> Held: core 退出或换进程，磁盘上那张还在
    Held --> Missing: 凭据文件被删
    Loaded --> Missing: 401 revoked，清身份并删凭据
```

| 状态 | 判定来源 | 心跳上报的 `credentialId` | 进入条件 |
| --- | --- | --- | --- |
| `Formal` | 配置里有非空 `network_secret` | 空串 | 配置判出正式节点，优先于磁盘上是否存着凭据 |
| `Missing` | 磁盘上没有可读的凭据 | `none` | 从未签发、文件被删、身份被清 |
| `Held(id)` | 磁盘上有凭据，但 core 未确认加载 | 空串 | 凭据刚落盘；core 未起、起不来、已退出或正在重启 |
| `Loaded(id)` | core 的期望规格里带着这张凭据，且进程已就绪 | 该 id | core 报告 `Started`（进程号非零） |

落盘到上报之间必须隔一次 core 重启：core 不支持热更新身份，换凭据只能换进程。提前上报刚写盘的那张，会让控制面撤销 core 仍在使用的旧凭据。空串对控制面是「既不确认也不签发」，恰好覆盖「正式节点」与「拿到了但还没加载」两种情形。

不变量：

1. 上报值只有三种形状：空串、`none`、磁盘上真实存在且 core 已加载的 id。
2. `Loaded(id)` 的 id 必然等于磁盘上那张凭据的 id：启动时按 id 比对，不一致按「没有凭据」处理，不带着别的凭据启动。
3. `Held` 永不上报 id，`Formal` 永不上报 `none`：前者会提前撤销在用的旧凭据，后者会让控制面每轮白签一张。
4. 正式节点即使磁盘上留着凭据也不改写认证方式，凭据对它不生效。
5. 凭据状态不缓存，每次上报现取磁盘事实与 core 事实。

依据：`client/crates/engine/src/sync.rs` 的 `CredentialState`、`CredentialState::reported_id`、`Engine::credential_state`、`Engine::save_credential`；`client/crates/engine/src/run.rs` 的 `AuthMode`、`CoreRun`、`Engine::on_core_event`、`Engine::start_core`、`NO_CREDENTIAL_ID`。

## 3. 启动器监督与启动确认

启动器只做一件事：把 `bin/` 里这一份拉到确定的结局，即验证、拉起、等确认、提交或回滚。四个入口（安装之后、普通启动、确认被打断后的下一次启动、Linux 常驻监督）走同一条 `boot_and_settle`，两个平台只差进程怎么托管。

```mermaid
flowchart TD
  A["入口：普通启动 / --file / --url / 常驻监督"] --> B["own：取 install.lock，拿不到即失败"]
  B --> C["clean_stale_markers + recover：按 install.state 退回没走完的安装"]
  C --> D{"有待装的包？"}
  D -- 是 --> E["--file / --url 由参数给，常驻监督看 update.tar.zst；不比当前版本新就丢弃"]
  D -- 否 --> F["verify：验 bin/version.json 签名，再逐文件比对摘要"]
  E --> F
  F -- 不通过 --> G["记一次失败，abort"]
  F -- 通过 --> H["拉起 bin/mesh-client，带 --root 与 --boot-check 标记"]
  H -- 创建失败 --> G
  H --> I["等 .boot-ok-标记，上限 60s"]
  I -- 看到标记 --> J["清掉该版本的失败计数，commit"]
  I -- 进程提前退出或超时 --> K["先停掉新版并确认它退出"]
  K -- 停不下来 --> L["不回滚，保留 installed，下一轮接着确认"]
  K -- 已退出 --> G
  G --> M["有在途安装则回滚，普通启动不动文件"]
```

| 结局 | 状态文件 | `bin/` 的去向 |
| --- | --- | --- |
| 校验不通过 | 记一次失败后 `abort` | 有在途安装才回滚；普通启动保持原样 |
| 进程创建失败 | 同上 | 同上 |
| 确认超时或提前退出 | 停掉新版、记一次失败、`abort` | 回到安装前的那一套 |
| 停不掉新版 | 保留 `installed` | 不改目录，下一次启动接着确认 |
| 确认成功 | 清失败计数、`commit` | 新版本定稿，清理备份与待装包 |

启动确认标记是 `<安装根>/.boot-ok-<token>`，token 由启动器每次生成；客户端在引擎、面板（桌面形态还含托盘）就位时写一次，不等网络。旧标记在每次事务入口统一清掉，上一次启动的标记不会被这一次误认。

| 项 | 常量 | 值 |
| --- | --- | --- |
| 等旧客户端退出 | `WAIT_CLIENT` | 60s |
| 等启动确认 | `WAIT_BOOT` | 60s |
| 停客户端后确认退出的等待 | `STOP_TIMEOUT` | 10s（Linux 先 SIGTERM，不退再强杀，各自等一次） |
| 包字节上限（`--url` 下载） | `PACKAGE_BYTES` | 200 MiB |
| 下载总时限 | `DOWNLOAD_DEADLINE` | 300s |
| 包内条目数 | `Budget::package().entries` | 1024 |
| 包根清单字节 | `Budget::package().manifest` | 64 KiB |
| 解包后总字节 | `Budget::package().unpacked` | 256 MiB |
| 读清单与解包时限 | `Budget::package().deadline` | 120s |
| 同一版本连续失败上限 | `LIMIT` | 2 次 |
| 失败记录保留版本数 | `KEEP_VERSIONS` | 5 个 |

不变量：

1. 安装根只有一个写者：拿不到 `install.lock` 就一行都不写，第二个启动器当场失败而不是并行改目录。
2. 确认只认本次的标记：token 每次不同，旧的标记不会被认领。
3. 确认成功之后 `bin/` 不再回滚；确认失败必须先把新版停掉并确认它退出，才允许改目录。
4. 普通启动失败不删任何文件；只有本次安装还没确认启动时才有东西可退。
5. `recover` 在拉起任何进程之前执行，每一轮都做（常驻启动器把它放在循环内）。
6. 等待客户端的那段时间不持安装根写权：客户端提交更新包要拿同一把锁。
7. 待装包只有消耗它的安装事务能删：删早一次，Linux 上的自动更新就永远装不上。

依据：`client/crates/launcher/src/install.rs` 的 `Owner::boot_and_settle`、`Owner::recover`、`Owner::abort`、`Owner::commit`、`wait_boot`、`clean_stale_markers`、`Budget`、`boot_or_restore`、`launch_installed`、`client_running`；`client/crates/launcher/src/launch.rs` 的 `Client::stop`、`STOP_TIMEOUT`；`client/crates/launcher/src/supervisor.rs` 的 `run`、`apply_pending`；`client/crates/install/src/failures.rs` 的 `Failures`、`LIMIT`、`KEEP_VERSIONS`；`client/crates/client/src/boot_check.rs` 的 `write_marker`；`client/crates/client/src/main.rs` 的 `boot_ready`。

## 4. 连接失败与 401

心跳的结果只有三类：成功、非 401 失败、401。每一类只改两件事——失败计数与下一次期限；身份是否保留由 401 的分支单独决定。控制面不可达只降级，不牵连数据面；core 不在跑也不中止心跳。

| 结果 | 失败计数 | 下一次期限 | 身份 |
| --- | --- | --- | --- |
| 成功 | 失败计数与 401 计数清零，记下最近心跳时刻 | 30s 后例行 | 保留 |
| 非 401 失败（网络、超时、5xx、取样失败） | 加一 | 连接退避 | 保留 |
| 401 `revoked` | 加一 | 不再排期 | 清掉 `device_id` 与本地凭据、停 core、回设置页 |
| 401 其它，时钟偏差超过 300s | 加一 | 连接退避 | 保留，只降级 |
| 401 其它，连续不足 3 次 | 加一，401 计数加一 | 连接退避 | 保留 |
| 401 其它，连续达到 3 次 | 同上 | 不再排期 | 清 |

```mermaid
flowchart TD
  A["心跳返回 401"] --> B{"code 是 revoked？"}
  B -- 是 --> C["清 device_id 与本地凭据、停 core、回设置页"]
  B -- 否 --> D{"响应带可用的 Date 头？"}
  D -- 是 --> E{"本机与服务端偏差超过 300s？"}
  E -- 是 --> F["只降级并提示校准时间，不清身份"]
  E -- 否 --> G["连续 401 计数加一"]
  D -- 否 --> G
  G --> H{"计数达到 3 次？"}
  H -- 是 --> C
  H -- 否 --> I["保留身份，按连接退避重试"]
```

| 项 | 常量 | 值 |
| --- | --- | --- |
| 单次心跳超时 | `HEARTBEAT_TIMEOUT` | 20s |
| 时钟偏差窗 | `CLOCK_SKEW_LIMIT_SECONDS` | 300s |
| 连续 401 上限 | `AUTH_FAIL_LIMIT` | 3 次 |

不变量：

1. 401 不区分原因：服务端只在删身份时回 `revoked`，其余一律 `unauthorized`，客户端因此先看时间偏差、再看连续次数。
2. 时钟偏差从不清身份：一次对时错误不该换来一次换机重注册。
3. 连续 401 计数只在成功心跳时清零；`revoked` 直接清身份，不等次数。
4. 清身份之后没有下一次期限（`unbind` 把 `due` 置空），界面回到 `unconfigured`。
5. core 未就绪不影响心跳继续：心跳通了而 core 没跑，按当前规格补启动。

依据：`client/crates/engine/src/run.rs` 的 `Engine::on_heartbeat`、`note_control_failure`、`Engine::unbind`；`client/crates/engine/src/sync.rs` 的 `Engine::handle_auth_rejected`、`Engine::reset_auth_fails`、`CLOCK_SKEW_LIMIT_SECONDS`、`AUTH_FAIL_LIMIT`。

## 5. 期限模型与状态推导

引擎只有一个线程读写状态，一条循环等两件到期的事：下一次联系控制面（`Control.due`）与下一次重拉 core（`Revive.due`）。两者独立推进，每次唤醒取最早的一个；没有期限时阻塞等命令。

```mermaid
flowchart TD
  A["pump：取 min(control.due, revive.due)"] --> B{"有期限？"}
  B -- 没有 --> C["阻塞等命令"]
  C --> E
  B -- 有 --> D{"到点前收到命令？"}
  D -- 是 --> E["handle 命令，publish，重算期限"]
  E --> A
  D -- 否 --> F["on_deadline：revive.due 到点则 revive_core，control.due 到点则 beat"]
  F --> G["publish"]
  G --> A
```

| 期限 | 字段 | 何时设置 | 值 |
| --- | --- | --- | --- |
| 下一次心跳 | `Control.due` | 心跳成功 | `HEARTBEAT_INTERVAL`，30s |
| | | 非 401 失败、401 未清身份 | `connect_backoff`：10s 起每次 ×1.5，`CONNECT_BACKOFF_MAX` 60s 封顶 |
| | | 本地身份读失败 | `LOCAL_STATE_RETRY`，5s |
| | | 存下身份、手动重连 | 立即 |
| | | 请求在途 | `None`：不排下一次 |
| | | 没有身份（`unbind`） | `None` |
| 下一次重拉 core | `Revive.due` | core 启动失败 | `revive_backoff`：1s 起每次 ×2，`REVIVE_BACKOFF_MAX` 30s 封顶 |
| | | core 自行退出 | 同上；上一代活够 `CORE_MIN_LIFETIME`（5s）才回到第一档 |
| | | core 报告就绪 | 清空 |

代次把「结果属于哪一次」钉死，三种代次都只增不减：

- 心跳代次 `Control.gen`：每发一次加一，`on_heartbeat` 只接受当前代次；重连与换身份也加一，让在途结果作废。
- core 代次 `CoreRun.gen`：`start_core` 每次分配一个新值（`next_gen`），core 事件与 STUN 结果按它匹配，迟到事件丢弃。
- 任务代次 `DownloadTask.gen`：`next_task` 只增不减，换 offer 不重置；下载与启动器替换的完成消息按「任务相等」与「当前 offer 版本与摘要相等」两条判据决定还算不算数。

`Status` 没有写入点：`Engine::status` 从 `fault`、是否有身份、最近心跳结果、core 是否就绪一次算出。

```mermaid
stateDiagram-v2
    [*] --> Unconfigured
    Unconfigured --> Connecting: 存下身份，期限立即到期
    Connecting --> Online: 心跳成功且 core 就绪
    Connecting --> Degraded: 心跳失败，或 core 在首轮心跳前退出（排了重拉）
    Connecting --> Error: core 启动失败等确定失败
    Online --> Degraded: 心跳失败，或 core 退出
    Degraded --> Online: 心跳成功、core 就绪，失败原因不再成立
    Online --> Error: 本地状态、配置或 core 启动确定失败
    Degraded --> Error: 同上
    Error --> Online: 两个平面都恢复
    Online --> Unconfigured: 401 revoked，清身份
    Degraded --> Unconfigured: 同上
```

| 顺序 | 判据 | 结果 |
| --- | --- | --- |
| 1 | `fault` 非空：本地状态读不出、配置写不进、core 起不来 | `error` |
| 2 | 没有客户端配置：未注册，或身份已被吊销 | `unconfigured` |
| 3 | 没有心跳结果、失败计数为 0、没有排定的重拉 | `connecting` |
| 4 | 失败计数为 0、有过心跳结果、core 已就绪 | `online` |
| 5 | 其余：两个平面有一个没就绪 | `degraded` |

不变量：

1. 期限只在引擎线程写；心跳、下载、面板、STUN 线程只投递结果。
2. 两边期限独立：控制面不可达只让心跳退避，不会取消 core 的重拉期限。
3. 请求在途时 `Control.due` 为空，因此同一时刻最多一条心跳。
4. 到点才推进：`on_deadline` 对两边都再查一次 `due` 是否已过。
5. 旧代次的结果一律丢弃，且不许落盘配置或凭据、不许按旧连接的 401 清身份。
6. 没有身份时控制面没有期限；若也没有排定的重拉，循环退化为纯等命令。
7. `Status` 是推导值，任何处理器都不写它；`settle` 只清掉失败原因，状态随下一次推导改变。

依据：`client/crates/engine/src/run.rs` 的 `Control`、`CoreRun`、`Revive`、`CoreSpec`、`AuthMode`、`Engine::pump`、`Engine::next_deadline`、`Engine::on_deadline`、`Engine::beat`、`Engine::on_heartbeat`、`Engine::on_core_event`、`Engine::schedule_revive`、`Engine::next_revive_attempt`、`Engine::revive_core`、`Engine::rebind`、`Engine::unbind`、`Engine::status`、`Engine::settle`、`connect_backoff`、`revive_backoff`、`HEARTBEAT_INTERVAL`、`HEARTBEAT_TIMEOUT`、`CONNECT_BACKOFF_MAX`、`REVIVE_BACKOFF_MAX`、`CORE_MIN_LIFETIME`、`STOP_REPLY_TIMEOUT`、`LOCAL_STATE_RETRY`、`STUN_REFRESH_INTERVAL`。

## 6. 更新模型

自更新把四个事实分开表达，每个只有一个落点；界面上的 `Phase`（`Idle` / `Downloading` / `Ready` / `Failed`）是它们的投影，不是另一份状态。服务端每轮心跳都带完整的一份 `appUpdate`；「是不是同一份」由客户端按版本与摘要判断，与响应形状、下载进度都无关。线格式见 [契约/updates.md](../../docs/contract/updates.md)，交接约定见 [契约/client.md](../../docs/contract/client.md)「客户端如何装」。

| 轴 | 类型 | 取值 | 谁改它 |
| --- | --- | --- | --- |
| offer | `Option<Offer>` | 没有；有（`version`、`sha256`、`auto`、`auto_tried`） | 心跳的 `Engine::offer_update`、`start_offer`、`withdraw` |
| task | `Option<DownloadTask>` | 没有；在跑（`gen`、`version`、`sha256`） | `prepare` 起任务，`on_download` 落任务 |
| artifact | `Artifact` | `None`；`Staged(.part 路径)`；`Failed{error, attempts, retry_at}` | `on_download`、`try_apply`、`prepare` |
| intent | `Option<Intent>` | 没有；`Manual`；`Auto` | 用户点安装、自动策略；offer 变化或交接完成时清空 |

```mermaid
stateDiagram-v2
    state "无 offer" as NoOffer
    state "有 offer，无工件" as Offered
    state "下载中（task 非空）" as Downloading
    state "已暂存（artifact = Staged）" as Staged
    state "准备失败（artifact = Failed）" as Failed
    state "已交接（改名成 update.tar.zst）" as Handed
    [*] --> NoOffer
    NoOffer --> Offered: 心跳带来完整 appUpdate，版本与签名都过
    Offered --> Downloading: prepare 起一个任务（Windows 预取，或已有授权）
    Downloading --> Staged: 下载完成，摘要与签名都过
    Downloading --> Failed: 下载或校验失败
    Failed --> Downloading: 退避到点自动重试；用户点安装则立即重下
    Staged --> Handed: 有意图且拿到安装根写权
    Staged --> Staged: 拿不到写权，延期（不重下）
    Staged --> Offered: 换 offer，旧暂存件删除
    Staged --> NoOffer: 撤回，旧暂存件删除
    Downloading --> Offered: 换 offer，在跑任务作废
    Downloading --> NoOffer: 撤回，在跑任务作废
    Handed --> [*]: 进程退出，安装交给启动器
```

| 当前 | 事件 | 迁移 | 落地动作 |
| --- | --- | --- | --- |
| 无 offer | 心跳带完整 `appUpdate`，版本与签名都过 | → 有 offer，`intent` 与 `artifact` 清空 | 删上一份暂存件（内容不再被授权）；Windows 立即预取，Linux 等授权 |
| 有 offer | 心跳带来同一份（版本与摘要都相同） | 不变 | 只更新 `auto`，不重下、不动进度 |
| 有 offer | 心跳撤回（响应里没有 `appUpdate`） | → 无 offer | 删暂存件、清意图；在跑任务留着，回来时自弃 |
| 有 offer | 再次完整下发（换版本或同版本重发） | 旧 offer → 新 offer | 删旧暂存件、清意图；在跑任务留着 |
| 有 offer、无工件 | 策略允许且这一份没试过 | 记 `intent = Auto` | 无（自动意图每份只记一次） |
| 有 offer、无工件 | 没有意图，且平台是 Windows | 起任务 | 预取到 `.part`：只暂存，不算授权 |
| 有 offer、有意图 | 需要字节 | 起任务 | 写 `<root>/update.tar.zst.part`，同一时刻最多一个任务 |
| 下载中 | 任务完成、仍是当前 offer | → `Staged` | 只暂存，不提交 |
| 下载中 | 完成时 offer 已换或已撤回 | → 新 offer，或 → 无 offer | 删掉这条任务自己的 `.part` |
| 下载中 | 失败、仍是当前 offer | → `Failed` | `attempts` 加一，`retry_at` 按退避推后 |
| `Failed` | 退避到点，下一轮心跳 | → 下载中 | 重下；累计 5 次后停手等人 |
| `Staged` + 有意图 | 拿到安装根写权 | → 交接 | 改名成 `<root>/update.tar.zst`，`intent` 与 `artifact` 清空，进程退出 |
| `Staged` + 有意图 | 拿不到写权 | 不变 | 只记日志，下一轮心跳再试，已校验的字节不重下 |
| 无 offer | 用户点安装 | 拒绝 | 无 |
| `Failed` | 用户点安装 | → 下载中 | 清掉失败记录与退避，立即重下 |

启动器槽位单独维护，因为它不随客户端包 offer 重建：

| 字段 | 含义 | 何时变 |
| --- | --- | --- |
| `installed_sum` | 盘上那份启动器的摘要 | 启动时读一次；替换成功后改成任务的摘要 |
| `installed_version` | 盘上那份自报的版本号 | 同上；读不到时不判降级 |
| `tried` | 已经有结论的那一份 offer 摘要 | 开始替换或判定「不比现在新」时写入；`Deferred` 时放回空 |
| `task` | 在跑的替换任务 | 单飞判据：非空时不起第二个 |

摘要与盘上那份相同时直接返回，不记 `tried`：每轮最多多算一次摘要。盘上版本号读不到时不判降级，放行这一次；`Deferred` 是唯一会把 `tried` 放回的结论，因此它不阻止下一轮重试。

```mermaid
stateDiagram-v2
    state "空闲（没有在跑的任务）" as Idle
    state "替换中" as Replacing
    state "已有结论（tried = 这一份）" as Concluded
    [*] --> Idle
    Idle --> Replacing: 签名通过、摘要不同、版本可证明更新、这一份没试过
    Idle --> Concluded: 版本不比盘上新，记下 tried
    Replacing --> Concluded: Replaced 或 Failed，tried 保留
    Replacing --> Idle: Deferred，tried 放回空
    Concluded --> Idle: 换到另一份 offer
```

不变量：

1. 四个轴互不代理：任何「可以交接」都由 `artifact == Staged` 与 `intent` 同时成立表达。
2. 任务单飞：`task` 非空时不新起下载——`.part` 只有一个，两个线程写只会互相截断。
3. 暂存不是信号：固定名 `update.tar.zst` 只在持安装根写权提交的那一刻出现，此前工件一直叫 `.part`。
4. 只有被授权的交接才提交：`Auto` 在交接前重新校验当前策略，策略已关就放弃；`Manual` 只看 offer 还在不在。
5. 锁争用只延期：拿不到写权时暂存件原样留着，下一轮心跳重试，已校验的字节不重下。
6. 结果按代次与 offer 双重判定：任务代次不等，或当前 offer 的版本与摘要变了，这条结果连文件都不留。
7. 意图随 offer 变化一起清空，因此意图天然只指向当前 offer，不需要另存一份身份副本。
8. 失败退避按次数翻倍、上限 10 分钟，累计 `MAX_DOWNLOAD_ATTEMPTS` 次后停手等人处理。
9. 启动器替换同一份只试一次：`Deferred` 是唯一会把 `tried` 放回的结论。

| 项 | 常量 | 值 |
| --- | --- | --- |
| 包字节上限 | `MAX_UPDATE_BYTES` | 200 MiB |
| 下载总时限 | `DOWNLOAD_TIMEOUT` | 20 分钟 |
| 启动器下载总时限 | `LAUNCHER_TIMEOUT` | 10 分钟 |
| 自动重试次数上限 | `MAX_DOWNLOAD_ATTEMPTS` | 5 次 |
| 失败退避 | `retry_delay` | 30s 起每次 ×2，上限 10 分钟 |
| 启动器替换的临时目录 | `UPDATE_DIR` | `update/` |

依据：`client/crates/engine/src/update.rs` 的 `UpdateState`、`Offer`、`Artifact`、`Intent`、`DownloadTask`、`Phase`、`Engine::offer_update`、`start_offer`、`withdraw`、`Engine::apply_update`、`Engine::auto_apply`、`Engine::advance_update`、`Engine::try_apply`、`Engine::prepare`、`Engine::on_download`、`Engine::handoff`、`download_package`、`commit_package`、`retry_delay`、`MAX_DOWNLOAD_ATTEMPTS`、`MAX_UPDATE_BYTES`、`DOWNLOAD_TIMEOUT`、`LauncherState`、`Engine::apply_launcher`、`Engine::on_launcher_result`、`LauncherResult`、`LAUNCHER_TIMEOUT`、`UPDATE_DIR`、`Engine::clean_update_dir`；`client/crates/install/src/artifact.rs` 的 `is_full_digest`、`digest_file`；`client/crates/install/src/lock.rs` 的 `Lock::owner_path`。
