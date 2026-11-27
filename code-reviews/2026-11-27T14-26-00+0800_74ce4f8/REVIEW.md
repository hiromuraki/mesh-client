# 客户端代码审查（Rust workspace）

- **提交**：`74ce4f8670d29a14c1b23e83670223e2ba5afbbb`（完整 SHA，`main`）
- **分支**：`main`
- **审查时间**：2026-11-27T14:26:00+08:00（Asia/Shanghai）
- **审查范围**：`client/` 全仓库——`crates/{text,install,state,remote,etrpc,core,engine,panel,client,launcher}`、`easytier/`、`Cargo.*`、`.github/workflows/build.yml`。
  **不含 `frontend/`**（`.vue`/`.ts`/`.css`/`node_modules` 全部跳过，不构建、不 typecheck）。契约文档按 mesh 根 `docs/contract/` 核对。
- **审查区间**：上一轮修复提交 `d9aa922` .. `74ce4f8`，共 7 个提交：
  `a37a8a7`（更新链要求完整摘要）、`aa115ee`（日志脱敏）、`00c9598`（身份目录 DACL）、`d99fa6b`（密钥/凭据 DPAPI 封装）、
  `005444e`（文案抽成 text id）、`47b635d`（SEALED_MAGIC 只在 Windows 定义）、`74ce4f8`（发布签名）。
  上一轮评审目录 `code-reviews/2026-11-27T05-45-47+0800_da0ba2`。
- **审查开始时的工作区状态**：`git status --porcelain` 无输出（干净），`HEAD` = `74ce4f8`。
  本轮只新增本目录与 `scripts/`，未修改生产代码与测试代码，未 `git add`、未 `git commit`。评审期间被跟踪文件无变动，因此没有需要排除的脏文件。
- **判据文档**：`client/MODULES.md`、`client/DESIGN.md`、mesh 根 `docs/contract/{client,updates,device-api}.md`、`docs/security.md`、`docs/i18n.md`。

## 结论

上一轮 F1–F10 的修复全部仍然成立（逐条核对见下节）。本轮新增 7 条：**1 条 P1、2 条 P2、4 条 P3**。
最要紧的是 F1：Windows 上的密钥/凭据落盘格式在本区间被换成 DPAPI 封装，但没有任何迁移路径——现有设备升级后读不出自己的 `client.key`，
心跳永远失败且更新已被提交（见 F1）。

| 编号 | 级别 | 摘要 | 证据 |
| --- | --- | --- | --- |
| F1 | P1 | 旧格式 `client.key`/`secure.key`/`credential.json` 升级后一律读不出来，设备永久离线 | 代码 + 现有单测可证 |
| F2 | P2 | `unchanged` 早退掐死「下载失败自动重试」：真实控制面下永不重试 | 代码可证 |
| F3 | P2 | 签名为空的旧发布记录：每 30 秒一行日志、永不更新、永不换启动器 | 代码可证（两端） |
| F4 | P3 | `harden_dir` 失败让 `Store::open` 失败，客户端完全起不来 | 代码可证 |
| F5 | P3 | 用 `Vec<u8>` 当 ACL 缓冲区：对齐只由分配器施舍 | 代码可证 |
| F6 | P3 | `mesh-text` 的 id 常量与测试里的 `ALL_IDS` 都手写，加一条文案要改 4 个文件 | 代码可证 |
| F7 | P3 | 英文清单 656 行只为测试嵌入二进制（`allow(dead_code)`） | 代码可证 |

## 上一轮（`da0ba2`）修复复核

| 上轮编号 | 结论 | 依据 |
| --- | --- | --- |
| F1 STUN 线程泄漏 | 仍然成立 | `run.rs:177/1045/1049` 的 `stun_stop` 与 `recv_timeout(STUN_REFRESH_INTERVAL)`（`run.rs:1039`）都在 |
| F2 探测期间不接受 Stop | 仍然成立 | `core/thread.rs:60-76` 的 `Pending` 状态机、`TICK`=100ms、`poll_ready`（`thread.rs:167-235`）都在；`stop` 会先取 `pending`（`thread.rs:337-338`） |
| F3 `install::run` 先 recover | 仍然成立 | `launcher/install.rs:44` `recover(root)?` 是 `run` 的第一件事 |
| F4 `--file` 不删用户的包 | 仍然成立 | `install.rs:64` `let owned = pkg == root.join(UPDATE_FILE);`，两处删除都以 `owned` 为条件（`install.rs:68/84`） |
| F5 启动标记延后 | 仍然成立 | `client/main.rs:140` `boot_ready(...)` 延迟回调；`main.rs:152-153` 注释说明只生效一次 |
| F6 CI 编 Windows 启动器 | 仍然成立 | `.github/workflows/build.yml:105` `cargo build --release --workspace`、`:109` `cargo test --all` |
| F7 publish 只算一次摘要 | 仍然成立 | `views.rs:185-190` 一次 `hash`，交给 `snapshot_with_hash` 与 `beat.config_hash` |
| F8 `stop` 失败交还 `Running` | 仍然成立 | `core/thread.rs:349` `stop_process`，`:361-363` 失败时 `self.running = Some(running)` |
| F9 分片总字节上限 | 仍然成立 | `etrpc/client.rs:30` `MAX_RESPONSE_BYTES = 8 MiB`，`:88` 逐片累计检查 |
| F10 `Log::tail` | 仍然成立 | `log.rs` 只剩 `pub(crate) fn tail_lines`（`:192`），无 `pub fn tail` |

区间内的文案抽离（`005444e`）没有削弱这些修复：日志与提示文字换成了 `mesh_text::id::*`，控制流与判据未变。

## 发现

### [P1][F1] 旧格式 `client.key`/`secure.key`/`credential.json` 在升级后一律读不出来，设备永久离线

- **位置**：[crates/state/src/lib.rs:411-420](../../crates/state/src/lib.rs#L411-L420)（Windows 的 `open_key`：没有 `MESHKEY1` 头就报错，
  注释声称「旧格式只会出现在手抄或跨平台搬运的目录里」）；`signing_key` 在 [lib.rs:236-238](../../crates/state/src/lib.rs#L236-L238) 直接 `?` 传出；
  同一区间内 `secure_key` 同理（[lib.rs:273-275](../../crates/state/src/lib.rs#L273-L275)），
  凭据则是 [lib.rs:448-466](../../crates/state/src/lib.rs#L448-L466) 要求 `protect == "dpapi"`，否则报「私钥以明文保存」。
  失败一路走到 [crates/engine/src/sync.rs:54-59](../../crates/engine/src/sync.rs#L54-L59)：`connect()` 返回 `ConnectError::Failed`。
- **触发条件**：Windows 设备上已有 `config/client.key`（或 `secure.key`、`credential.json`），然后升级到本区间或之后的版本。
  这些文件是 **`d9aa922` 及更早的版本写下的**：`git show d9aa922:crates/state/src/lib.rs` 里 `signing_key` 用
  `atomic_replace(&path, &seed, 0o600)` 写裸 32 字节，`secure_key` 写裸 base64，凭据写入没有 `protect` 字段。
  本区间的 `d99fa6b` 把三者一律换成 DPAPI 封装，**没有读旧格式的分支，也没有重封装（re-seal）迁移**（全仓库只有 `lib.rs:391`/`:413` 认 `MESHKEY1`）。
- **实际结果**：升级后的客户端每次 `connect()` 都在拿设备私钥这一步失败，退避后无限重试；面板显示「读设备私钥失败」这类错误。
  设备从此不在 mesh 里（心跳不发、配置不同步、凭据不续签）。因为进程本身能起来，`--boot-check` 标记照样写出、启动器照样 `commit`，
  **自动回滚不会触发**，只能人工介入：删掉 `client.key` 会换掉设备身份（公钥变了），服务端按签名校验拒绝，需要重新注册。
  `credential.json` 与 `secure.key` 是同一个根因的另外两条（正式节点会连 core 都起不来）。
  Windows 上这条路径没有任何测试反例：唯一的「裸字节」单测 [lib.rs:567-579](../../crates/state/src/lib.rs#L567-L579) 只断言**两种错误都算拒绝**，
  注释也承认「裸字节在 Windows 上先被判成『没封装』」——即旧文件被拒绝是**已知**行为，只是没人把它和升级路径联起来。
- **修复建议**：`open_key` 在缺少 `MESHKEY1` 头时按长度判定旧格式（32 字节裸 seed = `client.key`；44 个字符的 base64 文本、解码后 32 字节 = `secure.key`），
  读到就**原样使用并把同一份明文重新封装落盘**（一次性迁移，幂等）；`open_secret` 在 `protect` 为空且 `secret` 是合法 base64 时同样迁移。
  迁移失败再报错。补一条「把裸 32 字节 key 放进 `config/` 后 `signing_key()` 仍返回同一把密钥且磁盘变成封装格式」的 Windows 单测。

### [P2][F2] `unchanged` 早退掐死「下载失败自动重试」，真实控制面下永不重试

- **位置**：[crates/engine/src/update.rs:117-119](../../crates/engine/src/update.rs#L117-L119)（`if info.unchanged { return; }` 位于函数最前面），
  而重试在 [update.rs:197-203](../../crates/engine/src/update.rs#L197-L203)（`settled && same_offer → retry_failed_download`）；
  客户端上报的「我有哪一份」是 [update.rs:92-98](../../crates/engine/src/update.rs#L92-L98) 的 `known_app_update()`。
  服务端 counterpart：`api/device_api.go:354-361`——`req.AppUpdateKnown == release.SHA256` 就只回 `{version, autoUpdate, unchanged}`。
- **触发条件**：任一平台的包下载失败一次（发布文件服务短暂 5xx、超时、磁盘满）。此时 `update.phase = Failed`，但 `update.sha256` 仍是那份 offer 的摘要，
  且 `known_app_update()` 明确「与 phase 无关，Failed 也照报」（[update.rs:86-91](../../crates/engine/src/update.rs#L86-L91)）。
  下一轮心跳把该摘要报给服务端 → 服务端认为「你已经有这一份」→ 回精简形状 `unchanged: true` → 客户端在 117 行直接返回。
- **实际结果**：`retry_failed_download`（退避 30s 起、最多 5 次、最多 10 分钟）在这条最常见的失败路径上**一次都不会被调用**：
  界面停在错误上，直到用户手动点「重试」或服务端换一份新包。文档 `docs/contract/client.md:254` 也只写了「下一轮心跳照常上报上一份 `appUpdateKnown`」，
  没有写这条上报会关掉自动重试。现有单测 `a_failed_download_is_retried_once_the_backoff_expires`（`crates/engine/src/tests.rs:772-785`）用的是**完整形状**
  （`update_info` → `unchanged: false`），所以覆盖不到；精简形状的两条用例（`tests.rs:891-933`）起点都是 `Ready`/`Idle`，没有 `Failed`。
- **修复建议**：把 `unchanged` 的早退放到「失败重试」之后：先按 `settled && same_offer` 处理（失败则重试），再判断 `unchanged`；
  或者让 `known_app_update()` 在 `phase == Failed` 时返回 `None`（此时「我手上有这一份」本来就是假的）。
  无论哪种都补一条「`slim_update_info` + `phase = Failed` + 退避已到期 → 进入 `Downloading`」的单测。

### [P2][F3] 签名为空的旧发布记录：每 30 秒一行日志、永不更新、永不换启动器

- **位置**：[crates/engine/src/update.rs:173-189](../../crates/engine/src/update.rs#L173-L189)（验签不过就 `logf` + `return`），
  [update.rs:500-513](../../crates/engine/src/update.rs#L500-L513)（启动器同样先验签再判重）；
  服务端 counterpart：`db/db.go:148-151` 给存量行补的是 `DEFAULT ''`，`api/device_api.go:264-274` 的 `launcherOf` 对「有摘要、无签名」的启动器槽位照样下发。
- **触发条件**：控制面升级到带签名的版本时，`release` 表里已经有一行（升级前发布过客户端包或启动器），它的 `signature`/`launcher_signature` 是补列时的空串。
- **实际结果**：设备每轮心跳都收到同一份 offer，每次都在验签处失败并写一行 `engine.log.update_signature_invalid`——30 秒一行，
  一天 2880 行，设备侧没有任何「这份记录根本不可用」的区分；启动器同理，`apply_launcher` 每轮心跳失败并写 `engine.log.launcher_signature_invalid`。
  服务端只看到「已发布」，管理面板也显示有版本，没有任何地方提示「这一行永远发不出去」。
  （注：`launcher_signature` 为空时 `launcherOf` 仍然返回非 nil，所以不是「不发」而是「发了必被拒」。）
- **修复建议**：客户端把验签失败与「摘要有问题」一样按「这次 offer 不存在」处理，并对同一 `(version, sha256)` 只记一次日志（与已有的失败计数一样按版本去重）；
  服务端在发布/启动时把「有摘要无签名」的槽位视为未发布（`launcherOf` 返回 nil、`appChanged` 置假），并打一条启动告警列出这些平台，等重新发布。

### [P3][F4] `harden_dir` 失败让 `Store::open` 失败，客户端完全起不来

- **位置**：[crates/state/src/lib.rs:150-153](../../crates/state/src/lib.rs#L150-L153)（`create_dir_all` 之后立刻 `harden_dir(&dir).map_err(...)?`），
  实现见 [crates/state/src/win.rs:31-89](../../crates/state/src/win.rs#L31-L89)：目录 ACL 改完还要**逐个改写目录下每个子项**，任一子项 `SetNamedSecurityInfoW` 失败就 `return Err`；
  调用方 [crates/client/src/main.rs:74](../../crates/client/src/main.rs#L74) 用 `?` 把错误抛出 `main`。
- **触发条件**：`config/` 落在不支持 ACL 的卷（FAT32/exFAT U 盘、部分网络盘），或某个子项属于别的账户/被占用导致 `WRITE_DAC` 被拒。
- **实际结果**：客户端直接退出，不写日志、不进托盘、不连网；用户看到的是「打不开」。收敛权限是加固动作，却成了启动的硬前置。
  另外 [crates/engine/src/run.rs:194-199](../../crates/engine/src/run.rs#L194-L199) 的注释写「把原因写进快照后照常起线程，让面板能显示它」，代码却是 `panic!`——注释与实现不符。
- **修复建议**：目录本身的 ACL 失败可以致命（可选），子项改写降级为逐项记日志继续；`run.rs` 的注释与 `panic!` 二者改一致（面板能显示就写快照，不能就删掉这句注释）。

### [P3][F5] 用 `Vec<u8>` 当 ACL 缓冲区：对齐只由分配器施舍

- **位置**：[crates/state/src/win.rs:36-38](../../crates/state/src/win.rs#L36-L38)：`let mut acl_buf = vec![0u8; 512]; let acl = acl_buf.as_mut_ptr() as *mut ACL;`，
  注释写「ACL 结构要求 4 字节对齐，Vec<u8> 的分配满足这一点」。
- **触发条件**：任何一次 `harden_dir`。Rust 只保证 `Vec<u8>` 的分配按 1 字节对齐；今天能跑是因为 Windows 堆对小块分配返回 16 字节对齐的地址。
- **实际结果**：这是类型系统外的一次未定义行为假设，换分配器、换 `#[global_allocator]`、或将来把缓冲区改成 `Vec<u8>` 的切片来源（例如 `&mut buf[1..]`）就会踩到；
  而且失败形态是 `InitializeAcl`/`AddAccessAllowedAceEx` 静默返回错误或写坏内存，难以定位。
- **修复建议**：用 `#[repr(align(8))] struct AclBuf([u8; 512])` 或 `let mut acl_buf = [0u64; 64];` 再取 `as_mut_ptr() as *mut ACL`。

### [P3][F6] `mesh-text` 的 id 常量与测试里的 `ALL_IDS` 都手写，加一条文案要改 4 个文件

- **位置**：[crates/text/src/lib.rs:26-376](../../crates/text/src/lib.rs#L26-L376) 是 351 行手写的 `pub const`，
  测试里的 `ALL_IDS`（[lib.rs:545](../../crates/text/src/lib.rs#L545) 起）又是一份逐项列举的数组（约 200 行）。
- **触发条件**：新增/改名/删除任何一条文案。
- **实际结果**：一次改动要在 `lang/chinese.lang`、`lang/english.lang`、`id` 模块、`ALL_IDS` 四处同步；漏了 `id` 是编译错误（可接受），
  漏了 `ALL_IDS` 只是少一条覆盖（不会红），而漏了某份 `.lang` 由 `the_two_catalogues_define_the_same_keys` 兜住。
  真正没有兜住的是「`ALL_IDS` 少一项」——那正好是唯一用来证明 `id` 与清单一一对应的表。
- **修复建议**：加一个 `build.rs` 从 `lang/chinese.lang` 生成 `id` 常量与 `ALL_IDS`（约 40 行 build 代码可省 550 行手写代码，并把「清单与代码不同步」提前到编译期）。
  详见下文「引入库的收益分析」第 1 条。

### [P3][F7] 英文清单 656 行只为测试嵌入二进制

- **位置**：[crates/text/src/lib.rs:17-20](../../crates/text/src/lib.rs#L17-L20)：`ENGLISH_LANG` 用 `include_str!` 嵌入、`#[allow(dead_code)]` 压掉未使用告警，
  运行时不装载（`chinese()` 只建中文表，[lib.rs:393-397](../../crates/text/src/lib.rs#L393-L397)）。
- **实际结果**：`crates/text/lang/english.lang` 329 行 / 23 KB 是「只为测试存在的数据进二进制」——与上一轮 F10（`Log::tail` 只为单测存在）同一类问题，
  只是这次进的是 `.rodata` 而不是 API 面。它的**唯一**用途是让 `the_two_catalogues_define_the_same_keys` 有东西可比。
- **修复建议**：要么让英文清单真的可切换（一个 `OnceLock<&str>` + 环境变量/编译期 feature，成本约 10 行），要么把它挪到 `tests/` 下用 `include_str!("../lang/english.lang")` 只在测试里读。

## 引入库的收益分析

本节只评估本区间新增/改动的代码。行数均为实测（`(Get-Content x).Count` 或 diff 统计），不取整。

### 1. 值得引入（强收益）

**1.1 `phf` / `phf_codegen`（构建期生成 id 与查表）——建议做**

- **现状**：`crates/text/src/lib.rs` 1035 行 = 手写 id 常量 351 行（`:26-376`）+ 运行期解析/校验/渲染 159 行（`:378-536`）+ 测试 498 行（`:538-1035`，其中 `ALL_IDS` 约 200 行手写）；另有两行空行，合计 1035 行；
  另有 `lang/chinese.lang` 327 行、`lang/english.lang` 329 行。
- **候选**：`phf`（运行时）+ `phf_codegen`（仅构建期）0.11；也可以只用 `build.rs` 生成一个 `match` 函数（零依赖）。
- **收益**：省掉 `id` 模块 351 行与测试 `ALL_IDS` 约 200 行（合计约 550 行）；`parse()` 从运行期挪到构建期，
  `chinese()` 的 `OnceLock<HashMap>`、`parse` 失败时的 `panic!` 与整个 `ParseError` 类型（22 行，`lib.rs:429-446`）都可以删掉；
  「清单与 id 不同步」从测试期提前到编译期，正好补上 F6 里那个没被兜住的缺口。
- **代价**：构建期依赖 `phf_codegen`（不进产物）、运行时依赖 `phf`（零传递依赖，`#[forbid(unsafe_code)]` 仍然成立但 `phf` 内部有 unsafe）；
  或者走零依赖的 `build.rs` + 生成 `match`，编译时间增加可忽略（几十行生成代码）。
- **建议**：做（至少做「生成 id 常量 + `ALL_IDS`」这一步，它不改变运行时形态）。

### 2. 可以引入但收益一般

1. **`zeroize`**（客户端，1.x）：现在 `open_key` 返回 `Vec<u8>`、`open_secret` 返回 `String`（[state/src/lib.rs:236-258](../../crates/state/src/lib.rs#L236-L258)、`:273-296`、`:448-466`，密钥缓冲相关约 60 行）。换成 `Zeroizing` 能减少换页/转储泄漏面，但不改变任何可观测行为，且要改约 60 行签名——收益小于风险，不列强收益。
2. **`golang.org/x/text/message` + `language`**（服务端）：`text/text.go` 只有 151 行、`text/ids.go` 49 行，自带解析/渲染约 60 行。收益是省这 60 行并拿到复数规则；但清单是自定义 `.lang` 格式，解析仍要自己写，且要新引 `x/text`（`x/crypto` 已在，但 `x/text` 是另一棵树）——省的行数与引入的体积不成比例。
3. **`fluent`/`rust-i18n`**（客户端）：能替掉 `mesh-text` 的 159 行渲染逻辑，但会引入 `fluent-bundle`+`fluent-syntax`+`unic-langid` 等一串 crate，且它们同样不生成 id 常量（F6 的痛点不解决），而本项目只有两种语言、无复数、无运行时切换。
4. **`go-i18n/v2`**（服务端）：除与上条同样的理由外，还会把消息格式换成 ICU MessageFormat，等于给 103 行清单引入一门新语法。
5. **`testify`**（服务端）：`api/api_test.go` 现在有 1700+ 行手写 `t.Fatalf` 断言。省的是样板，不是逻辑；项目风格已经统一，纯增益但不值得为此改存量。

### 3. 不该引入

1. **DPAPI 封装库**（`dpapi`/`win-dpapi` 之类）：`windows-sys` 已经在依赖里，`win.rs:92-149` 的 `protect`/`unprotect` 一共 58 行，两个 API 调用 + 两次 `LocalFree`。多一个库反而多一份对 `DATA_BLOB` 布局的依赖。
2. **换掉 `ed25519-dalek` / `hex` / `sha2`**：发布签名（`crates/remote/src/signing.rs` 157 行）与设备请求签名（`crates/remote/src/lib.rs:301-320`）都建在已有依赖上，本区间**没有引入任何新外部依赖**（`Cargo.lock` 只多了 workspace 内的 `mesh-text` 与 engine 的 dev-dependency）。这是对的，别动。
3. **`minisign`/`sigstore`/`cosign` 替代自研签名串**：这些工具签的是**文件**，而这里要签的是「服务端自己算出来的摘要 + 槽位 + 平台 + 版本」——即服务端无法伪造的元数据绑定，签名时私钥在离线机器上、被签的字节在控制面上。minisign 表达不了这个形状（只能先传文件过去），换成它们会把「离线私钥只签摘要」这个核心性质弄丢。
4. **`subtle`/常数时间比较**：`update.rs:196/701` 与 `apply_launcher` 的摘要比较用 `eq_ignore_ascii_case`。摘要是公开值，不是秘密，常数时间比较没有威胁模型。
5. **用 `tracing` + 一层 subscriber 取代 `crates/engine/src/redact.rs`（186 行，含 55 行测试）**：脱敏规则是「哪些键名算秘密」这一业务知识，`tracing` 的 `field::Visit` 也得自己实现一遍，还要把全仓库的 `logf` 迁到 `tracing` 宏——为 186 行引入一套新日志栈。
6. **给 `etrpc` 换 protobuf/gRPC**：上游 EasyTier 的 management RPC 不是 gRPC（自研 protobuf + 分片 + 事务），没有现成客户端库；这是既有结论，本区间未改，仅记录。

## 待确认

1. **`client.key` 的旧格式是否真的存在于线上**。F1 的前提是「已部署的 Windows 客户端写过裸 32 字节 `client.key`」——`git show d9aa922` 证实那个版本这么写，
   但本机 `dist/` 里跑的是哪一版无法从仓库判断。若线上全部是 `d99fa6b` 之后的版本，F1 降为「跨平台搬运/手抄目录」的边界问题。建议直接看任意一台上线机器的 `config/client.key` 前 8 字节是不是 `MESHKEY1`。
2. **Windows 上 `atomic_replace(..., 0o600)` 的实际权限**。`mesh-install` 的 `atomic_replace` 在 Windows 上只能设只读位，真正的权限来自父目录的继承 ACE；
   本区间的 `harden_dir` 补上了这一点，但「先建目录再收紧」之间仍有一个短暂窗口（`lib.rs:150-153`）。窗口内是否可能已有密钥落盘（例如并发启动两个实例），没验证。
3. **`credential.json` 旧格式在正式节点上的后果**。正式节点不读 `credential.json`（用 `secure.key`），所以 F1 的第三条只影响凭据节点；
   凭据节点本来就会因为 `client.key` 先失败，两条的先后顺序没有实测。
4. **`docs/contract/client.md:254` 的措辞**。它写「任一项不通过都按『没收到这份 offer』处理……下一轮心跳照常上报上一份 `appUpdateKnown`」，
   与 F2 描述的「上报会导致服务端回精简形状、从而关掉重试」是同一段事实的两种读法。修 F2 之前建议先确认契约想表达哪一种。
5. **`mesh_text::format` 在 release 构建里少给参数会静默留空**（`lib.rs:385-391` 的 `debug_assert!`）。单测在 debug 下能全绿，
   release 下参数个数错了只是文案里少一段。是否有意为之（注释说了是有意），以及是否值得改成编译期检查，留给设计层判断。

## 构建与测试（实际执行）

工作目录 `S:\local\repos\mesh\client`，工具链 `cargo 1.100.0 / rustc 1.100.0`（`rust-toolchain.toml` 钉住），Windows。

| 命令 | 结果 |
| --- | --- |
| `cargo test --workspace` | 通过：**243** 个测试，0 失败。分布：client 17、dependency_direction 2、core 8、managed_core 7、engine 75、etrpc 27、install 27、launcher 10、panel 26、remote 15、state 15、text 14 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（`Finished dev profile`，无警告） |
| `cargo fmt --all --check` | 通过（无输出，退出码 0） |
| `cargo test -p mesh-engine redact` | 7 个 `redact::tests` 全过（复核脱敏行为用） |

`scripts/` 里只有一条探针：`probe-redact-line.ps1`。它把 `crates/engine/src/redact.rs` 原样复制到临时 crate 里跑 `line()`，
用来确认「JSON 值被整段替换、引号配平」这个我一开始读错的细节（结论：实现正确，`{{"credential_secret":***,"n":1}}`，**不是** finding）。
F1/F2/F3 都是纯代码可证的结论，没有可离线复现的探针：F1 需要一台真有旧格式 `client.key` 的 Windows 机器，F2 需要 `pub(crate)` 的引擎内部状态（写探针等于改测试，本轮不允许）。

## 未验证项

- Windows 托盘真机行为与 UAC 提权链路（F1 的实际后果在真机上，本轮只按 `sync.rs:54` 的调用链推断）。
- Linux（musl / systemd）构建与运行：本机没有 musl 目标，`cargo test --workspace` 只在 Windows 上跑过；`state` 的 DPAPI 分支在 Linux 上根本不编译，`open_key`/`open_secret` 走 `#[cfg(not(windows))]` 的直通实现。
- `easytier/mini.patch` 与内核构建：本轮未重跑 `build/easytier.ps1`。
- 控制面一侧的行为（服务端对旧发布行的处理）：按 `api/device_api.go` 与 `db/db.go` 的代码核对，未起两端的联调环境。
- 长跑内存/线程曲线：本轮没有做。
