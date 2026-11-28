# Desktop 打包（R6-C1/C2/C3）

日期：2026-11-28
结论：**macOS 本地 RC 产物可构建；Windows 产物 BLOCKED；正式签名/公证 BLOCKED。**

## 1. 打包体系

新增 `apps/leveler-desktop/scripts/package-desktop.mjs`，使用官方 `@electron/packager`
（`^20.3.1`）把现有 Desktop 源码与 release `leveler` 组合成自包含 `.app`。

命令：

```bash
cargo build --release -p leveler-cli --bin leveler
cd apps/leveler-desktop
node scripts/package-desktop.mjs --binary ../../target/release/leveler --out ../../target/desktop-rc
```

产物：

```
target/desktop-rc/
  CodeLeveler-darwin-arm64/CodeLeveler.app
  CodeLeveler-1.0.29-macOS-arm64.zip
  CodeLeveler-1.0.29-macOS-arm64.dmg
  manifest.json
```

## 2. Bundle 结构

- `Contents/Resources/app`：Desktop 源码 + 生产 `node_modules`（packager prune）；
- `Contents/Resources/runtime/leveler`：与本次构建一致的 Runtime（SHA 校验）；
- `Contents/packages/conversation-presentation`：renderer 共享投影；
- Info.plist：`com.codeleveler.desktop`、版本 = Cargo 版本、可执行名 `CodeLeveler`。

`main.cjs` 的 `runtimeBinary()` 优先显式 `LEVELER_BINARY`，打包后使用
`process.resourcesPath/runtime/leveler`，源码运行才回退 `target/debug/leveler`。因此
Desktop 不会误连 Homebrew 旧构建，也不会从 PATH 猜测。

## 3. 产物身份

manifest 记录：

- 源码 HEAD 与 dirty 文件数；
- Runtime 路径、`--version`、SHA-256、dirty；
- bundle 完整性检查（runtime/conversation/marked/xterm/Electron Framework）；
- Info.plist 版本/ID/可执行名；
- `codesign -dv` 结果与 `spctl --assess` 结果；
- `.zip`/`.dmg` SHA-256。

## 4. 非生产级 / 缺口

| 项 | 状态 |
| --- | --- |
| CI 接入 | 未接入；只有本地脚本 |
| 签名 | **AD_HOC**（0 个 Developer ID；本地 ad-hoc） |
| Hardened Runtime / entitlements | 未实现 |
| 公证/Staple | **BLOCKED** |
| Universal binary | 未构建；只有 arm64 |
| Intel | 未验证 |
| Windows NSIS/MSIX | **BLOCKED**（无工具链/无真机） |
| 自动更新 | 未实现 |
| `asar` | `false`（便于协议路径；正式发布可评估开启） |
| 安装到 `/Applications` | 本轮只安装到用户级 `~/Applications`，不覆盖正式安装 |

## 5. 结论

这是**未签名的本地 RC 打包路径**，足以验证“安装产物确实包含匹配 Runtime 并能执行真实
Agent 任务”，但不构成正式分发能力。正式发布必须补齐签名、公证、Windows 安装器、更新信任链
与 CI 产物。
