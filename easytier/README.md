# easytier-mini 的补丁

Windows 客户端用的精简内核来自上游 `easytier-contrib/easytier-mini`：体积约为完整版
`easytier-core` 的一半，常驻内存也小一档。上游原样的 mini 用不了——它手写的参数解析既不读
`ET_CREDENTIAL` / `ET_LOCAL_PRIVATE_KEY`，也不把 `--secure-mode` 落到静态配置上，凭据节点
会被当成「正式节点 + 空口令」拒掉。

```
easytier/
  upstream.txt   钉住的上游仓库与提交
  mini.patch     对上游的改动（mini 的身份处理 + 主程序依赖）
```

## 构建

源码树按需拉取，落在仓库外的 `.tmp/easytier`，可以反复复用：

```pwsh
.\build\easytier.ps1      # 拉取 → 打补丁 → 编译 → 拷到 dist/runtime/windows-amd64
```

产物是 `dist/runtime/windows-amd64/easytier-core.exe`，客户端按这个名字找它。脚本自己会跑两条
自检：`--version` 报 `easytier-mini 2.7.1`，`--credential x` 必须被拒（补丁没生效时它会接受）。

## 升级上游

1. 改 `upstream.txt` 里的 `commit`。
2. 删掉旧源码树里的补丁（`git -C .tmp/easytier checkout -f`），再跑 `build/easytier.ps1`。
3. 冲突基本只会落在 `parse_args` 的参数循环与 `main` 的启动段两处。
4. 解完冲突后重生成补丁：

   ```pwsh
   git -C .tmp/easytier diff --output client/easytier/mini.patch <commit> -- easytier-contrib/easytier-mini/
   ```

## 约束

- **只有 Windows 用 mini**：它用 `ReadOnlyApiRpcServer`，按设计永远不是 EasyTier manager，
  签不出凭据。三个种子必须保留完整版 `easytier-core`，Linux 包也始终是完整版。
- 补丁是幂等的：脚本先用 `git apply --check` 试运行，已打过就跳过。
- 构建依赖 `PROTOC` / `VC_LTL` / `YY_THUNKS` 三个本地路径（Win7 兼容与静态 CRT），脚本会补
  默认值，缺了直接报错。
- **不打 `--locked`**：补丁给 mini 的 `Cargo.toml` 加了 `easytier-core` 依赖，锁文件本来就要更新。
