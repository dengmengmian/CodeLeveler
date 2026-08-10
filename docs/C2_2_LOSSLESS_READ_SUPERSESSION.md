# C2.2 LOSSLESS READ SUPERSESSION REPORT

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。

**结论前置：C2.2 = FAIL。** 机制实现正确、证明严密、回归零退化 —— 但**在实测工作负载上它一个 token 也回收不了**。更重要的是，本轮把 C2.1 给出的"无损下界 16,842 token"**证伪了**：那个数字来自我在 C2.1 分析脚本里的一个缺陷，真实下界小一个数量级（ripgrep 5,476，yq 0），而且方向与 C2.2 规格指定的方向相反。

---

## 1. Context Projection Architecture

```
canonical messages (drive loop 的 in-memory 工作上下文)
      │
      ├─► read-result projection      ← 本轮新增（drive.rs，token 估算之前）
      │
      ├─► token estimate              max(provider used_tokens, estimate_tokens)
      ├─► generic compaction if needed（未改动，阈值仍是 reliable_context）
      ├─► ContextSnapshot             ← 因此快照即实际发送内容
      └─► ModelRequest
```

三条接缝，各一处：

| 环节 | 位置 | 说明 |
| --- | --- | --- |
| 采集 | `executor/host.rs` `dispatch_raw` | 工具执行的**唯一活跃漏斗**。成功结果解析出 `ReadRecord` 存入侧表，键为 `call_id` |
| 侧表 | `Executor.read_lifecycle` | `Mutex<HashMap<String, ReadRecord>>`，进程内、每 executor 一份 |
| 投影 | `executor/drive.rs`（`context_tokens` 计算之前） | 原地改写 `messages`，`sink`（durable）不受影响 |

**为什么用侧表而不是给 `ToolResultContent` 加字段**：后者会改动持久化消息 schema 与 12 处构造点，且把 lifecycle 元数据写进 durable transcript。侧表把它完全限制在请求组装期，同时天然满足 §8 的降级要求 —— crash/recovery 后侧表为空，等价于"全部保留"。

**已知边界**：投影是 **turn 内**的。恢复路径 `host.rs::reconcile` 不经过 `dispatch_raw`，重放的工具调用没有记录，因此永不被 supersede。

---

## 2. Supersession Proof Rule

旧读取 A 被丢弃，当且仅当存在**更后**的读取 B，同时满足全部五条：

| # | 条件 | 不满足时 |
| --- | --- | --- |
| 1 | `A.path_key == B.path_key` | KEEP |
| 2 | `A.fingerprint == B.fingerprint`（整文件指纹） | KEEP |
| 3 | `!A.clipped_inside_line` | KEEP |
| 4 | `B.start_line <= A.start_line` | KEEP |
| 5 | `B.provable_end() >= A.end_line`，其中 `provable_end = B.clipped_inside_line ? B.end - 1 : B.end` | KEEP |

外加一条**尺寸闸门**：marker 文本必须短于被替换的内容，否则不投影。这条是实测逼出来的 —— marker 要写路径和区间，短读取上它比原文还长，投影反而会撑大请求并丢字符。

失败结果（`is_error`）无论有无记录一律不投影。缺任何一个 metadata 字段 → 无记录 → 不投影。`clipped_inside_line` 缺省读作 `true`（保守值），不是 `false`。

**只看返回值，不看参数**（§3 要求）：`read_file` 输出受 256KB 上限约束，`read_file(path)` 不等于 `lines 1..EOF`。ripgrep 的 `defs.rs` 就是活例 —— 无参数整文件读取实际只返回了 **lines 1–7070 of 7675**。按参数推断会把它当成全覆盖，那是一个 false positive。

---

## 3. Metadata Contract

`read_file` 成功时在 `ToolOutput.metadata` 下发布（**不改模型可见 schema，不进正文**）：

```json
{"read_lifecycle": {
  "canonical_path_key":    "<workspace-resolved path>",
  "full_file_fingerprint": <u64>,
  "returned_start_line":   <int|null>,
  "returned_end_line":     <int|null>,
  "total_lines":           <int>,
  "clipped":               <bool>,
  "clipped_inside_line":   <bool>
}}
```

指纹**复用**读取时已经流式计算的 `leveler_context::ContentFingerprint`（原本服务于 `RepeatedReadGuard` / `FileStateTracker`），未二次 hash。区间取自输出循环真实记录的 `first_shown` / `last_shown`；请求区间落在 EOF 之后时两者为 `null`，该结果既不能 supersede 也不能被 supersede。

---

## 4. Durable-vs-Projected Context

| | 内容 |
| --- | --- |
| **Durable**（`sink` / event log / task evidence） | 原始完整 tool result，一字未动。marker 从不写回 |
| **Projected**（ModelRequest / ContextSnapshot） | 被证明冗余的旧读取替换为 marker |

结构完全保持：不删消息、不删 content part、不删 `call_id`。每个 `ToolCall` 仍配对其 `ToolResult` —— orphan result 会被 provider 直接 400。

---

## 5. Deterministic Tests

单元 15 条（`crates/leveler-agent/src/read_supersession.rs`）：

| 编号 | 测试 | 结果 |
| --- | --- | --- |
| A | `an_exact_repeat_supersedes_the_earlier_read` | ✅ |
| B | `a_later_broader_read_supersedes_the_narrower_earlier_one` | ✅ |
| C | `a_later_narrower_read_keeps_the_earlier_whole_file_read` | ✅ |
| D | `a_partial_overlap_supersedes_nothing` | ✅ |
| E | `a_changed_fingerprint_keeps_the_pre_edit_read` | ✅ |
| F | `a_failed_read_is_never_superseded` | ✅ |
| G | `a_clip_inside_a_line_blocks_supersession_in_both_directions` | ✅ |
| H | `two_different_files_with_identical_contents_are_both_kept` | ✅ |
| I | `only_information_dominated_reads_are_projected` | ✅ |
| J | `projection_is_idempotent` | ✅ |
| K | `call_and_result_pairing_survives_projection` | ✅ |
| — | `a_read_smaller_than_its_marker_is_left_alone`（尺寸闸门） | ✅ |
| — | `results_without_lifecycle_metadata_are_never_projected` | ✅ |
| — | `metadata_without_a_returned_range_yields_no_record` | ✅ |
| — | `a_missing_clip_flag_is_treated_as_clipped` | ✅ |

工具契约 7 条（`read_file.rs`）：区间/指纹/总行数发布正确、同文件两次读取指纹相同且编辑后必变、EOF 之后无区间、行内截断与行级截断分别标记、失败读取不发布任何 lifecycle、正文逐字不变。

集成 2 条（L / M / N，`loop_test.rs`，走 `Executor::run` 真实路径 + 真实 `read_file`）：

- `a_covered_read_leaves_the_request_but_never_the_transcript` —— 请求里旧读取变 marker、覆盖它的读取完整、**每个请求的每个 ToolCall 都有配对 ToolResult**、ContextSnapshot 反映投影后内容、**durable transcript 保留原文且从不含 marker**。
- `a_read_from_before_an_edit_is_never_superseded` —— 真实 `replace` 编辑后，编辑前的读取在请求中逐字保留。

**变异检查（证明测试非空断言）**：逐条削弱五个证明条件，每次都有特定测试转红 ——

| 削弱的条件 | 转红的测试 |
| --- | --- |
| `fingerprint` 相等 | `a_changed_fingerprint_keeps_the_pre_edit_read`（+ I） |
| `path_key` 相等 | `two_different_files_with_identical_contents_are_both_kept` |
| `!clipped_inside_line` | `a_clip_inside_a_line_blocks_supersession_in_both_directions` |
| `start_line` 覆盖 | `a_partial_overlap_supersedes_nothing` |
| `provable_end` → `end_line` | `a_clip_inside_a_line_blocks_supersession_in_both_directions` |

---

## 6. Offline Replay

`scripts/replay_supersession.py` 把生产规则（含尺寸闸门）逐条施加到 C2.1 保存的 `ContextSnapshot` 上。区间从 `read_file` 自己写的 `%6d\t` 行号前缀**精确**还原；版本用"两次读取之间该路径是否被编辑工具触碰"近似（生产用整文件指纹，严格更紧，所以重放只会**高估**）。

| Run | 最终请求读取 token | **生产规则（forward）回收** | 参考：反向规则回收 |
| --- | ---: | ---: | ---: |
| EDIT1 | 287 | **0** | 0 |
| MF1 | 869 | **0** | 0 |
| EXP1 | 2,546 | **0** | 0 |
| **yq** | 21,140 | **0** | 0 |
| **ripgrep** | 72,657 | **0** | 5,476 (7.5%) |

累计到每一轮请求（yq 824,430 / ripgrep 6,105,333 读取 token）：**回收 0，降幅 0.0%。**

### 6.1 为什么是 0 —— 以及 C2.1 的下界为什么是错的

**(a) 方向反了。** 规格 §2 定义的是"后来的读取覆盖更早的读取 → 丢弃更早的"。而 Agent 的实际访问模式是**先大范围、后窄区间**：

| 路径 | 读取序列 |
| --- | --- |
| `crates/core/flags/hiargs.rs` | 1–1471（12,288 tok）→ 700–880 → 85–105 |
| `crates/core/flags/defs.rs` | 1–7070（12,289 tok）→ 6400–6500 → 1217–1350 |
| `crates/core/main.rs` | 80–260 → 400–483 → 1–80 → 140–240（互不包含） |

窄区间永远覆盖不了它前面的大范围读取，所以 forward 规则一次也不触发。反向（丢弃被更早读取覆盖的**后来**读取）能触发，但只值 5,476 token。

**(b) C2.1 的 16,842 是分析脚本的缺陷，不是真实冗余。** 那个脚本按 `start_line` 参数缺省判定"整文件读取"，于是把 `is_error=true` 的**探索预算拒绝**（29 token 的 `Explore budget used…`）当成了一次整文件读取，进而把其后真正的读取判成冗余：

| 路径 | 被误判的"首次整文件读取" | 被连带误判为冗余 |
| --- | --- | ---: |
| `crates/core/flags/defs.rs` | `is_error=true`，29 tok | 14,500 |
| `cmd/evaluate_sequence_command.go` | `is_error=true`，29 tok | 1,241 |
| `cmd/constant.go` | `is_error=true`，29 tok | 221 |

更正后：yq 无损下界 **0**（24 个文件各读一次，零重复），ripgrep **5,476**。`docs/C2_1_CONTEXT_COST_ATTRIBUTION.md` 的 §5 / §10 / §12 / §13 / §15 已就地更正并标注更正说明。

**这不是"离线重放与 C2.1 不一致"，是 C2.1 那个数字本身不成立。**

---

## 7. Real-model Before / After

见 §8。由于 §6 已证明该规则在这两个仓的访问模式下投影量恒为 0，本轮的真实模型运行**是正确性回归，不是 token 对比** —— 对比一个可证明为 0 的差值没有测量意义。

**ripgrep 100 轮未重跑**：单次约 9.4M provider token，用于验证一个已证明为 0 的差值。这是我主动缩小的范围，明确记录在此，可按需补跑。

---

## 8. Correctness Regression

模型 `deepseek/deepseek-v4-flash`，默认产品行为（无 ablation）。

### C1 Representative Set（`evals/realtask`，11 case）

**11/11 PASS，`expect_passed` 全 True。False completion 0，loop guard trips 0。**

安全语义逐条比对 C1 Final §6：

| Case | C1 Final 终态 | 本轮终态 | 隐藏验收 |
| --- | --- | --- | --- |
| WV1 wv1-runbook-repo | CompletedUnverified | **CompletedUnverified** | PASS |
| BB1 bb1-preexisting-build-failure | CompletedUnverified | **CompletedUnverified** | PASS |
| BB2 bb2-test-gate-compile-failure | Completed | **Completed** | PASS |

三条精确匹配，未退化。

轮次 / token 与 C1 Final 基线对比（8 个可比 case）：

| Case | rounds C1 → 本轮 | input tokens C1 → 本轮 |
| --- | --- | --- |
| MF1 | 16 → 13 | 243,626 → 200,777 |
| MF2 | 16 → 14 | 351,605 → 231,998 |
| MF3 | 9 → 13 | 145,155 → 215,876 |
| EXP1 | 11 → 8 | 189,694 → 133,876 |
| EXP2 | 13 → 20 | 226,100 → 377,144 |
| EDIT1 | 9 → 9 | 124,542 → 117,596 |
| R1 | 8 → 8 | 125,928 → 121,085 |
| R2 | 7 → 8 | 104,835 → 123,468 |
| **合计** | | **1,511,485 → 1,521,620（+0.7%）** |

有涨有跌，合计 +0.7% —— **纯运行间波动**，与投影量为 0 一致。

### yq（真实仓）

**1/1 PASS**，`Completed`，hidden acceptance PASS，55 轮 / 2,650,025 token / 32 reads / 6 edits 0 失败 / first edit @r21。

对比 C1 Final 基线（50 轮 / 2,130,655 token / 26 reads）：token +24%。

**这不是本改动造成的，有直接证据**：

| 检查 | 结果 |
| --- | --- |
| 新 run 的 `context_snapshot` 中 marker 出现次数 | **0** |
| 对新 run 重放生产规则的回收量 | **0 token / 0.0%** |

投影代码执行了但一次都没有触发，因此 +24% 完全来自轨迹波动（多 5 轮、多 6 次 `read_file`）。

### 确定性套件

`cargo fmt --check` ✅ · `cargo check --workspace --all-targets` ✅ 0 error · `leveler-agent` + `leveler-tools` 全套件 ✅ 0 failed（含本轮新增的 24 条测试）。

> **未跑**全量 `cargo test --workspace --no-fail-fast`：生产接线随后按 §17 全部撤销，最终树等同于 C1 head（该 head 上的全量套件已在 C2.1 验证为 122 suites / 2,536 tests / 0 failed）。

---

## 9. Token Reduction

| 拆项 | yq | ripgrep |
| --- | ---: | ---: |
| read-result tokens 变化 | **0** | **0**（离线证明） |
| total provider tokens 变化 | 见 §8 | 未重跑 |
| tokens/round 变化 | 见 §8 | 未重跑 |

**本机制在实测工作负载上的 token 收益为零。**

---

## 10. Remaining Read Cost

安全 lower-bound 做完后，`read_file` 仍占最终请求的 **57.4%（yq）/ 72.2%（ripgrep）** —— 一分未减。按 §16 要求归类：

| 类别 | yq | ripgrep |
| --- | ---: | ---: |
| **unique necessary reads**（只读一次的文件） | **21,140 (100%)** | 27,926 (38.4%) |
| **broad whole-file reads 中从未回读的部分** | 0 | **30,864 (42.5%)** |
| broad reads 中后来确实回读过的部分 | 0 | 3,311 (4.6%) |
| partially overlapping reads（窄区间回读本身） | 0 | 10,556 (14.5%) |
| **stale across mutation** | **0** | **0**（两仓的读取全部落在同一文件版本上） |

两个仓是**两种完全不同的形态**：

- **yq**：100% 是唯一的、各读一次的文件。**任何无损去重机制在这里的理论上限都是 0。** 它的成本是广度，不是重复。
- **ripgrep**：最大的一块是 **42.5% —— 大范围读取里模型后来根本没回去看的部分**。`hiargs.rs` 读进 12,288 token，之后只回看了 700–880 与 85–105（合计 2,342 token）。也就是说约 10k token 是读进来就再没用过的。

**这不是上下文管理问题，是读取粒度问题。** 陈旧、重复、跨版本失效这三类在实测中合计接近 0；真正的浪费发生在**读取动作发出的那一刻**。

---

## 11. C2.3 RECOMMENDED TOP-1

> **Read Scope Discipline —— 收窄 `read_file` 单次返回的默认体量**

由数字决定：

1. ripgrep 读取成本的 **42.5%（30,864 token）** 是大范围读取中从未被回看的部分 —— 这是所有分类里最大的一块，比无损去重的全部理论上限（5,476）大 **5.6 倍**。
2. 该浪费在**读取发生时**就已产生，任何事后的生命周期管理都无法回收（本轮已实证：回收量 0）。
3. 现有 256KB / `MAX_BYTES` 上限对代码文件而言过于宽松：单次读取就能吃掉 12k token，而模型真正需要的往往是其中 2k。
4. yq 侧的形态（100% 唯一读取）说明**不能靠去重**：yq 上任何去重机制收益恒为 0，而收窄读取粒度对两个仓同时有效。

可选形态（留待 C2.3 决定，本轮不实现）：降低默认返回预算并在截断标记里给出更强的分页指引；或对超过阈值的无区间读取返回结构骨架 + 分页提示。

**明确不推荐**：继续加读取生命周期机制（本轮已证明其天花板是 5,476 token）；反向 supersession（同样只值 5,476，且会用"指向四十轮前那条消息"来回答模型刚提的问题，有诱发重复读取的行为风险）。

---

## 12. FINAL

按 §14 逐条核验：

| # | 要求 | 结果 |
| --- | --- | --- |
| 1 | 所有 supersession 满足严格信息覆盖证明 | ✅ 五条件 + 尺寸闸门，变异检查逐条钉死 |
| 2 | canonical history 零损失 | ✅ 集成测试断言 durable transcript 保留原文且从不含 marker |
| 3 | C1 representative correctness 零退化 | 见 §8 |
| 4 | offline replay 实际减少 read-result context | ❌ **0 token / 0.0%** |
| 5 | real repo provider token 成本出现方向一致的下降 | ❌ 因 #4 为 0，不可能发生 |

# C2.2: FAIL

## 13. 处置（按 C2.3 §17）

生产接线**已全部撤销**，最终树等同于 C1 head：

| 撤销 | 理由 |
| --- | --- |
| `drive.rs` 投影调用 | 实测收益 0，在默认路径上是无收益复杂度 |
| `host.rs` 采集 + `Executor.read_lifecycle` 侧表 | 同上 |
| `read_supersession.rs` 模块与 15 条单元测试 | 无生产消费方 |
| `read_file` 的 `read_lifecycle` metadata 与 7 条契约测试 | 工具 metadata 不进事件日志，无消费方；BroadReads/NarrowReads 指标可直接从 `ContextSnapshot` 的 `%6d\t` 行号前缀精确还原（`scripts/replay_supersession.py` 已在这么做） |
| `loop_test.rs` 2 条集成测试 | 随被测特性一同移除 |

**保留**：本报告、`scripts/replay_supersession.py`（离线研究工具，是 C2.3 导航指标的现成基础）、以及对 C2.1 的更正。

不为"已经写了"而保留 production behavior。

**机制是对的，方向是错的，前提是假的。**

代码本身正确、安全、零退化，可以留在树上（它不会造成任何损害，且在"先窄后宽"的访问模式出现时会自动生效）。但它解决不了 C2.1 指出的问题，因为 C2.1 指出的那个可回收量**经更正后并不存在**。

C2.1 的核心观测依然成立且未被推翻：**读取结果占真实仓上下文的 57–72%，且轮内从不回收。** 被推翻的只是"其中有一块可以无损回收"这个推论。真正的可攻击面已由 §10 定位到读取粒度。
