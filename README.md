# client

Mesh 客户端的 Rust 实现。两个平台同一套安装布局：

- **Windows**：桌面形态，托盘常驻，控制面板在默认浏览器里打开（回环端口）。
- **Linux**：headless，由 systemd 拉起 `mesh`（常驻启动器），面板同样只监听回环。

安装根里的启动器 `mesh` / `mesh.exe` 也在这个仓库（`crates/launcher`）。

旧的 Go 实现在 `../client-prototype/`，只作参考、不再构建。重写过程、每个 crate 与 Go 侧的对应、
与设计文档的偏差、真机验收记录都在 [../docs/client-rust-migration.md](../docs/client-rust-migration.md)。

## 目录

| crate | 位置 | 职责 |
| --- | --- | --- |
| `mesh-install` | `crates/install` | 安装根形状、`bin/version.json`、版本序、失败计数、单实例锁 |
| `mesh-state` | `crates/state` | `config/` 下的身份与配置（`client.json`、密钥、`config.toml`） |
| `mesh-remote` | `crates/remote` | 设备面 HTTP：注册、心跳与签名、整包下载 |
| `mesh-etrpc` | `crates/etrpc` | core 的 rpc portal：帧、protobuf、三个查询 |
| `mesh-core` | `crates/core` | `easytier-core` 子进程托管（Job、输出泵、停止、就绪探测） |
| `mesh-engine` | `crates/engine` | 状态机：连接循环、心跳、配置同步、core 启停、401、更新、快照 |
| `mesh-panel` | `crates/panel` | 本地面板：回环监听、`/api` 路由、嵌入前端 |
| `mesh-client` | `crates/client` | bin `mesh-client`：CLI、单实例、线程装配、headless、托盘 |
| `mesh-launcher` | `crates/launcher` | bin `mesh`：拉起、等启动确认、装包与回滚 |

`frontend/` 是面板的前端（pnpm + Vue 3 + Vite），编译期被 `mesh-panel` 嵌进二进制。
依赖方向由 `crates/client/tests/dependency_direction.rs` 盯着。

## 构建

```pwsh
cd frontend; pnpm build      # 面板必须先有 dist，否则 cargo build 直接失败
cargo build --release        # 全部二进制：target/release/mesh-client.exe 与 mesh.exe
```

出包（编版本号、搭运行时、打 tar.zst、发布）用 mesh 根下 `build/` 的脚本，
见 [../build/README.md](../build/README.md)。

Linux 产物必须在 WSL 里编 musl 静态（目标机最低 CentOS 9 的 glibc 2.34），源码要先 rsync 到 ext4——
在 `/mnt` 下编会慢十几倍。具体命令见迁移文档的「构建环境的细节」。

## 测试

```pwsh
cargo test                   # 单元测试与集成测试
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

真机验证（托盘、UAC、自启任务、更新交接）只能人工做：Windows 客户端建 TUN 需要管理员，
启动必然走 UAC，从命令行起不来。

## 运行

```pwsh
.\target\debug\mesh-client.exe --help   # 参数表
.\target\debug\mesh-client.exe --root <目录>   # 装到别处试
```

生产形态永远经启动器：`dist/mesh.exe` 或 `dist/mesh`。安装布局、配置热更新、core 管理、
更新与自启的行为见 [../docs/client.md](../docs/client.md) 与 [../docs/updates.md](../docs/updates.md)。
