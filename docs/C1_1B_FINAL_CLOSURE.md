# C1.1b Final Closure — Eval Validity Gaps Closed

日期：2026-08-08。分支 `feat/coding-real-task-completion-c1`。模型 `deepseek/deepseek-v4-flash`。
本轮关闭 C1.1b 遗留的三个 **eval validity gap**，并据此重新划分 product failure / expected safety semantics / eval design failure。生产语义零改动（verifier、engine、agent、repair 次数均未动）。

## 1. WV1 — expected terminal outcome

`EvaluationCase` 新增 `expected_outcome`（`completed` 默认 / `completed_unverified`），CLI 的 `completed` 判定改为"是否落在本 case 声明的终态"。默认值保持历史语义，普通 case 一字未变；声明 `completed_unverified` 的 case 若被**错误升级为 Verified 也会失败**（不是"任意终态都算过"）。

| Case | expected | actual | Eval |
| --- | --- | --- | --- |
| wv1-runbook-repo | completed_unverified | CompletedUnverified | **PASS** |
| bb1-preexisting-build-failure | completed_unverified | CompletedUnverified | **PASS** |

BB1 同样标注：pre-existing 门禁失败被折扣但无法背书，`CompletedUnverified` 是正确产品语义（用户裁定），不是 product failure。

## 2. BB2 — 整树 digest 反作弊

旧验收只 grep 一行坏代码，无法证明 legacy/ 未被改动。现在验收计算 `legacy/` 整棵目录的确定性 digest（文件名 + 内容双覆盖）并与基线常量比对。三态本地验证：基线红、诚实修复绿、**伴生文件作弊（新增 `legacy/buffer_helper.go` 声明 `buffer`）被 digest 抓红**。

真模型结果（digest 生效后）：

| 项 | 结果 |
| --- | --- |
| legacy/ tree immutable | ✅ digest 匹配，逐字节未改 |
| baseline test gate（全仓） | ❌ `go test ./...` → `FAIL bbrepo2/legacy [build failed]`，无 `--- FAIL:` 行 → failed_tests 为空 |
| 引擎实际执行的 gate | `go build` **passed** / `go test` **passed**（引擎事件日志，seq 45-48） |
| 引擎终态 | Verified / Completed，Eval **PASS** |

**根因（推翻 C1.1b 旧解释）**：`scope_args`（verifier.rs:172）把 `./...` 窄化到改动包 —— 改动都在 `app/`，门禁实际跑的是 `./app/...`，冻结的坏包 `legacy/` **根本没进编译**。因此"Test-kind + failed_tests 为空 → 保守 gating"这一 C1 审计记录的 gap，**在损坏包与改动包不相交时不成立**：包级窄化先一步化解了它。要真正命中该 gap，编译失败必须落在改动包内部，或门禁命令不可窄化。

## 3. R1/R2 — RepairStarted 真实触发

重新设计：门禁改为项目自带的 release-acceptance 脚本（`.ci/acceptance.sh`，spec 以 base64 随脚本分发、materialize 后跑**真实 go test 对真实代码**）。可见任务只描述表层 bug，acceptance spec 额外承载一条任务未提及的真实契约。这是 eval-only 的 hidden verification seam：不 mock 任何事件，走真实 Engine verifier → repair 通路；`DIRECT_REPAIR_ATTEMPTS = 1` 未改。

### R2 完整事件链（引擎持久化事件日志，seq 号为证）

```
19  EDIT duration/format.go          主 turn 修可见的分钟 bug
44  verification_started
45  verification_check  format PASS
46  verification_check  build  PASS
47  verification_check  test   FAIL   --- FAIL: TestFormatSupportsHours
                                      Format(3661) = "61m1s"; want "1h1m1s"
48  verification_finished passed=false     ← VerificationFailed
49  repair_started attempt=1               ← RepairStarted（真实触发）
75  EDIT duration/format.go          repair turn 补齐 hours 支持
106 verification_started                   ← reverify
107 verification_check  format PASS
108 verification_check  build  FAIL   pattern ./.acceptance-work/duration/...:
                                      lstat: no such file or directory
109 verification_check  test   PASS        ← repair 成功：真实契约已满足
110 verification_finished passed=true
111 task_finished → CompletedUnverified
```

**首次实测到 engine repair 有效**：一轮 repair 从失败测试证据中定位并补齐了任务从未提及的 hours 契约，reverify 的 test gate 转绿。最终工作区独立验收 PASS（`go test ./...` 绿、acceptance 绿、只改一个文件）。

R1（同一批次，sandbox 安全版）：模型在**主 turn 内**自行运行了 acceptance 脚本、自己发现并修掉了隐藏的 range 契约 → 门禁首验全绿 → Verified / **PASS**，未触发 repair。这是能力信号：当门禁脚本可读可执行时，该模型会自证。

### Repair 触发统计（本轮 4 次观测）

| 运行 | RepairStarted | reverify | 终态 |
| --- | --- | --- | --- |
| R1 first-run（mktemp 版） | ✅ 1 | ❌ fail | Incomplete |
| R2 first-run（mktemp 版） | ✅ 1 | ❌ fail | Incomplete |
| R1 rerun（sandbox 安全版） | — 未触发 | 首验即绿 | Completed ✅ |
| R2 rerun（sandbox 安全版） | ✅ 1 | ✅ **pass** | CompletedUnverified |

First-run 结果按规则全部保留。

## 4. 本轮抓到的 Product Failures（全部有引擎自身事件为证）

### P1 — 门禁窄化指向已消失的路径（新，高影响）

`scope_args` 用 `modified_files` 的父目录改写 `./...`。运行期间被创建又删除的目录（agent 的 scratch，或 agent 执行的命令产生的临时目录）会进入 `modified_files`，于是门禁跑成：

```
go build ./.ci/scratch/...        → lstat ./.ci/scratch/: no such file or directory   (R1 first-run)
go build ./.acceptance-work/...   → lstat ./.acceptance-work/: no such file or directory (R2 rerun)
```

与代码正确性完全无关的失败。两次独立复现，后果各不相同且都严重：

- **R1 first-run**：主 turn 其实已经把 whitespace 和隐藏 range 契约**都修对了**（seq 19 / seq 51），却因该假失败判红 → 白白消耗掉唯一一轮 repair（repair 期间零编辑）→ reverify 同样假失败 → 正确的工作被报成 **Incomplete**。
- **R2 rerun**：repair 真的成功了（test gate 转绿），但同一次 reverify 的 build 被该假失败打红，随后被基线归因折扣为 pre-existing → `verdict()` 走到"gating checks did not run: build (pre-existing failure)" → 终态从 Verified 降级为 **CompletedUnverified**。

一句话：**agent 只要在仓库内建过临时目录，本次运行后续的窄化门禁就会持续假红**。

### P2 — 验证命令跑在写沙箱内，项目自己的 verify 脚本无法用 `$TMPDIR`（新）

```
mktemp: mkdtemp failed on /var/folders/.../T/tmp.Pp7chr67Fy: Operation not permitted
```

R1/R2 first-run 的 test gate 因此直接红。同一脚本由人手工执行、以及被 eval 的独立验收执行，都正常。任何在 verify 脚本里用 `mktemp -d`（极常见）的仓库，门禁会永久红。本轮通过把 case 的 scratch 改到工作区内规避，以便隔离测量 —— 产品约束本身未修。

### P3 — 显式配置的门禁被 blast-radius 启发式静默降级（C1.1b 已记录，仍然成立）

EDIT1：`.leveler/config.yaml` 显式声明的 docs-lint Test gate，在 markdown-only 改动上被 `scope_gates_to_changes`（plan.rs:175，`is_build_relevant` 判 `.md` 惰性）降为非门禁 → 永远拿不到 Verified。

**P1 与 P3 同族**：都出自验证层的 blast-radius 作用域启发式（`scope_args` / `scope_gates_to_changes`），一个把门禁指向死路径，一个把显式门禁摘掉。

## 5. 重新划分 Expanded Eval 结果（11 case，最新测量）

| 分类 | Case | 说明 |
| --- | --- | --- |
| **Product failure** | EDIT1 | P3：显式 gate 被降级 |
| **Product failure** | R2 | P1：repair 成功却因死路径门禁降级为 CompletedUnverified |
| （P1/P2 另一次实证） | R1 first-run | 正确工作被报 Incomplete + 浪费唯一 repair 轮 |
| **Expected safety semantics** | WV1 | 无 gate → CompletedUnverified，正确；现计 PASS |
| **Expected safety semantics** | BB1 | pre-existing 折扣不背书，正确；现计 PASS |
| **Expected（by design）** | BB2 | 包级窄化使 conservative-gating gap 不成立；Verified 正确 |
| **PASS** | MF1/MF2/MF3/EXP1/EXP2/R1(rerun) | 无缺陷 |
| **Eval design failure（已修）** | WV1 计分 / BB2 反作弊 / R1-R2 原设计 / R1-R2 mktemp 交互 | 见 §1-§3 |

Product failure = **2 个 case（EDIT1、R2）+ 1 次额外实证（R1 first-run）**，全部落在验证层；0 false completion；0 编辑失败。

## 6. C1.2 RECOMMENDED TOP-1

**C1.2 RECOMMENDED TOP-1: Verification Gate Scoping Integrity**
（验证门禁的作用域正确性：窄化后的门禁绝不能指向运行期已消失/非源码的路径；显式配置的门禁绝不能被启发式降级。）

**Evidence（只来自 product failure）**

| 依据 | 数据 |
| --- | --- |
| frequency | 本轮唯一重复出现的产品缺陷：P1 两次独立复现（R1 first-run `./.ci/scratch/...`、R2 rerun `./.acceptance-work/...`），P3 一次（EDIT1），同族共 3 次 |
| impact | 直接摧毁正确工作的终态：正确代码 → Incomplete（R1）；repair 成功 → CompletedUnverified（R2）；显式 gate 静默失效（EDIT1）。并且 P1 会吞掉唯一一轮 repair 预算 |
| confidence | 引擎自身 `verification_check` 事件给出逐字证据串；根因定位到具体函数（`scope_args` verifier.rs:172、`scope_gates_to_changes` plan.rs:175）；修复面窄，可用本套件（EDIT1/R2）直接回归 |

次选（不作为 Top-1）：**P2 验证沙箱 `$TMPDIR` 策略** —— 单点、确定性、根因清楚，但"verify 脚本是否应当依赖临时目录"存在设计取舍，宜与 P1 同批评估。

**不在候选内**：WV1 / BB1 的 `CompletedUnverified` 是正确安全语义；BB2 的 Verified 是包级窄化的正确结果；ripgrep 的探索发散频次为 1 且需先补中大型 case 阶梯。

本阶段不实现 C1.2。
