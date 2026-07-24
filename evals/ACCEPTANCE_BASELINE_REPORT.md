# Agent 验收基线报告（第一轮：只记录）

> 结论先行：用**真实 Provider（deepseek-v4-pro）+ 真实 agent 路径（leveler eval，非内部 API）**
> 跑现有 eval 套件。smoke 基线 100% 干净、0 假完成、0 空转、Quality Score 100。真正暴露问题的
> daily（core+hard+recovery，28 例）运行中，结果与问题清单见下方（完成后填）。

## Phase 1 · 环境（Baseline）

| 项 | 值 |
|----|----|
| 版本 commit | `89f2bba`（含 eval 门禁 a594cd7 + L1/L2 可观测性）|
| 系统 | macOS (darwin 25.5.0) |
| 模型 | deepseek/deepseek-v4-pro |
| Provider | deepseek，base_url `https://taotoken.net/api/v1`（非 localhost、非 mock）|
| 启动方式 | `./target/debug/leveler eval quick\|daily`（orchestrated 真实 agent 路径）|
| 禁项遵循 | 未直调内部 Agent API；未 mock |

## Phase 4/5 · smoke 基线（真实，已完成）

```
tier: quick (3 cases across evals/smoke), mode: orchestrated
✓ go-triple        steps=11 tokens=141186/1457 latency=59337ms  Verified
✓ rust-first-even  steps=12 tokens=166242/2741 latency=107343ms Verified
✓ rust-mul         steps=7  tokens=86660/957   latency=97712ms  Verified
→ 3/3 passed (100% completion, 100% completion accuracy), avg 10 steps
→ avg 10 tool calls · loop rate 0% · validation rate 100%
★ Agent Quality Score: 100/100
```

| 指标 | 值 | 判定 |
|------|----|----|
| success rate | 100% (3/3) | ✅ |
| false completion rate | 0% | ✅ |
| loop count | 0 | ✅ |
| tool efficiency | avg 10 calls，0 循环浪费 | ✅ |
| verification rate | 100% | ✅ |

**观察（喂给 runtime 可观测性）**：单例延迟 **59–107s**，几乎全在模型推理。这正是"等待模型"黑盒的
来源，实证了本轮 L1/L2 的必要性——已提交 89f2bba（命令心跳 + 假 blocked 用词）。

## Phase 4/5 · daily 验收（core+hard+recovery，28 例）— 运行中

> 真实模型跑，约 40 分钟。逐例失败经 Monitor 捕获。完成后汇总总指标。

### 已捕获失败（初步签名，根因待单例复现）

| case | completed | expect_passed | termination | 首因 | 解读 |
|------|-----------|---------------|-------------|------|------|
| go-batch-boundaries | ❌ | ✅ (go test exit 0) | incomplete | runtime | **假阴性**：代码对但 agent 未干净完成 |
| go-copy-map | ❌ | ✅ | incomplete | runtime | 同上 |
| go-normalize-email | ❌ | ✅ | failed | runtime | 代码对但 agent 运行时错误终止 |

### 根因（已单例复现，证据驱动）

停 daily 后单跑 `rust-dedup-stable`：**Verified / completed / 100%**——同案例同模型，daily 失败、
隔离通过。逐项排除后锁定：

| 假设 | 证据 | 结论 |
|------|------|------|
| GOCACHE 沙箱挡 go | `host_cache.rs:528` 已重映射 GOCACHE/GOPATH | ❌ 排除 |
| Go 工具链坏 | smoke go-triple `verification_ran=True` 真跑 go 通过 | ❌ 排除 |
| provider 限流 | daily_run.log 无 429/timeout/stream 错误；失败与通过**交替**非 burst | ❌ 排除 |
| **完成可靠性变异** | daily 里 dedup `verification_ran=False`；隔离 `=True`→Verified | ✅ **根因** |

**根因**：agent **非确定地跳过运行验证命令**就收尾。跳过时，完成证据门（正确地）因无通过证据
拒绝确认 → 判 Incomplete，尽管代码通过独立 `expect`。这是 **false negative**（做对却报未完成），
与 false completion 相反，比它更隐蔽（用户以为白干了）。7 例里命中 4 例（2 例 verification_ran=False
直接触发；2 例 verification_ran=True 但迭代中途 incomplete/failed，属同族的完成收敛问题）。

### 性质与处置（关键诚实性）

- **不是确定性 bug**：隔离能过，无法用确定性测试锁住 → 按调试纪律**不做猜测式补丁**。
- **是非确定性可靠性变异**：真实用户会间歇遇到"明明做完了却说没完成"。
- **变异率已量化**（4 例 × 3 次，真实模型）：**7/11 = 63% 假阴性**，且**所有失败都是假阴性**
  （other_fail=0，每个失败的 expect_passed 都是 True）：

  | case | runs | false_neg |
  |------|------|-----------|
  | go-batch-boundaries | 3 | 1 (33%) |
  | go-copy-map | 3 | 2 (67%) |
  | go-normalize-email | 3 | 2 (67%) |
  | rust-dedup-stable | 2 | 2 (100%) |

  63% 已非"低频抖动"而是**严重可靠性缺陷**（真实日用中过六成把做对的任务报未完成）→ 定级 **P1（近 P0）**。
  smoke 3/3、rust-cache/go-context/go-json 通过，说明地基没坏，问题集中在**完成收敛**这一环。
- **正确下一步**（Phase 6 第二轮，进行中）：抓 debug 失败实例定确切门决策
  （agent 跳过验证 vs 门跑了验证却拒绝确认），再做最小加固并用重复率验证修复效果。

### 已捕获指标（daily 前 7 例，因根因确认已停）

| 指标 | 值 | 说明 |
|------|-----|------|
| 完成率 | 3/7 (43%) | 但 4 个失败全是**假阴性**（代码实际正确）|
| **真实能力率** | 7/7 (100%) | 按独立 `expect` 判定，代码全对 |
| false completion | 0% | 无错误宣布完成 |
| loop rate | 0% | 无空转 |

## Phase 2 · TUI 稳定性（既有测试层，当前绿）

- `tui_path_soak`（short/long/goal，≥80 轮，hang=硬失败）：**通过**。
- `tui_session_e2e`（TestBackend 断屏，多轮/工具/取消）：**通过**。
- 说明：交互式 Ctrl+C/滚动/输出丢失由 PTY 测试层覆盖，非 eval case。

## Phase 3 · 权限（执行层，非 UI）

- `scenarios/permission/protect-secrets`：金丝雀密钥保护，`expect` 已红/绿验证（GREEN 需真实模型跑，
  纳入 release 层）。执行层沙箱/敏感路径校验见 [[security-semantics-fixes]]。

## 问题清单（P0/P1/P2）

> 第一轮只记录，不修。daily 跑完后按首因归因填。当前 smoke 层无问题。

| 级别 | 现象 | case | 首因 | 备注 |
|------|------|------|------|------|
| — | smoke 层无问题 | — | — | 待 daily 数据 |
