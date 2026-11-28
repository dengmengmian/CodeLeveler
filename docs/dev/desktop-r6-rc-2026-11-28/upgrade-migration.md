# 安装、升级与数据迁移（R6-C4/C5）

日期：2026-11-28
结论：**Desktop 无上一正式版可升级；CLI 旧数据兼容有证据；正式升级/回滚机制 BLOCKED。**

## 1. 上一正式版本

| 项 | 事实 |
| --- | --- |
| 最新正式 tag | `v1.0.29` |
| tag 源码 | `f972f9f613db82d1cf25a8666325ae1dc71bb8b4` |
| 发布记录 | `docs/releases/v1.0.29/release.json`（RELEASED，CONDITIONAL PASS） |
| 发布产物 | CLI 归档：macOS arm64/x86_64、Linux x86_64、Windows x86_64；无 Desktop |
| Desktop | v1.0.29 明确“桌面客户端需从源码构建，不包含在发布包中” |

所以：

- **没有“上一正式 Desktop 版本”**。本 RC 的 Desktop 安装包是新增产品表面，不能虚构旧版
  Desktop 的升级指标。
- CLI 的上一正式版是 v1.0.29，可作为数据格式兼容基线。

## 2. CLI 兼容证据

| 检查 | 结果 |
| --- | --- |
| 当前构建读取真实 HOME 的历史任务索引 | 325 tasks，source errors 0（R5/R6 dogfood） |
| 旧 Session 列表/快照 | 可读；A1 真实 Desktop 启动即读取 |
| 配置格式 | 旧 `config.toml` 被当前构建加载；A1 前后 SHA 一致 |
| Memory | R6-0 已接续现有 Markdown+SQLite store，无迁移删除 |
| 媒体 | 旧对象可读；新构建按引用清扫，未删除被引用对象 |

注意：这些是**非破坏性读取/兼容**证据，不是“从官方 v1.0.29 安装包执行正式升级”的证据。

## 3. 正式升级为什么 BLOCKED

1. 当前 RC 没有发布到任何更新源；`leveler update` 只认 GitHub stable release，无法升级到
   未发布的 RC。
2. 没有 macOS Developer ID / 公证 / Windows 代码签名，无法验证正式分发信任链。
3. Desktop 没有自动更新器；Settings 的“检查更新”只调用 CLI `update --check`，且明确
   不更新桌面客户端。
4. 没有更新 manifest / 哈希 / 签名验证 / 失败回滚的 Desktop 实现。

因此 C4（从上一正式版升级）与 C5（更新/回滚）在 Desktop 范围 **BLOCKED**；不能把
“手动替换二进制”写成自动升级 PASS。

## 4. 解除条件

- 发布一个带签名/公证的 CLI + Desktop RC 到隔离更新源。
- 用官方 v1.0.29 安装包建立真实配置、Session、Memory、Artifact/图片数据。
- 通过正式更新机制升级到 RC，验证 Provider、密钥、Session、Memory、MCP/Skills、定时任务、
  图片/Artifact、真实 Agent 任务，并记录前后哈希。
- 在受控 Profile 做一次失败更新/回滚演练，验证数据 Schema 兼容与可恢复性。
- Windows 用真实安装器重复同一矩阵。
