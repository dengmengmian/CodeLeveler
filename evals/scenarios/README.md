# Scenario evals — 真实用户级 Agent 验收

> 结论先行：`scenarios/` 是把"真实用户会交给 Agent 的任务"固化成可复现验收的场景集。
> 与 `smoke/core/hard` 用同一个 `EvaluationCase` schema 和同一个 `leveler eval` runner，
> 只是任务更贴近真实工程（真实大仓、长上下文、权限、TUI），验收更强调**验证驱动**。

## 目录

| 子目录 | 考什么 |
|--------|--------|
| `feature/` | 在真实/较大代码库里端到端加一个功能（CLI flag、字段、模块） |
| `debugging/` | 定位并修复真实 bug，禁止改测试绕过 |
| `long_context/` | 大仓探索、跨文件、长上下文保持 |
| `permission/` | 危险操作是否请求授权/拦截/保护敏感文件（Phase 7 落地） |
| `tui/` | 通过真实 TUI 客户端路径的交互验收（Phase 4 落地） |

## Case 格式（复用 `EvaluationCase`）

```yaml
id: <唯一 id>
name: <人类可读标题>
repo: fixtures/repos/<name>   # 可选：clone 真实仓（本地、gitignored、不复制进仓库）
base_ref: "<tag 或 SHA>"       # 可选：固定 commit，保证可复现
files: {}                      # 无 repo 时即整个 workspace；有 repo 时作为 overlay 注入 bug/失败测试
max_rounds: 140                # 真实大仓给更大预算
task: |
  自然语言任务，独立于具体实现，写清可观测的验收契约。
expect:                        # 独立验收命令，退出码 0 = 通过
  program: bash
  args: ["-c", "<自包含断言脚本>"]
```

### 关键约定

1. **验收与"模型说完成"解耦**：`expect` 是独立命令，`false_completion_rate` 就靠它兜底。
2. **验收脚本放在 `expect` 里，不落工作区**：Agent 改不到验收逻辑，杜绝"改测试骗过"。
3. **真实仓不入库**：`fixtures/` 整体 gitignored，由 `scripts/fetch_eval_repos.sh` 按固定 ref 拉取。
4. **task 独立于实现**：任务描述只讲契约（要什么行为），不泄露该改哪个文件/怎么改。

## 运行

```sh
# 先按固定 commit 拉真实仓（只需一次；已存在则跳过）
scripts/fetch_eval_repos.sh ripgrep

# 跑 feature 场景（换成你配置的模型）
leveler eval run --cases evals/scenarios/feature --model <provider/model> \
  --json-out evals/baselines/scenarios-feature.json
```

## 新增一个真实仓场景（红→绿自检）

沿用 `evals/README.md` 的纪律：加 case 前，先确认

1. **红**：未实现时 `expect` 失败（`git checkout <base_ref>` 干净仓上直接跑 `expect` 应失败）。
2. **绿**：贴一份已知可行实现后 `expect` 通过。

只有红/绿都验证过，这个 case 才不是"摆设"。ripgrep 场景的固定 ref 见 `scripts/fetch_eval_repos.sh`。
