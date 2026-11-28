# macOS 产物与验收（R6-C2/R6-D）

日期：2026-11-28
结论：**本地产物可构建；正式签名/公证 BLOCKED；安装版真实 Dogfood 见下。**

## 1. 构建环境

| 项 | 值 |
| --- | --- |
| 主机 | MacBook（Apple Silicon）/ macOS 26.6.2 / Darwin 25.6.0 / arm64 |
| Electron | 44.5.1 |
| 打包工具 | `@electron/packager` 20.3.1（新增 devDependency） |
| Runtime | `target/release/leveler`，Cargo 1.0.29，源码 HEAD 见 manifest |
| 签名身份 | `security find-identity -v -p codesigning` → **0 valid identities** |
| 公证 | 无 Apple ID/Team/notary 凭据 |
| Intel | 未构建/未验证（本轮只产出当前 arm64） |

## 2. 产物

由 `apps/leveler-desktop/scripts/package-desktop.mjs` 生成：

- `CodeLeveler.app`：Electron + `Contents/Resources/runtime/leveler` + renderer 资源；
- `.zip`（ditto）；
- `.dmg`（hdiutil UDZO）；
- `manifest.json`：版本、源码 HEAD/dirty、Runtime 版本/SHA-256、bundle 检查、签名/Gatekeeper 状态、产物 SHA-256。

| 产物 | SHA-256 |
| --- | --- |
| zip | `6f626ab582aa8a5e95b59900c5e077fc459701a08a3efa9b6adf80656f0aa2b6` |
| dmg | `d7a5509c67300bb1989926a4358f5917800549b31cf498ca29f9b27bb5b919c7` |
| Runtime | `e4ee9ab45af7da6172ee351c5f8212a7751d4346bac1576b7a34e6b1925c16c9` |
| Bundle CDHash | `44272c200982d0818778ca6a06868825a1626645`（adhoc） |

完整清单见 `artifacts-manifest.json`。

## 3. 已验证

- 打包脚本检查 bundle 完整性：Runtime 存在且与输入 SHA 一致、`conversation-presentation`
  存在、`marked`/`xterm` 存在、Electron Framework 存在；
- Info.plist 的 `CFBundleShortVersionString` 与请求版本一致；
- 安装到用户级 `~/Applications` 后，Desktop 启动的 Runtime 路径必须是
  `.../Contents/Resources/runtime/leveler`（真实进程命令核对）；
- 安装版真实 Provider/Dogfood 结果见 `installed-product-dogfood.md`。

## 4. 未验证 / BLOCKED

| 项 | 状态 |
| --- | --- |
| Developer ID 签名 | **BLOCKED** |
| Hardened Runtime / entitlements | **NOT IMPLEMENTED**（未签名产物无 entitlements） |
| 公证 / Staple | **BLOCKED** |
| Gatekeeper 正式分发评估 | **NOT ASSESSED**（未签名本地产物不适用正式结论） |
| Intel 构建 | **NOT VERIFIED** |
| 卸载与数据保留 | 待安装版验收后记录 |
| 自动更新 | **BLOCKED**（Desktop 无更新器） |
| 首次运行/升级自旧版 | 无旧 Desktop 版本；见 `upgrade-migration.md` |

不得把本地产物描述为“已签名/已公证/可直接分发”。正式分发前必须用真实 Apple 证书重做
签名、Hardened Runtime、公证、staple 与 Gatekeeper 验证。
