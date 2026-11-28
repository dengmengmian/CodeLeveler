# R6 架构与发布身份

日期：2026-11-28
基线：`de1c8c36`（main，工作树含 R6 改动）
上一正式版：`v1.0.29`（tag `v1.0.29`，源码 `f972f9f6`）

## 1. 产品层次（未重建第二套权威）

| 层 | 职责 | 本轮是否改变 |
| --- | --- | --- |
| Model / Provider | 协议适配、凭据解析、流式调用 | 否（仅 Debug 脱敏） |
| Runtime / Engine | 会话、turn、Goal、工具执行、审批、持久化 | 否（新增 media sweep 调用点与 budget 透传） |
| Memory | capture → Dream → topic，Markdown + SQLite | 否 |
| Media | 内容寻址图片 store | 是：原子/私有写入、引用感知清扫 |
| Desktop Bridge | Electron main ↔ Runtime 的类型化 IPC | 是：打包后可解析内置 runtime；未改权限判定 |
| Desktop Renderer | 展示与用户输入；不拥有执行语义 | 否（仅新增预算停止文案来源） |
| Release | CLI 归档 + GitHub Release | 否（Desktop 打包是本轮新增的独立脚本） |

Desktop 不维护第二套 Agent Harness、Goal 状态机、记忆系统、MCP Client、Skills 引擎、
Review 引擎或工具执行存储。所有完成/继续/审批判断仍来自 Runtime 的结构化状态、事件和
持久化事实；模型自由文本不参与控制流。

## 2. 打包后的进程与文件布局

```
CodeLeveler.app/
  Contents/MacOS/CodeLeveler            Electron
  Contents/Resources/app/               Desktop 源码 + 生产 node_modules
  Contents/Resources/runtime/leveler    与本次构建一致的 Rust Runtime
  Contents/packages/conversation-presentation/   renderer 共享投影
```

- `main.cjs` 的 `runtimeBinary()`：
  1. `LEVELER_BINARY`（显式覆盖；测试/开发）；
  2. `app.isPackaged` → `process.resourcesPath/runtime/leveler`；
  3. 源码运行 → `target/debug/leveler`。
  不再从 PATH 猜测，也不会误连 Homebrew 旧构建。
- 打包脚本把 Cargo 版本写入 `CFBundleShortVersionString`，并在 manifest 记录 Runtime
  `--version`、SHA-256、git HEAD 与 dirty 数。
- Desktop `package.json` 的 `0.1.0` 不再是产物版本；打包时显式覆盖为产品版本。
- Runtime 与 Desktop 仍各自校验 daemon 的 `BuildIdentity`/协议版本；内置二进制与
  Desktop 由同一构建脚本组合，避免“新版 Desktop + 旧版 Runtime”。

## 3. 身份与版本事实

| 项 | 值 |
| --- | --- |
| 当前源码 HEAD | `8bde10ca66e6`（候选构建；文档提交 `a6329dbd` 在其后） |
| Cargo workspace | `1.0.29` |
| 上一正式 tag | `v1.0.29` = `f972f9f6` |
| 距上一 tag | 96 个提交（23 feat / 27 fix / 27 test / 其余 docs/chore/style/merge） |
| 本轮候选产物 | `target/desktop-rc/`（ad-hoc 本地 RC，未签名/未公证，不是正式发布） |
| Windows Runtime 产物 | 仅 CI 构建的 CLI zip；**没有** Desktop 安装器或真机验收 |

版本决策：本轮**不**改 Cargo 版本、不 tag、不 release。RC 产物信息以
`HEAD + dirty + 构建 manifest` 标识；正式发布事务仍需按 `./dev publish` 决定下一版本号。
因此产物版本字段仍是 `1.0.29`，不得把该 RC 描述为已发布的 1.0.29 或 1.0.30。

## 4. 架构风险

| 风险 | 影响 |
| --- | --- |
| 打包脚本是新增的本地工具，不是已接入 CI 的发布流水线 | macOS 产物可复现性只在当前机器验证 |
| Windows 没有 Desktop 构建/打包/验收 job | Windows 正式支持 **BLOCKED** |
| 自动更新无 Desktop 实现 | 更新/回滚门禁 **BLOCKED** |
| 图片引用无持久索引 | 启动时从 transcript/events 派生，成本随图片历史增长 |
| `TaskFinished` 不存结构化 budget | replay 依赖机器 detail 解析；live 使用结构化值 |
