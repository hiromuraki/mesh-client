# easytier-mini 的本地改动

上游 2.7.1 的 `easytier-contrib/easytier-mini` 是个精简节点：体积约为完整 core 的一半，
常驻内存也小一档。但它手写的参数解析不认我们的身份入口，直接用会连不上网。这里的补丁把它补齐。

## 我们依赖的三个入口

客户端启动 core 时只会给这些（`client/crates/engine/src/run.rs` 的 `start_core`，密钥一律走环境变量，
不进命令行——命令行对同机其它用户可读）：

| 节点类型 | 传给 core 的东西 |
| --- | --- |
| 正式节点（三个种子） | `--secure-mode` + `ET_LOCAL_PRIVATE_KEY=<secure.key>` |
| 凭据节点（其余设备） | `ET_CREDENTIAL=<凭据私钥>` |

另有 `--hostname <设备名>`：设置页里的设备名必须真的出现在别的节点的表里。

## 补丁改了什么

补丁文件：`client/easytier/mini.patch`，对 `easytier-contrib/easytier-mini` 的两处改动。

1. `Cargo.toml`：增加 `easytier-core` 依赖（只为打开它的 `chacha20` 特性），并把
   `easytier` 的特性收窄到实际用得到的几个，见下节。
2. `src/main.rs`：
   - 从 `ET_CREDENTIAL` / `ET_LOCAL_PRIVATE_KEY` 读密钥（上游只认命令行开关，客户端按
     约定只给环境变量）；
   - `load_local_config` 在**装载之前**把 `secure_mode` 写进配置文本，再交给 loader；
   - 装载之后设 `--hostname`。

规则与完整版 core 的 `merge_into` 一致。**不要**加 `--credential` / `--local-private-key`
命令行开关：客户端从不这么传，加了就是死代码。

### 为什么在装载前注入 secure_mode

core 在**装载 TOML 的那一刻**就定下了网络身份（`easytier-core/src/config/toml.rs` 的
`new_from_config`）：

| 装载时看到 | 定型结果 |
| --- | --- |
| 有 `network_secret` | 正式节点，secret 就是口令 |
| 无 `network_secret`、`secure_mode.enabled` | 凭据节点，secret 为 `None` |
| 其余（含 `secure_mode` 还没打开） | 正式节点，secret 为空串 |

装载完再调 `set_secure_mode` 只换得来密钥、换不来身份，凭据节点会退化成「正式节点 + 空
secret」，握手时按空 secret 算一个 proof 发出去，种子以
`authentication failed: invalid proof and unknown credential` 拒掉。

因此补丁走文本注入：读配置原文 → 按原文装载一次（只为拿到文件里可能存在的密钥）→
算出这次要用的 `secure_mode` → 丢掉原文里可能残留的 `[secure_mode]` 段、在文末追加新的
→ 重新装载。身份定型、密钥规范化、公钥推导都由 core 自己完成，补丁不复制任何判据。

## 编进去的特性

`easytier` 只开：`aes-gcm`、`icmp-proxy`、`logging`、`management-rpc`、`proxy-cidr-monitor`、
`tcp-hole-punch`、`tun`；core 额外开 `chacha20`。

| 特性 | 为什么留 |
| --- | --- |
| `tun` | mesh 网段就是一块虚拟网卡；顺带启用 `linux-netlink` |
| `management-rpc` | 客户端经 RPC portal 读节点表与连接状态 |
| `tcp-hole-punch` | TCP 打洞 |
| `aes-gcm` + `chacha20` | 两种 AEAD 都编进去，配置里写哪个都行 |
| `logging` | core 日志进 `client.log`，排障全靠它 |
| `icmp-proxy` | 它同时打开 core 的 `proxy-packet`——Windows 构建里 `easytier` 的
  `windows_udp_broadcast` 无条件引用 `gateway::udp_broadcast`，关掉这个模块直接编译失败。
  该模块是 UDP 广播中继（局域网游戏发现依赖它） |
| `proxy-cidr-monitor` | 同样不能省：`virtual_nic` 初始化时会去找这个 host，没编进去每次
  启动都报一条 `ERROR proxy CIDR monitor host is unavailable` |

去掉的：`dhcp-ipv4`（地址由控制面写死；EasyTier 自带的 DHCP 只按在线对端避让，会与掉线
设备撞地址）、`proxy-smoltcp-stack`（用户态 TCP/IP 栈，只服务整段子网代理）、`web-client`
（连 EasyTier 官方 web 控制台的那条路，配置由我们自己的控制面下发，用不到）。`smoltcp`
本身仍会随 `tun` 特性被编进来，只是没启用那部分功能。

## 与完整版的差异

完整版默认还带 `quic`、`kcp`、`websocket`、`wireguard`、`faketcp`、`socks5`、`upnp`、
`magic-dns`、`dns-resolver`、`endpoint-discovery`、`extended-services`、`vpn-portal`、`zstd`、
`lzo`、`ring-crypto` 以及 manager 能力。mini 这些都没有：它只做「TUN + UDP/TCP 隧道 +
打洞 + 子网通告」，不做网关、不做协议封装、不做发现。

## 重新打补丁与构建（上游更新后）

```pwsh
# 1. 改 client/easytier/upstream.txt 里的 commit
# 2. 清掉旧补丁，让脚本按新提交重打
git -C .tmp/easytier checkout -f
.\build\easytier.ps1      # 拉取 → 打补丁 → 编译 → 拷产物 → 自检
```

GitHub 直连不通时给 git 加代理：`$env:HTTPS_PROXY='http://127.0.0.1:7897'`。

`build/easytier.ps1` 是幂等的：先用 `git apply --check` 试运行，已打过就跳过。有冲突基本只会在这两处：
`parse_args` 的参数循环、`main` 里的 `load_local_config` 调用。

构建前三个本地依赖要指到位（否则 `build.rs` 会去 GitHub 下载，在没直连的机器上会一直挂着不报错），
脚本会补默认值：

```pwsh
$env:PROTOC='C:\Users\11717\tools\protoc\bin\protoc.exe'
$env:VC_LTL='C:\Users\11717\tools\VC-LTL'
$env:YY_THUNKS='C:\Users\11717\tools\YY-Thunks'
```

产物落在 `mesh/dist/runtime/windows-amd64/easytier-core.exe`，约 8.8 MiB（完整 core 22 MB）。
脚本最后会跑 `--version` 与 `--credential x`：后者必须报 unknown argument，补丁没生效时它会接受。

## 验证

`build.ps1` 跑完已经做过两条自检：`--version` 报 `easytier-mini 2.7.1`，`--credential x` 必须被拒
（补丁没生效时它会接受）。

要看它真能入网，就起一个隔离网络自检（`no_tun = true`、独立 `--rpc-portal`），再用客户端面板的
`/api/dashboard` 读节点表：它应该在里面，`hostname` 与传进去的一致。

## 可复现性

同一份源码连编两次，产物**不是**逐字节相同：9,247,232 字节里有 24 字节不同，其余完全一致。

| 差异位置 | 内容 |
| --- | --- |
| PE 头 `TimeDateStamp` | 链接时刻的 unix 时间 |
| 调试目录里的三处时间戳副本 | 同上 |
| `RSDS` 记录 | 调试信息 GUID（每次链接随机） |

代码与数据段逐字节相同，差异不影响行为，也不影响发布链：控制面只按 `bin/version.json` 里
每份文件的 sha256 校验，内核摘要不进任何判断。要让哈希也对齐，得在链接后把这几处置零，
收益只有「本地与 CI 产物同哈希」，当前不做。

## 已知差异

- **没有 `dns-resolver` 特性**：查不了 TXT/SRV 记录，于是 `txt:stun.easytier.cn` 这类 STUN 地址
  取不到（日志：`get txt record failed ... this build does not include TXT DNS resolution`）。
  `txt:` 前缀以外的主机名与对端 URI 的 A/AAAA 记录走的是 `getaddrinfo`，不受影响。
- **没有完整的加密后端**：mini 用的是 RustCrypto 的 `aes-gcm` 与 `chacha20`，没有
  `ring`/`openssl` 那套硬件加速。算法本身与完整版互通，但吞吐靠 CPU 指令集。
- **`ReadOnlyApiRpcServer`**：mini 永远不是 EasyTier manager，不能签凭据，所以 ali-seed 必须留着完整版。
- 不认 `--rpc-portal-whitelist`：只有设置了 `MESH_RPC_WHITELIST` 时客户端才会传，当前部署没有用。

## 打包注意

发布包按平台装不同的 core：

- Windows 包带 mini（`dist/runtime/windows-amd64/easytier-core.exe`），完整版备份在
  `dist/runtime/windows-amd64-full/easytier-core.exe`。Windows 上没有种子也没有 manager，mini 的能力够用。
- Linux 包始终是完整版：三个种子要在上面当 manager 或至少保持完整功能。

自更新只换 `bin/`，所以 Windows 上换版本不会把 mini 换回完整版；要改的还是源头那份运行时母本。
