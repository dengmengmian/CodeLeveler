# CodeLeveler 1.0.28

[English](RELEASE.md) · [文档索引](README.zh-CN.md) · [历史版本](../CHANGELOG.zh-CN.md)

本版要求人工批准确切内容后，带权限的项目定义才会生效；本地 Runtime 控制 socket 增加访问令牌边界；`/tasks` 与 `/doctor` 成为查看运行工作和诊断的统一入口。`/review` 改为原生、证据驱动的审查，会话增加标题、可逆归档和授权管理，Goal 身份在重启与上下文折叠后保持稳定。发布包改用 xz，旧版浏览器界面退役。

## 安全与权限

- 项目定义默认不生效。任意层级的 `AGENTS.md`、`.leveler/instructions.md`、`.leveler/rules/*.md`、`.leveler/agents` 下的声明式 Agent，以及 `.leveler/skills` 和兼容项目根下的 Skill，只有人工批准其精确字节后才会进入模型的控制上下文。`leveler trust` 列出待批准项并批准显示的 SHA-256，`leveler trust show`、`diff`、`revoke-definition` 管理记录。编辑、`git pull`、提交、重启或 Resume 都不会继承旧批准；未批准的定义不会静默当成不存在，下一回合会收到带路径和状态的诊断。Trust Store 位于工作区之外，缺失、损坏或位于仓库内时失败关闭。
- Skill 的批准覆盖 `SKILL.md` 与整个 `references/` 树，使用复合 SHA-256。`load_skill` 返回校验后的引用正文，受限文件工具（包括递归搜索）拒绝按路径直接读取这些内容。旧的单文件 Skill 批准会显示为 `changed`，需要重新批准一次。
- Unix Runtime 控制 socket 在 `0600` 之外增加每次绑定生成的随机访问令牌。令牌以 `0600` 放在 `0700` 目录中，受限命令不可见；Linux 服务端拒绝不同 mount namespace 的 peer。TCP 上的 `ManageSchedule` 仅可信本地可用。Windows 继续使用命名管道 ACL。
- 执行权限与真实授权对齐：常驻 `Allow` 或自动审查只回答“能否运行”，保留当前模式的资源边界；精确调用授权不会清除显式 `--deny-network`。权限规则拒绝未知字段，含通配符的路径不生成常驻授权，凭据文件名按 ASCII 大小写不敏感比较，共享模型预算按请求原子预留。
- 显式 `--deny-network` 在 Full 下继续生效；审批提示会说明是否会提升工作区写围栏。子 Agent 不持有用户全局定时任务工具。

## 工作台与诊断

- `/tasks` 聚合当前 Goal、子 Agent、后台任务与审查、定时任务定义及最近运行，以及全局存储中的全部会话。进行中 / 最近 / 全部共享一套选择；`x` 通过运行时取消当前会话任务，`r` 重新读取实时来源。
- `/doctor` 与 `leveler doctor` 共享结构化诊断。每项检查都是带稳定 ID、类型化状态、真实证据和可选操作的 finding；`UNKNOWN` 与 `NOT CHECKED` 不会显示为通过，MCP 实时连接状态明确标为未检查。`leveler doctor --json` 输出同一份报告。

## 审查

`/review` 是手动、只读、证据驱动的审查，范围包括当前交付、工作区、整个项目、指定 Session、Commit、分支基线或指定路径。审查者以独立子 Agent 运行，拥有自己的 ID、预算、取消和持久化报告；Lead Reviewer 可启动只读调查，候选 Finding 保留来源，只有明确采纳的才计入正式缺陷。报告是绑定证据版本的完整修订，后续变化会标记过期；审查与所有调查共享仓库的 token / 成本上限。自动审查退役，旧的 `independent_review: required` 不再启动审查。

## 会话与输出

- 标题与逐字保存的 Goal 请求分离；归档可逆；`leveler permissions grants` 可列出和撤销资源授权；删除会话拒绝正在运行的工作。
- `/model <ref>` 只切换当前会话，`/model default <ref>` 修改全局默认；`/permission` 立即生效。`/output tool [id]` 打开完整工具产物，`/copy N` 选择完整回答，`/export` 追加工具证据段。检查点回退需要第二次确认并说明工作区回滚。

## 执行一致性

- 多个交付物按多步骤任务处理；已采纳的 Goal 目标在崩溃恢复和上下文折叠后重新绑定而不是重复创建；调度前崩溃窗口已有测试覆盖。
- 完成证据与实际执行的 argv 对齐，包括模型用显示名引用一次执行的情况。需求一旦成立，其机械判定类型即冻结；用户新的话术可以降低要求，但不能静默提高；Schema 承诺的终态摘要为必填。
- Daemon 先发布访问令牌再发布 socket 端点，并原子写入 ready 文件；启动压力测试覆盖该契约。重启后的回合先读取上下文再行动，首个回合自己的消息与此前工作区分开。每次物理模型尝试只记录一次用量，用户私有数据以仅所有者可读写权限落盘。

## 界面与平台

- TUI 新增会话记录搜索与回合导航，退出时保护正在运行的工作，重建 MCP、项目设置和帮助页，隔离终端设备回复，在搜索片段中中和 bidi 覆盖字符，并为旧命令提供本地化反馈。
- Windows 资源授权绑定稳定的卷号/文件 ID 与创建时间；固定 Commit 与分支审查直接读取 Git 对象，不启动 `git` 进程；持久执行宿主在多个进程间共享写根租约；登录触发器注册到当前用户。
- macOS 与 Linux 发布包改为 `.tar.xz`，约小 40%，同时保留 `.tar.gz` 供旧版本自更新。MCP 声明变化会丢弃缓存连接；定时任务管理器公开其实际接受的策略取值。

## 破坏性变化与已知限制

- 旧版 Web UI 与 `leveler web` / `/web` 退役，终端、CLI 与桌面客户端保留。自动审查退役。旧的单文件 Skill 批准需要重新批准一次。权限文件中出现未知字段时按 degraded 规则集处理并要求批准，而不是扩大为 `Allow`。
- Windows 上受限 Shell 仍可创建原本缺失的 `AGENTS.md` 或 `.leveler` 树，但未经批准绝不会被采纳，既有受保护对象继续受 OS 栅栏保护。FullAccess 仍是明确的用户权限边界，而不是 OS 保证。安装包未经过 Apple 公证或 Microsoft 签名。

本版命令行安装包支持 macOS（Apple Silicon、Intel）、Linux x86_64 和 Windows x86_64。没有 Linux ARM 包。桌面客户端需从源码构建，不包含在发布包中。安装和更新见[首页](../README.zh-CN.md)，完整版本历史见[更新日志](../CHANGELOG.zh-CN.md)。
