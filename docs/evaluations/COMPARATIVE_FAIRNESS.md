# Comparative Fairness — 已知不可消除差异与偏置方向

对照原则（PRE_BETA_EVAL_PLAN §5）：同 case、同冻结 revision、同 prompt 原文、同 wire 模型
（`deepseek-v4-flash`）、同网关（`https://taotoken.net/api/v1`）、同 key、同 wall-clock 超时、
统一外部判分（case 的 `expect`，基线必须先红）。以下是无法完全对齐的项，逐条记录偏置方向与处理。

| # | 差异 | 受影响方 | 原因 | 预期偏置 | 处理 |
|---|---|---|---|---|---|
| 1 | 每工具调用超时：DSH bash 默认 60s/次 | DSH | DSH 产品默认（`dsh-bash-sandbox timeoutMs: 60000`） | 长构建 case（yq/scale）DSH 不利 | 保留——测的是 shipping 产品；逐 case 记录是否因此失败 |
| 2 | 权限语义：CL `assisted+--auto-approve`；AtomCode `-y`（跳过全部提示）；DSH `danger-full-access`（approval never + sandbox 放开） | 三方 | 各家"无人值守"档语义不同 | DSH/AtomCode 名义权限更宽；CL 保留自动审批边界 | 记录；工作区外写入按 SAFETY 观察项单独记 |
| 3 | 完成自报语义：CL 有结构化终态（Verified/CompletedUnverified/…）；AtomCode/DSH 只有最终文本+退出码 | AtomCode/DSH | 产品形态差异 | FALSE_COMPLETION 只能对 CL 精确测量（§32 本来就是 CL 核心指标）；对照家以"最终输出声称成功 && expect 红"人工复核 | 对照家该指标标 UNMEASURED_PRECISE，附人工复核记录 |
| 4 | DSH 启动垫片：从 case 工作区 cwd 启动需 `TSX_TSCONFIG_PATH` 指回 monorepo（tsx 路径别名） | DSH | DSH 未发布独立二进制，源码运行 | 无行为影响（纯模块解析） | 记录在案；不改 DSH 行为 |
| 5 | DSH 模型接入为 `--patch` overlay 自定义 provider（`api: openai-completions` + `thinkingFormat: deepseek`） | DSH | DSH 默认 provider 指官方端点，key 只在网关有效 | 与 CL/AtomCode 同端点同 key——这是对齐手段本身 | 记录 patch 全文于 evidence |
| 6 | **AtomCode 用真实用户配置 `~/.atomcode`**（用户指示）：默认走 `llm-api.atomgit.com`，**据用户确认其上游即 taotoken**——三家实际同一上游模型服务 | AtomCode | 测 shipping 产品的真实形态 | 残余差异仅多一跳 AtomGit 中继（时延/限流），模型服务一致；AtomCode 行为含其真实产品默认（subagent 等） | 记录（上游一致性来自用户对自有基础设施的确认，未做线级验证）|
| 7 | 上下文声明：CL `deepseek-v4-flash` 1,048,576；DSH patch 1,000,000；**AtomCode 真实配置 `context_window = 512000`**（先前文档误记为 1M） | AtomCode | 真实 `~/.atomcode/config.toml`，评测不得改 | 超长上下文 case 上 AtomCode 可能更早截断；HC-001（navsvc）远小于 512K，预期无影响 | 记录；不改真实配置 |
| 8 | Harness thesis 弱档 glm-5.2 窗口 200K/100K（其余 1M） | thesis 车道 | 弱模型自身能力 | 大上下文 case 上"弱模型"与"小窗口"混淆 | 按 case 记录；thesis 结论措辞受限 |
| 9 | AtomCode subagent 默认开启（max_concurrent 3）；CL delegation 默认开启；DSH subagent/goal 插件默认在 | 三方 | 产品默认 | 委派是各家产品行为的一部分 | 机会-采纳按 §12/§13 记录，不计入对错 |
| 10 | 判分基线红探针会先跑一次 expect（可能预热依赖缓存，如 cargo registry） | 三方同等 | 外判需要证明基线是红的 | 三方同等受益（探针后 `git clean -qfdx` 还原树，缓存在全局目录） | 同等即公平 |
| 11 | web/搜索：AtomCode 用真实配置（`web_search provider=exa`，可能触发外部检索）；DSH `tool-web fetch:false` 默认；CL 无内建 web 搜索 | 三方 | AtomCode 真实配置随附能力 | AtomCode 在检索型 case 或有额外信息优势 | 逐 case 记录是否实际用了 web_search |
| 12 | AtomCode 各 run 共享真实 `~/.atomcode`（sessions/datalog 累积），CL/DSH 每 run 隔离 HOME | AtomCode | 真实配置的代价 | 跨 run 状态污染风险低（per-project datalog 分桶；会话不复用 `-c`） | 记录；不复用 `--continue` |

## 判分与标签冻结（§19）

- PASS = expect 退出 0（且该 case 基线红已证明）
- FAIL = harness 正常结束但 expect 非 0
- TIMEOUT_FAIL = 达到统一 wall-clock 上限被 SIGKILL 后 expect 非 0（计 FAIL 类，另计超时率）
- UNJUDGEABLE_BASELINE_GREEN = 基线即绿，逐出该 case（对照集已剔除 rust-mul，换 rust-first-even）
- INFRA_FAILURE = 适配器/provider 故障，不计成绩
- PARTIAL 复核：FAIL 行逐例看 diff 与 expect 输出人工判定，报告中单列，不进 PASS 率
