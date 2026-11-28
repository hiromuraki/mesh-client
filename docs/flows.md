# 客户端流程图

依据 `client/`（Rust）的当前实现绘制：启动器在 `crates/launcher`，客户端在 `crates/client`。图中常量均取自代码：心跳 30s、请求超时 20s、core 就绪 25s、停止 core 5s、core 重拉退避 1s→30s、连接退避 10s→60s、面板端口 11020（最多顺延 20 个）、更新包下载 20 分钟、更新包上限 200MB、启动确认 60s、启动器等旧进程退出 60s。

安装目录布局、槽位与提交规则见 [contract/updates.md](../../docs/contract/updates.md)。

## 1. 心跳（heartbeat）

连接成功后立即发一次，此后每 30s 一次；响应只携带发生变化的内容，配置变化触发同步，`appUpdate` 触发后台下载。`credentialId` 报的是 core 实际加载的那张凭据，不是刚写进磁盘的那张——落盘到 core 重启之间还有一段时间，提前上报会让控制面吊销正在使用的那张。

```mermaid
sequenceDiagram
  participant E as 客户端引擎
  participant S as 控制面

  loop 连接成功后立即一次，此后每 30s
    E->>S: POST /api/v1/heartbeat（configHash、appVersion、credentialId，20s 超时）
    alt 请求失败或超时
      Note over E,S: 非 401 降级为 degraded；401 按「心跳与 401」一节处理
    else 成功
      alt 什么都没变
        S-->>E: 204 No Content（无正文）
      else 有变化
        S-->>E: 200（只带变化项，可能含新凭据、appUpdate、launcher）
      end
      Note over E: 记录 lastHeartbeat；degraded 恢复 online
      opt credentialSealed 非空
        Note over E: 用设备私钥解封；解不开即本轮作废（不回落明文）<br/>落地 credential.json 并重启 core（core 不支持热更新身份）
      end
      opt appUpdate 非空且 autoUpdate
        Note over E: 下载新包（见第 3 节）<br/>已就绪或下载中则忽略
      end
      opt appUpdate 非空但 autoUpdate 为 false
        Note over E: 只提示，等用户点「立即更新」
      end
      opt configHash 不同
        Note over E: 应用新配置（见第 2 节）
      end
      opt core 没在跑
        Note over E: 让主循环重走一遍连接流程，补启动 core
      end
    end
  end
```

![心跳时序](./flows/01-heartbeat.svg)

## 2. 配置更新（config apply）

心跳报告 hash 与服务端不同才下发配置；与磁盘上的 `config.toml` 逐字节相同就不重写文件、也不重启 core——唯一例外是 core 手里的凭据已经不是本地那张：换凭据只能换进程。core 起不来不回滚，下一轮心跳继续尝试。

```mermaid
flowchart TD
  A["管理页修改设备配置<br/>configHash 变化"] --> B["心跳上报旧 hash<br/>响应带 configHash + configB64"]
  B --> C{"与磁盘 config.toml<br/>逐字节相同？"}
  C -- 是 --> D{"core 在运行<br/>且凭据没换？"}
  D -- 是 --> E["结束：不重写文件、不重启"]
  D -- 否 --> F["补启动 core<br/>凭据换了则换进程"]
  C -- 否 --> G{"core 在运行？"}
  G -- 是 --> H["停旧 core<br/>Windows: taskkill /T，失败再 /F<br/>Linux: 进程组 SIGTERM，5s 后 SIGKILL"]
  H -- 停不掉 --> I["中止并报错"]
  G -- 否 --> J["原子写 config.toml<br/>临时文件 + rename"]
  H --> J
  J --> K["启动新 core<br/>25s 内查 rpc portal<br/>确认 node 就绪"]
  K -- 成功 --> L["同步完成<br/>core 在跑才报 online"]
  K -- 失败 --> M["状态 error，不回滚<br/>下一轮心跳继续尝试"]
```

![配置更新流程](./flows/02-config-apply.svg)

## 3. 应用更新（self update）

是否更新只看心跳里的 `appVersion` 与发布记录 `version` 的版本序（`YYYY.MM.DD-N`，按日期与当日序号数值比较）。`autoUpdate` 为 true 时客户端自己装，false 时只提示；同一版本连续失败 2 次后不再自动装。

客户端只负责下载与交接；解包进候选槽、校验、拉起、等确认由启动器做；提交 `bin/current` 由客户端在启动成功时做。两个平台的差别只在交接方式：

- **Linux**：客户端先把整包摘要写到 `<安装根>/update.tar.zst.sha256`，再把包改名成 `update.tar.zst`，然后正常退出。常驻启动器（systemd 的 `ExecStart`）看到子进程退出就回到循环开头：先按摘要自算比对，不符就丢弃包与摘要、不碰候选槽；相符再读包里的 `version.json`，不比当前槽新就丢弃，否则清空候选槽、解包、校验、拉起。确认以 `current` 变成该槽为准；旧槽不在确认那一瞬删，留到下一次启动、选槽之前清理。失败则停掉候选、`current` 不动。
- **Windows**：包已经在本地，客户端拉起 `mesh.exe --file <包> --wait-for <pid> --sha256 <摘要>` 后退出。启动器等旧进程退出（≤60s）、清空候选槽、解包、校验、拉起，等 `current` 变化；失败就停掉候选并把当前槽拉回来；旧槽同样留到下一次启动、选槽之前清理。

```mermaid
sequenceDiagram
  participant E as 客户端引擎
  participant S as 控制面
  participant L as 启动器 mesh（.exe）
  participant N as 候选槽

  S-->>E: 心跳 200 appUpdate（version、url、notes、sha256、autoUpdate）
  Note over E: 置 downloading；已在下载或已就绪则忽略<br/>autoUpdate=false 时只提示，等用户点
  E->>S: GET appUpdate.url（整包 tar.zst，≤200MB，20 分钟超时）
  S-->>E: 字节流
  alt 下载失败或 sha256 不符
    Note over E: 删除包，状态 error，记一次失败
  else 校验通过
    alt Linux
      Note over E: 先落 update.tar.zst.sha256，再改名成 update.tar.zst<br/>停资源后正常退出
      Note over L: 子进程退出 → 按摘要自算比对<br/>不符即丢弃；相符再读包内 version.json<br/>不比当前槽新则丢弃
      Note over L: 清空候选槽 → 解包 → 校验清单
      L->>N: 拉起候选槽的 mesh-client，等启动确认（≤60s）
      Note over E,L: 启动成功后客户端原子写 bin/current = 该槽名
      alt current 变成该槽
        Note over L: 旧槽留到下一次启动、选槽之前清理
      else 子进程退出或超时
        Note over L: 停掉候选槽；current 不动、候选槽保留<br/>记一次失败（同一版本 2 次后不再自动装）
      end
    else Windows
      Note over E: 包已在本地
      E->>L: --file 包 --wait-for 进程号 --sha256 摘要
      Note over E: 回调 quit，客户端退出
      Note over L: 等旧进程退出（≤60s）→ 清空候选槽 → 解包 → 校验 → 拉起
      Note over L: 确认失败则停候选、拉回当前槽，current 不动
    end
  end
```

![应用更新时序](./flows/03-app-update.svg)

## 4. 应用启动（startup）

启动器只拉起 `bin/current` 选中的槽。槽名固定为 `alice`、`bob`，启动器与程序路径集合都不随版本变化，防火墙规则与服务配置只认这两条。选槽顺序：`current` 有效就用它；否则在有效槽里取目录名字典序最大的（`bob` 优先）；两个都无效即没有可启动版本。启动器从不提交，只在 `current` 缺失、非法或指向无效槽时补写一次（修复）。

客户端进程先取单实例锁，再读本地身份；没有身份时起本地面板等用户填地址与 token，有身份时打一次心跳取配置与凭据、起 core、等就绪后进入心跳循环。界面是浏览器里的面板（`127.0.0.1:11020`，被占用就顺延），启动时**不再自动打开浏览器**，托盘菜单里的「打开面板」才开。起 core 需要身份：正式节点用 `--secure-mode` 加 `ET_LOCAL_PRIVATE_KEY`，其余节点等控制面签发凭据、私钥走 `ET_CREDENTIAL`；两把私钥都不进命令行。没有凭据就先停在 `error` 并让心跳继续跑。启动成功（引擎与面板就位；桌面版还要求托盘起来）后，客户端若发现自身可执行文件的父目录是槽名，就原子写 `bin/current` = 该槽名；启动器在 60 秒内看到它即确认成功。

```mermaid
sequenceDiagram
  participant L as 启动器
  participant M as mesh-client
  participant P as 本地面板
  participant S as 控制面
  participant C as easytier-core

  L->>L: 拿到 install.lock；current 与另一个槽都有效时先删另一个槽，并清旧布局残留
  L->>L: 读 bin/current；缺失、非法或槽无效 → 按选槽顺序选一个并补写（修复）
  L->>L: 有 update.tar.zst 且比当前槽新 → 清空候选槽、解包、校验
  L->>M: 启动选中槽的 mesh-client --root 安装根
  M->>M: lockfile.Acquire(instance.lock)：已有实例则唤起面板后退出
  M->>P: 监听 127.0.0.1:11020（占用则顺延，最多 20 个端口）
  M->>L: 启动成功，原子写 bin/current = 所在槽名
  Note over L: 60s 内 current 变成该槽 → 确认成功；子进程退出或超时 → 失败
  M->>M: 读 config/client.json
  alt 无身份（没有 client.json 或 deviceId 为空）
    M->>P: 停在设置页，等用户填服务端地址与 token
    M->>S: POST /api/v1/register（token 一次性，10 分钟有效）
    S-->>M: deviceId、configToml、configHash
    M->>M: 保存身份、写 config.toml，触发重启 core
  else 已有身份
    M->>S: POST /api/v1/heartbeat（首轮，20s 超时）
    S-->>M: configHash + configB64（有变化时），可能带首张凭据
    M->>M: 内容相同且文件存在则不重写
  end
  M->>C: core --config-file config.toml --rpc-portal loopback 地址
  Note over M: 身份二选一：正式节点 --secure-mode + ET_LOCAL_PRIVATE_KEY<br/>其余节点 ET_CREDENTIAL；没有凭据就不启动 core
  M->>C: ShowNodeInfo 轮询（500ms 间隔，25s 上限）
  C-->>M: 节点就绪且 hostname / ipv4 匹配
  Note over M: 状态 online，进入每 30s 的心跳循环
  Note over M: 托盘菜单：打开面板（调系统浏览器）、退出
```

![应用启动时序](./flows/04-startup.svg)

## 5. 应用退出（shutdown）

托盘「退出」、SIGINT/SIGTERM 与更新交接才真正结束进程：先停 core（Windows 走 `taskkill /T`，失败再 `/F`；Linux 先给进程组 `SIGTERM`，3 秒不退再 `SIGKILL`），再关日志与面板监听，最后释放单实例锁。

Linux 上客户端退出即「交接」：常驻启动器就是 systemd 的主进程，客户端只是它的子进程，退出不代表服务停止。Windows 上启动器在确认客户端起来之后就已经退出，客户端退出后需要用户再双击。

```mermaid
sequenceDiagram
  participant UI as 托盘与信号
  participant E as 引擎
  participant C as easytier-core
  participant M as 主程序
  participant L as 启动器

  alt 托盘「退出」或 SIGINT/SIGTERM
    UI->>E: quit() 先调用 Shutdown()
    Note over E: cancel 上下文，run 与心跳退出
    E->>C: Stop(5s)：Windows taskkill /T，失败再 /F<br/>Linux 进程组 SIGTERM，3s 后 SIGKILL
    C-->>E: 进程已退出
    Note over E: corePID 清零，关闭 log/client.log，停面板监听
  else 更新交接（Linux）
    E->>M: 包装到 update.tar.zst，停资源后正常退出
    Note over L: 子进程退出 → 循环回到开头 → apply_pending()
  else 更新交接（Windows）
    E->>L: 启动器已带 --file --wait-for 拉起
    Note over E: 回调 quit()，客户端退出
    Note over L: 等旧进程退出后清空候选槽、解包、拉起新版本
  end
  Note over M: 释放 instance.lock
```

![应用退出时序](./flows/05-shutdown.svg)

## 6. 心跳与 401

30s 一次 `POST /api/v1/heartbeat {deviceId, timestamp, nonce, platform, signature, configHash, appVersion, credentialId}`。`credentialId` 报本地正在用的那张：没有凭据报 `none`，正式节点报空串。

401 不「一律清身份」（服务端只在删除身份时回 `revoked`）：

- `code=revoked`：清本地 `device_id`、停 core、回设置页。
- 其它 401（`unauthorized`）：服务端不区分时钟偏差、nonce 冲突与签名错误，客户端用响应的 `Date` 头判断本机时间偏差。偏差超过 300 秒窗口时只降级并提示「请校准本机时间」，**不清身份**；没有明显偏差时，连续 3 次 401 才按身份失效处理。任何一次成功的心跳或连接都会清零计数。