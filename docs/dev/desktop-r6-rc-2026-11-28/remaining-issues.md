# R6 剩余问题

日期：2026-11-28

## P0

未发现。没有可复现的数据丢失、凭据泄漏、权限绕过、任意路径读写或不可恢复更新损坏。
这是“本轮证据范围未发现”，不是对 Windows/未测路径的证明。

## P1（阻断正式发布）

| # | 问题 | 影响 | 关闭条件 |
| --- | --- | --- | --- |
| P1-1 | Windows Desktop 无真机验收 | 双平台正式支持不成立 | 真实 Windows 11 x64、普通用户、完整 B2/B3/B4/B5 矩阵 |
| P1-2 | 无 Windows Desktop 安装器 | 用户无法正式安装 | 与 macOS 同一打包体系产出签名安装器并验证安装/卸载/数据保留 |
| P1-3 | macOS 无 Developer ID 签名/公证 | 不能正式分发 | Apple 证书 + notary 凭据；`codesign --options runtime` + `notarytool` + staple + Gatekeeper 验证 |
| P1-4 | Desktop 自动更新/回滚不存在 | 无法安全升级 | 更新 manifest/哈希/签名验证、失败回滚、活跃 Session/Runtime 保护 |
| P1-5 | 上一正式版无 Desktop，正式升级无法执行 | C4 不成立 | 发布一个正式 Desktop 版本后，建立 vN→RC 升级矩阵 |
| P1-6 | Memory A4 全矩阵未实测 | 并发/竞争/超时语义有证据缺口 | 非空 `/flush`、Desktop+TUI 双写、并发 `/remember`/Dream、35 分钟边界、真实遗忘竞争 |
| P1-7 | Goal 资源治理产品面不完整 | 用户无法在 Desktop 设置/查看预算与提醒 | Desktop 预算编辑器、软阈值提醒、goal 级 usage、结构化终态持久化 |
| P1-8 | 24h Soak 未执行 | 长期稳定性无证据 | 在其它门禁通过后跑 24h；本轮不能引用 Soak 例外 |

## P2

- 媒体 store 启动扫描没有大库基准；无持久化 media refs 索引。
- Desktop 不显示 cost；`query_usage`/`query_observability` 未形成可见产品面。
- Memory 管理结果仍为 runtime 英文原文；页面键盘操作不完整。
- 真实崩溃窗口下的 `/recover` 破坏性分支未演练。
- Windows 输入法/拖放/剪贴板图片未验证。
- 打包脚本未接入 CI；`asar: false`；没有 universal binary / hardened runtime / entitlements。
- `leveler clean` 尚未把媒体孤儿作为独立可见类别；当前由 daemon 启动清扫。
- `docs/dev` 报告需要显式取消忽略后才能进入 Git；否则只能算本机证据。
- `background_crash_matrix::b9_start_identity_mismatch_never_signals_the_unrelated_live_process` 在全量并行负载下出现过 1 次“write reservation settled”失败；单独 10/10、整套 5/5、全量重跑 EXIT 0。根因未定位，归类 FLAKY_UNRESOLVED，不能算已修复。

## 不是问题但需说明

- 媒体软预算不会删除被历史引用的图片；超预算只警告。这是有意的历史真实性取舍。
- 未设置预算时不会硬停正常长任务；无进展有既有守卫，但没有“花费 vs 进展”启发式。
- v1.0.29 的 R5 chat+steer INCOMPLETE 无法复现；原始 Session 已删除，结论为 UNKNOWN，
  不能改写成“已修复”或“产品 BUG”。
