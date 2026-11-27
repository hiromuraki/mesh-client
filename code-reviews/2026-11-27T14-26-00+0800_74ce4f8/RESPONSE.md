# 客户端评审回应

- **对应评审**：`code-reviews/2026-11-27T14-26-00+0800_74ce4f8/REVIEW.md`
- **评审提交**：`74ce4f8`
- **回应时间**：2026-11-27T15:05:00+08:00（Asia/Shanghai）
- **范围**：本轮全部 7 条（F1 P1、F2 P2、F3 P2、F4–F7 P3）。上一轮 F1–F10 的复核结论无异议，未改动相关代码。

## F1（P1）旧格式密钥升级后读不出来 → 已修复

评审的判断成立，而且本机就是活的样本：`dist/config/client.key` 现在仍是 **32 字节裸 seed**（无 `MESHKEY1` 头）。这台机器一旦重启就会踩雷，而它当时正等着升级到 107——那条路径上必定失败。

**做法**（`crates/state/src/lib.rs`）：

| 文件 | 旧格式 | 处理 |
| --- | --- | --- |
| `client.key` | 32 字节裸 seed | `open_key` 按期望长度认出来，照用并就地重封装 |
| `secure.key` | base64 文本（44 字节） | 同上，另加一条 base64 解码分支 |
| `credential.json` | `protect` 为空、`secret` 是明文 base64 | `open_secret` 认出来，`load_credential` 就地把整个文件重写一遍 |

`open_key` 现在返回 `(明文, 要不要重写)`，`legacy_key` 负责认「裸字节」与「base64 文本」两种旧形态；认不出来才报错。`open_secret` 同形。迁移是幂等的：第二次打开已经在当前格式上，不再重写。

错误文案跟着改了：`state.error.key_not_sealed` → `state.error.key_unrecognized`，文案从「密钥是明文存的」改成「既不是 DPAPI 封装，也不是 {1} 字节的旧格式（实际 {0} 字节）」，`credential_plaintext` 同理。原文案把「认不出」说成「明文」，会把人引向错误的排查方向。

**验证**：新增三条测试：

- `a_bare_seed_key_is_used_and_resealed`：写入裸 32 字节 → 读出的密钥与写入的一致（**不能换一把**）→ 磁盘变成 `MESHKEY1` 封装 → 重开仍是同一把。
- `a_base64_secure_key_is_used_and_resealed`：跨平台，Windows 验封装、Unix 验改写成裸字节。
- `a_plaintext_credential_is_used_and_resealed`：无 `protect` 字段的旧文件 → 读出正确私钥 → 重开后仍解得开。

## F2（P2）`unchanged` 早退掐死下载重试 → 已修复

**做法**（`crates/engine/src/update.rs`）：`info.unchanged` 分支里先判 `phase == Failed`，是就调 `retry_failed_download`，再返回。没有改 `known_app_update()`：它「与进度无关」的语义是对的，错的只是早退把重试挡在了前面。

**验证**：新增 `a_failed_download_is_retried_on_a_slim_offer`（`Failed` + 退避到期 + 精简形状 → 进入 `Downloading`）与 `a_ready_offer_is_left_alone_by_a_slim_offer`（`Ready` 不被精简形状重置）。原用例保留。

## F3（P2）签名为空的旧发布记录 → 在服务端修，客户端不加状态

客户端这一侧的「每 30 秒一行日志」是**症状**：它会收到这种 offer，是因为服务端把「有摘要、无签名」的槽位当成已发布在发。根因修掉之后客户端根本收不到，再加一个「按 (version, sha256) 去重记日志」的状态只是掩盖症状，还要多一个字段和一条清除路径。

服务端改动见 `server/code-reviews/2026-11-27T14-26-00+0800_aa8da9e/RESPONSE.md` 的 F1：`launcherOf` 与 `appChanged` 都把空签名当「未发布」，启动时打一条 WARN 列出这些平台。**请下一轮评审给这条结论一个接受或驳回。**

## F4（P3）`harden_dir` 失败让客户端起不来 → 已修复

**做法**：

- `win::harden_dir` 的子项改写失败不再 `return Err`，记下最后一个错误继续；目录本身失败仍返回 `Err`（权限确实没收全，要说出来）。
- `Store::open` 不再 `?` 掉这个错误，而是存进 `Store::harden_error`，由 `Engine::spawn` 在日志建好之后写一条 `engine.log.config_harden_failed`。收敛权限是加固动作，失败要可见，但不该成为启动的硬前置。
- `run.rs` 里那句「把原因写进快照后照常起线程」与实际的 `panic!` 不符，注释改成事实：打不开 `config/` 就没法做事，直接让进程死掉，由启动器按启动确认失败回滚。

## F5（P3）`Vec<u8>` 当 ACL 缓冲区 → 已修复

改成 `let mut acl_buf = [0u64; 64]`，`InitializeAcl` 的长度改用 `size_of_val`，注释说明为什么不能用 `Vec<u8>`。

## F6（P3）id 常量与 `ALL_IDS` 手写 → 已修复（`build.rs` 生成）

评审建议的那条强收益做了，但**没有引入 `phf`**：用一个零依赖的 `build.rs` 从 `lang/chinese.lang` 生成 `id` 模块与 `ALL_IDS`，`lib.rs` 里只剩 `pub mod id { include!(concat!(env!("OUT_DIR"), "/ids.rs")); }`。

| 项 | 改动前 | 改动后 |
| --- | --- | --- |
| `crates/text/src/lib.rs` | 1035 行 | 约 450 行 |
| 手写 `pub const` | 351 行 | 0（生成） |
| 手写 `ALL_IDS` | 约 200 行 | 0（生成，`#[cfg(test)]`） |

生成时按常量名排序，两个不同 id 折成同一个常量名会直接让构建失败——这是原来手写时不存在的一道检查。`cargo:rerun-if-changed` 盯住两份清单。

## F7（P3）英文清单只为测试嵌入 → 已修复

`ENGLISH_LANG` 加 `#[cfg(test)]`：运行时不装载，也不再进 `.rodata`。测试仍旧 `include_str!` 读源文件，文件缺失同样是编译错误。

## F1 的去留（用户裁定）

本轮修复之后用户指出：旧格式密钥「读到就迁移」是**之前明确决定不做**的，理由是这类设备各自手动改一次即可，不值得在生产代码里长期留一条只为上一版数据服务的分支。

这一轮保留（107 已经发布，去掉会让那批还在旧格式上的 Windows 设备直接失联），**下一轮评审时连同它的三条测试一起移除**，届时需要在 `open_key` / `open_secret` 里留一条明确的错误文案指向「手动删掉 `config/` 重新注册」。

## 构建与测试

工作目录 `client/`，`cargo 1.100.0`，Windows。

| 命令 | 结果 |
| --- | --- |
| `cargo test --workspace` | 通过：**248** 个测试，0 失败（评审时 243，+5：state +3、engine +2） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all --check` | 通过 |

## 未自动验证项

- F1 的真机效果没有在两台 Windows 设备上实跑：本机 `dist/config/client.key` 是裸 32 字节（已核实），但让客户端重启并升级属于用户手动动作（需要 UAC）。三条单测覆盖的是同一段代码路径。
- F4 的 FAT32 / 网络盘场景无法本机复现，只验证了「子项失败不致命」的代码路径与日志文案。
- Linux（musl）构建未在本机跑：`open_key` / `open_secret` 的 Unix 分支与 `a_base64_secure_key_is_used_and_resealed` 的 Unix 断言只在 CI 覆盖。