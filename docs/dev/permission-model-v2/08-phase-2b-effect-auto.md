# 08 · Phase 2B 报告：效果导向 Auto（EffectAuto）

日期：2026-11-24
阶段结论：**PASS（macOS 本地；不可逆判定与普通工作放行均通过真实运行时验证）**
范围：不可逆检测器、审批理由码、AUTO 判定、MCP 注解、普通脚本零审批。

## 1. 判定模型

`EffectAuto` 只读取当前调用的结构化输入：

| 输入 | 用途 |
|---|---|
| 工具名 + 声明风险 | 选择分析路径；不单独决定审批 |
| `CommandView { program, args }` | argv / shell AST 分析 |
| MCP typed annotations | 只读 `destructiveHint == true` |
| workspace root | 构建产物白名单的锚点 |
| 环境快照 | 只用于路径/方言判断，不读脚本内容 |

不读取：脚本正文、模型描述、工具说明、自然语言关键词。没有 LLM 分类器，没有“未知命令默认 ASK”。

## 2. 不可逆闭集

| 类别 | 识别对象 | 理由码 |
|---|---|---|
| 文件/目录真实删除 | `rm`/`rmdir`/`unlink`/`shred`/`del`/`erase`/`rd`、`find -delete`/`-exec rm`、`xargs rm`、`rsync --delete` | `irreversible.filesystem_delete` |
| 数据清空 | `truncate -s 0`、重定向清空 | `irreversible.file_destroy` |
| 设备/磁盘 | `dd` 写设备、`mkfs*`、`wipefs`、`fdisk`、`parted`、`diskutil`、`format` | `irreversible.device` |
| 持久数据 | 内联 SQL `DROP`/`TRUNCATE`/`DELETE`/`FLUSH*`、Redis `FLUSH*` | `irreversible.persistent_data` |
| 远端资源 | `docker volume rm`、`kubectl/terraform/pulumi destroy` | `irreversible.remote_resource` |
| 宿主机 | `shutdown`/`reboot`/`halt`/`poweroff` | `irreversible.host_shutdown` |
| Git 历史 | `reset --hard`、`clean -fd`、`push --force`、`branch/tag -D`、`stash drop` 等 | `irreversible.git_history` |
| MCP | `destructiveHint == true` | `irreversible.mcp_destructive` |

只读、构建、测试、格式化、依赖安装、普通 Git（含普通 push）、普通网络、普通 MCP、子 Agent/后台任务均不产生事实。

## 3. 关键实现选择

- **包装器剥离**：`sudo`、`doas`、`env`、`command`、`nohup`、`time`、`nice` 按确定性的 argv 规则剥离；`sudo -n true` 仍是普通工作，`sudo rm -rf <真实文件>` 仍会 ASK。已知取值选项表覆盖 `-u`、`-g`、`--user` 等；未知包装器不猜测，交回原程序分析。
- **不读脚本**：`python script.py`、`npm run build`、`make`、`./scripts/build.sh` 只按 argv 判定，不打开脚本、不追踪副作用。
- **可重建产物**：仅当锚点可证明时自动删除（`Cargo.toml`+`target`、`package.json`+`node_modules`、`next.config.*`+`.next`、`package.json|pyproject.toml|setup.py`+`dist|build|out`，以及 `__pycache__` 等缓存目录）。
- **scratch root 修正**：2D 验收发现“位于 `/tmp`/系统临时目录的真实文件被当作可丢弃”会导致真实删除不 ASK，违反“真实文件删除必须审批”。已移除该豁免；现在只有可证明的构建产物或不存在目标才免问。该修正同时移除了 `EffectInput.scratch_roots` 死参数。
- **路径归一化**：写入解析使用最深已存在祖先的 canonical 路径 + 词法后缀；修复了空后缀导致的尾部分隔符（`seed.txt/` → `ENOTDIR`）。

## 4. 证据

| 证据 | 命令 | 结果 |
|---|---|---|
| 检测器单测 | `cargo test -p leveler-execution --lib irreversibility` | 9 passed（脚本不读、真实删除、构建产物、包装器、Git、SQL、设备、MCP、变量目标） |
| 审批层 | `cargo test -p leveler-agent --lib effect_auto` | 4 passed（普通脚本/构建/普通 Git/网络放行；直接删除/Git 历史 ASK；typed MCP） |
| 工具层 | `cargo test -p leveler-tools --lib effect_auto` | 4 passed |
| 网络 | `cargo test -p leveler-tools --lib any_scope_reaches` | passed（`Any` 不对私有地址施加 SSRF 排除） |
| 真实运行时 | `permission_model_v2_dogfood`（v2_assisted 臂） | 普通写入/工作区外写入/Python/sh 构建/本地 push/私有 LAN 均 0 审批；真实删除与 `git reset --hard` 各 1 次 ASK，拒绝后目标未受影响 |

## 5. 验收

| 标准 | 结果 |
|---|---|
| 普通脚本、构建、测试零审批 | PASS（dogfood `python_script`、`shell_build` 审批 0 且副作用真实发生） |
| 直接可识别不可逆操作 ASK | PASS（`delete_real_file`、`git_reset_hard`，理由码正确） |
| 不读脚本、不做间接分析 | PASS（检测器无文件读取；单测锁定 opaque 脚本为 None） |
| 通过变量/包装器仍识别破坏原语 | PASS（`$dir` 不可解析 → ASK；`sudo/env/nohup` 剥离） |
| MCP 只按 typed 注解 | PASS（`destructiveHint=true` ASK；名称不参与判定） |
| 可重建产物免问且不按目录名 | PASS（锚点检查；单测覆盖） |
| 无 LLM 分类器、无未知命令自动 ASK | PASS（`EffectAuto` 在审批层直接返回 Auto，事实来自结构化输入） |

### 逐项验证

| 不可逆项 | 判定 | 证据 |
|---|---|---|
| 真实文件/目录删除 | PASS | 单测 + macOS/Linux dogfood：ASK，拒绝后文件保留 |
| `find -delete` / `-exec rm` / `xargs rm` / `rsync --delete` | PASS | 检测器单测（`analyze_argv` 分支） |
| `truncate -s 0` / 重定向清空 | PASS | 检测器单测 |
| `dd` 写设备 / `mkfs` / `wipefs` / 分区工具 | PASS | 检测器单测（普通文件 `dd` 不 ASK） |
| 内联 SQL `DROP` / `TRUNCATE` / `DELETE` / `FLUSH` | PASS | 检测器单测（`psql -c`） |
| `docker volume` / `kubectl` / `terraform` / `pulumi destroy` | PASS | 检测器闭集 + 分支 |
| `shutdown` / `reboot` / `halt` / `poweroff` | PASS | 检测器闭集 |
| Git 历史丢失 | PASS | 单测 + dogfood：`git reset --hard` ASK，拒绝后未提交修改保留 |
| MCP `destructiveHint=true` | PASS（host 层） | `effect_auto_uses_typed_mcp_annotations_not_the_tool_name`；live MCP UNVERIFIED |
| 包装器（sudo/env/nohup）隐藏删除 | PASS | `transparent_wrappers_do_not_hide_a_direct_deletion` |
| 可重建构建缓存 | PASS | `real_deletion_asks_and_build_output_does_not` |
| 工作区外真实文件（原 scratch 豁免） | PASS（已修正） | 单测 + 验收：真实文件在临时目录也必须 ASK |

## 6. 未覆盖

- 真实 MCP fixture 未接入 2D dogfood；MCP 证据来自 host 单测与 typed 注解路径，**live MCP 标记 UNVERIFIED**。
- Windows 的 `cmd /C` 分支有单测，但没有 Windows 实机验证。
