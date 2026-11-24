# 03 · 新 Auto 的不可逆后果判定

日期：2026-11-24
目标：设计一个**确定性、结构化、可测试**的判定层，让新 Auto 只对当前调用可直接识别的不可恢复破坏请求批准；不引入动作级 LLM，不做间接副作用追踪。

## 1. 设计原则

1. **判定对象是本次调用解析出的 argv/参数**，不是工具名、自然语言描述或抽象 RiskLevel。
2. **未知不等于危险。** 无法解析的脚本、未知程序按 Auto 的高自主契约直接运行；只有直接可见的已识别破坏动作才 Ask。
3. **删除程序例外。** 当程序本身是已识别的删除/销毁程序，但目标不可读（变量、glob、解析不完整）时，信息不足本身就是风险 → Ask。这是「未知不等于危险」的唯一例外，因为动作已经直接可见。
4. **不读脚本、不读配置、不展开依赖。** `python script.py`、`npm run build`、`make build` 不因为内部可能删除/联网而审批。
5. **不调用模型。** 判定只依赖 argv、shell AST、Git effects、MCP 类型化 annotations。
6. **Ask 必须能说出具体对象。** 提示列出路径 / Git effect / MCP server+tool，不允许「可能有风险」这种泛化理由。

## 2. 判定结果

```rust
pub enum EffectVerdict {
    AutoAllow,
    RequireHumanApproval {
        code: PermissionReasonCode,
        facts: IrreversibleFacts,
        description: String,
    },
    ExplicitDeny {
        code: PermissionReasonCode,
        reason: String,
    },
    ExecutionBoundaryFailure {
        code: PermissionReasonCode,
        reason: String,
    },
}
```

与现有代码的映射：

| 新结果 | 现有 `PolicyResolution` | 说明 |
|---|---|---|
| `AutoAllow` | `Allow(AuthorizationEvidence::Policy { profile })` | 不产生授权记录 |
| `RequireHumanApproval` | `Ask(PendingApproval)` | `ApprovalRequest` 新增 `reason_code` |
| `ExplicitDeny` | `Deny(PolicyDenial)` | 规则/hook/信任/所有权 |
| `ExecutionBoundaryFailure` | `Deny(PolicyDenial { kind: BoundaryFailure })` | 不是用户拒绝，也不是命令失败；是「本宿主无法强制执行该边界」 |

`PermissionReasonCode` 稳定、可序列化，进入 `ApprovalRequested` 事件与 TUI 审批页；这是上一轮审计指出的可观测性缺口。

**无人值守语义：** 新 Auto 的 Ask 在没有人类审批者时变成 `DeniedUnattended`（「没人可问」，不是用户拒绝），模型必须停止该动作。要让无人值守运行执行闭集动作，用户必须预先写 `allow` 规则，或显式选择 `--auto-approve` / Full。不得把「没人可问」投影成 `blocked_kind=user_decision`。

**自动审查不得吞掉不可逆 Ask：** `Executor::ask` 在 reviewer 之前的 `AutoReviewer` 是可选 seam。`irreversible.*` 理由码必须跳过 reviewer，直接进入人审；加测试：注入一个恒返回 `Allow` 的 reviewer，`rm -rf src` 仍然产生真实审批。生产默认 `NeedUserReviewer`，但这条约束防止未来接线时静默绕过。

### 2.1 理由码

| 码 | 含义 | 典型来源 |
|---|---|---|
| `irreversible.filesystem_delete` | 删除真实文件/目录 | `rm`/`rmdir`/`unlink`/`shred`/`find -delete`/`xargs rm` |
| `irreversible.filesystem_destroy` | 清空文件内容 | `truncate -s 0` |
| `irreversible.disk_destroy` | 磁盘/分区/设备销毁 | `dd of=/dev/…`、`mkfs*`、`wipefs`、`fdisk`、`parted`、`diskutil erase*` |
| `irreversible.persistent_data` | 数据库/卷/远端持久数据删除 | `psql -c 'DROP…'`、`redis-cli FLUSHALL`、`docker volume rm`、`kubectl delete` |
| `irreversible.git_history` | Git 历史/工作区不可恢复丢失 | `reset --hard`、`clean -f`、`push --force`、`branch -D`、`stash drop` 等 |
| `irreversible.mcp_destructive` | MCP 工具声明 destructiveHint | `mcp__*` annotations |
| `irreversible.host_shutdown` | 关机/重启/停机 | `shutdown`/`reboot`/`halt`/`poweroff` |
| `boundary.credential_use` | 使用已存凭据 | 现有 credential 阶段 |
| `boundary.credential_store_read` | 读取凭据库原文 | `security find-generic-password -w` 等 |
| `boundary.privilege_escalation` | 请求 OS 提权 | `sudo`/`su`/`doas` |
| `boundary.private_network` | 私有/局域网目的地 | `web_fetch` admission |
| `boundary.definition_write` | 定义写入需真人 | `save_agent`/`save_skill` |
| `boundary.host_ownership` | 子 Agent 未 claim/越界 | ownership |
| `boundary.foreign_path` | 其他执行者拥有的路径 | ownership |
| `boundary.network_unenforceable` | 宿主无法强制请求的网络边界 | Windows 断网、Linux unshare 不可用 |
| `boundary.mcp_network_denied` | 显式断网下的 MCP | MCP 进程不可约束 |
| `boundary.sensitive_path` | 凭据/信任路径 | 文件工具硬拒绝 |
| `boundary.unclassified_privileged_tool` | 工具声明破坏性/特权但无结构化理由 | 宿主契约缺口，显式拒绝 |
| `boundary.git_local_program` | Git 会执行本地程序 | `-c core.hooksPath`、`--exec-path`、`GIT_SSH_COMMAND` |
| `boundary.git_remote_unbrokered` | Git remote 凭据不可绑定 | 现有 `git_remote_credential_unbrokered` |
| `rule.ask` / `rule.degraded` | 用户显式 Ask / 规则不可解析 | permission rules |
| `request.permissions` | 模型显式请求提权 | `request_permissions` / `escalate` |
| `legacy.command_dangerous` | 旧 Auto 的命令分类 | 仅第一档 |
| `legacy.tool_admission` | 工具自身 admission | 第一档 |
| `legacy.mcp` | 旧 Auto 的 MCP 无条件 Ask | 第一档 |
| `legacy.role_permission` / `legacy.read_only_turn` | 旧的角色/只读 overlay | 第一档 |

## 3. 判定流水线

```text
1. Full 短路（不变）
2. pre-hook Deny（不变）
3. 结构性所有权拒绝 → ExplicitDeny（不变，patch 后）
4. 只读角色/只读 overlay（不变）
5. 定义写入 → RequireHumanApproval（不变）
6. 权限规则 deny / ask / allow / degraded（不变）
7. 工具自身 admission_reason（保留：
   native_action、computer_act、task 所有权、私有网络、cwd 约束等）
8. 若工具声明 `Destructive`/`Privileged`，但既没有结构化 admission_reason，也不属于已结构化处理的工具（definition/native/computer/task），返回 `ExplicitDeny`（`boundary.unclassified_privileged_tool`）。这是宿主契约缺口，不得静默自动执行，也不得把它变成笼统的人审。
9. 【新】不可逆检测器 → RequireHumanApproval（命中的第一个事实）
10. 凭据使用 / 凭据库读取 → RequireHumanApproval（独立边界）
11. sudo/su/doas → RequireHumanApproval（独立边界）
12. 必要边界无法强制 → ExecutionBoundaryFailure
13. 其余 → AutoAllow
```

顺序理由：

- 规则和 hook 是用户的显式配置，永远优先于 Auto 的自主判断。
- 所有权是结构性事实，不应被用户批准绕过（patch 已如此）。
- 工具 admission 是宿主声明的具体副作用（邮件发送、桌面控制），不是风险猜测。
- 不可逆检测在凭据之前：一个 `rm` 即使也用到凭据，先说清删除。
- 边界失败放在最后：先决定「要不要做」，再决定「这个宿主能不能安全地做」。

**实现顺序细节：** 在第 6 步之前先跑一次只解析、不决定的 effect analysis，把删除/写入目标加入 `rule_paths`；这样 `write_path_glob` 的 `deny`/`ask` 能覆盖删除目标，而规则的优先级仍高于 facts。facts 在第 9 步才用于触发 Ask。

## 4. 不可逆检测器

### 4.1 输入

```rust
pub struct EffectInput<'a> {
    pub tool: &'a str,
    pub risk: RiskLevel,
    pub command: Option<CommandView<'a>>,
    pub mcp_annotations: Option<McpAnnotations>,
    pub workspace_root: Option<&'a Path>,
    pub scratch_roots: &'a [PathBuf],
    pub env: &'a EnvSnapshot, // 仅用于解析 $TMPDIR 这类可信宿主变量
}
```

命令解析复用：

- `leveler_execution::shell_ast::executed_commands(program, args)`：已解析的完整命令列表、`complete` 标志。
- **Windows `cmd /C` 不走 bash AST。** 它使用与 `classify_shell_script_fallback(script, true)` 相同的 cmd 词法（引号、`^` 转义、`%NAME%` 展开不可读）；`del`/`erase`/`rd` 等 cmd 内建命令按程序名识别。PowerShell 脚本体仍不解析，属于文档化缺口。
- 对 `complete == false` 的脚本，使用同一 tree-sitter AST 的专用 walker，保留 `(program, Vec<Option<String>>)`；不读取脚本体。
- AST 完全失败时，使用 literal token fallback：只识别「段首是已识别破坏程序」的段；引号内的文本不当作命令。

### 4.2 闭集：已识别的破坏动作

| 类别 | 程序 / 形态 | 结构化判据 | 结果 |
|---|---|---|---|
| 文件删除 | Unix：`rm`、`rmdir`、`unlink`；Windows cmd：`del`、`erase`、`rd`、`rmdir /s` | 目标全部可读且全部可重建 → Allow；否则 Ask | 见 4.3 |
| 覆盖删除 | `shred` | 同 rm 的目标分析 | 同上 |
| 清空文件 | `truncate -s 0` / `--size=0` | 目标可重建 → Allow；否则 Ask | — |
| 查找删除 | `find … -delete`；`find … -exec[dir] rm/rmdir/unlink/shred` | 目标集合动态，无法枚举 → Ask | 列出 `find` 表达式 |
| 管道删除 | `xargs rm/rmdir/unlink/shred` | 目标来自 stdin → Ask | 列出 xargs 命令 |
| 同步删除 | `rsync --delete*` | 目标树可能很大 → Ask | 列出 destination |
| 磁盘销毁 | `dd of=/dev/*`、`of=\\.\*`；`mkfs`/`mkfs.*`；`mke2fs`；`wipefs`；`fdisk -w`/`--script`；`parted … mkpart/rm/mklabel`；`sfdisk`；`diskutil erase*`/`partitionDisk`/`secureErase`；Windows `format` | 目标为设备/分区，或 argv 明确写子命令 | Ask；`fdisk`/`parted` 的纯查询形态不 Ask |
| 数据库 | `psql`/`mysql`/`mariadb`/`sqlite3`/`clickhouse-client`/`duckdb` 的内联 SQL（`-c`/`-e`/`--command`/首个位置参数）首关键字 `DROP`/`TRUNCATE`/`DELETE`/`FLUSHALL`/`FLUSHDB`/`SHUTDOWN` | 首关键字（跳过注释与空白），大小写不敏感 | Ask |
| 容器卷 | `docker volume rm` / `prune`；`docker compose down -v`；`docker system prune --volumes` | 明确 volume | Ask |
| 远端资源 | `kubectl delete`；`terraform destroy`；`pulumi destroy` | 明确 delete/destroy 子命令 | Ask |
| 主机停机 | `shutdown`、`reboot`、`halt`、`poweroff` | 程序名 | Ask |
| Git | `git_effects().effects.irreversible == true` | 现有结构化 effects | Ask |
| MCP | `annotations.destructive_hint == Some(true)` | 服务器声明的类型化 hint | Ask |

**闭集之外的程序一律不因「未知」审批。** 新增条目必须同时满足：动作直接可见、判据是结构化的、存在真实的不可恢复后果。不得把「看起来像删除」的自然语言描述加进来。

**内置文件工具的删除：** `apply_patch` 的 Delete File 属于结构化文件编辑（有 diff、可检查），不是 shell 删除程序。新 Auto 将其视为普通文件修改自动执行。若产品要求对内置删除也审批，应作为工具级 admission reason 单独设计，不在 argv 检测器里模拟。

### 4.3 删除目标分类

```text
fn classify_delete_targets(program, args, ctx) -> DeleteVerdict
```

规则：

1. 解析选项：Unix `-f`/`-r`/`-R`/`--recursive`/`--no-preserve-root`/`--`/组合短选项；Windows cmd `/s`/`/q`/`/f`/`/a`；其余为操作数。
2. 没有操作数 → `Allow`（命令会以 usage 失败，不删除任何东西）。
3. 任一操作数不是可证明的字面路径 → `Ask`：
   - 含 `$`、反引号、`$(`, `${`, `*`, `?`, `[`, `{`；
   - 含 `~`（`~user` 也无法在 admission 时确定）；
   - 含 `..` 段；
   - AST 标记该词不可读（`Option::None`）；
   - Windows `%NAME%` 展开。
4. 每个字面目标做「可重建」判定（4.4）。
5. 全部可重建 → `Allow`；否则 `Ask`，提示列出每个非可重建目标。

**提示与审计事实：** `Ask` 时把被删除的具体路径（规范化后的绝对路径）填入 `ApprovalRequest.paths`，把程序/子命令/目标写入 `description`，并设置 `reason_code`。这样审批页、`write_path_glob` 规则和审计事件看到的是同一组路径，不是一句「可能有风险」。

**唯一的环境变量例外：** 整词形如 `$TMPDIR/<rest>` 或 `${TMPDIR}/<rest>`，且 `<rest>` 不含上述不可读字符。使用宿主 `EnvSnapshot` 中的 `TMPDIR`（缺失则 `/tmp`）解析；解析结果必须是某个 scratch root 的严格后代，且 scratch root 不是 `/`。不满足则按不可读处理。

### 4.4 可重建目标

「可重建」只用于**删除**判定；普通文件覆盖不套用此表（产品规定普通覆盖自动执行）。

一个目标 `p` 可重建，当且仅当：

1. `p` 不存在 → 可重建（删不到东西）。
2. `p` 是某个 scratch root 的严格后代（不是 root 本身）：
   - 运行时私有 scratch 目录；
   - 宿主 `TMPDIR`；
   - `/tmp`（Unix）及其解析后的真实路径；
   - Leveler 拥有的 per-workspace 工具缓存目录。
3. `p` 在 workspace 内，且满足以下任一条：
   - 路径中任一段是 `__pycache__`、`.pytest_cache`、`.mypy_cache`、`.ruff_cache`；
   - 存在祖先目录 `A`，使 `p` 的下一个段是 `target` 且 `A/Cargo.toml` 存在；
   - 下一个段是 `node_modules` 且 `A/package.json` 存在；
   - 下一个段是 `.next` 且 `A` 下有 `next.config.*`；
   - 下一个段是 `dist`/`build`/`out` 且 `A/package.json`、`A/pyproject.toml` 或 `A/setup.py` 存在。
4. 其余一律**不可重建**。

设计说明：

- 使用「生态系统锚点」，避免把项目里恰好叫 `target` 的真实源码目录当成构建产物。
- `cargo clean`、`npm run build` 不进入删除检测器：它们是程序自己的清理/构建语义，属于产品接受的间接副作用。
- `p` 存在性的 `stat` 只用于分类，不跟随符号链接写入；删除由 OS 执行。
- 该表是闭集。项目自定义的构建目录不会自动放行；用户可以用 `deny`/`ask` 规则补充，或先让命令失败再显式处理。

### 4.5 Git

直接复用 `leveler-execution/src/git_effects.rs` 的 `irreversible`，不重新解析 Git 子命令。现有点：

| 动作 | `irreversible` | 新 Auto |
|---|---|---|
| `git reset --hard` / `--merge` | true | Ask |
| `git clean -f` / `--force` | true | Ask |
| `git push --force*` / `--mirror` / `--delete` / `+ref` / `:ref` | true | Ask |
| `git branch -d/-D`、`tag -d` | true | Ask |
| `git stash drop/clear` | true | Ask |
| `git checkout/switch -f` 或 `--` | true | Ask |
| `git restore`、`git rm` | true | Ask |
| `git update-ref -d`、`git prune` | true | Ask |
| `git worktree remove/prune -f` | true | Ask |
| 普通 `git add/commit/push/fetch/merge/rebase/status/log` | false | Allow |

需要补充的 Git effects（当前缺口，Phase 2B 一并补）：

| 动作 | 现状 | 目标 |
|---|---|---|
| `git filter-branch`、`git filter-repo` | 未标记 | `irreversible = true` |
| `git reflog expire --expire=now --all` | 未标记 | `irreversible = true` |
| `git gc --prune=now` | 未标记 | `irreversible = true` |

**不可读的破坏性 Git 参数：** 对 `reset`/`clean`/`checkout`/`switch`/`restore`/`push`/`branch`/`tag`/`stash`，若 argv 中存在不可读词（`$FLAGS`、`%FLAGS%`、AST 不完整），Ask。理由与删除目标相同：动作直接可见，但无法证明其可逆。其余 Git 子命令的不可读词不因此审批。

### 4.6 MCP

- 普通 MCP：Allow。
- `destructive_hint == Some(true)`：Ask，**只按精确调用授权**（参数向量是唯一可绑定的资源边界）。`decisions()` 不提供 `ApproveAlways`：持久工具规则会让同一工具的任意参数、跨会话自动放行，超出用户看到的资源。提供 Once / Session（仍按精确签名复用）/ Deny。
- `destructive_hint == Some(false)` / 缺失 / 服务器不支持 annotations：Allow。
- annotations 是服务器提供的不可信元数据，只能**提高**摩擦，不能降低边界：
  - `read_only_hint` 不用于新 Auto 的 Allow 判定（新 Auto 本来就 Allow）；
  - `destructive_hint=true` 用于 Ask；
  - 服务器谎报 `false` 时新 Auto 无法发现 → 文档化缺口。

MCP 在显式 `--deny-network` 下仍是 `ExecutionBoundaryFailure`（进程不可约束），与 annotation 无关。

### 4.7 磁盘/设备判据

- Unix 设备：目标以 `/dev/` 开头，或 `stat` 显示 block/char device。
- Windows：`\\.\PhysicalDrive*`、`\\.\*:`、`format` 程序。
- `dd of=<普通文件>` 不进入磁盘销毁；它是普通文件覆盖 → Allow（产品规定普通覆盖自动执行）。
- `mkfs` 的 `-t` 参数不影响判定；程序名匹配即可。
- `fdisk`/`parted` 只有在包含写子命令/交互式分区操作时才是销毁；为降低误报，只在它们带 `-w`/`--script` 或 `parted … mkpart/rm/mklabel` 时 Ask；否则 Allow。实现时以 argv 明确子命令为准，不做交互式猜测。

## 5. 信息不足与解析失败

| 情况 | 新 Auto | 理由 |
|---|---|---|
| 脚本含 `$()` / 反引号，AST 解析失败 | Allow，除非 fallback 在段首识别到破坏程序 | 不因未知审批 |
| 程序是 shell wrapper 但没有 `-c`（`sh script.sh`） | Allow | 不读脚本 |
| 程序未知（`foo --bar`） | Allow | 未知 ≠ 危险 |
| 破坏程序存在，但目标不可读（`rm -rf "$dir"`） | Ask | 动作直接可见，目标不可证 |
| `git reset "$FLAGS"` | Ask | 可能是 `--hard` |
| `python -c "os.system('rm -rf /')"` | Allow | 不扫描语言内嵌 payload |
| `eval "rm -rf /"` | Allow | 不做字符串命令重构；显式 deny 规则可覆盖 |
| `make clean` / `cargo clean` / `npm run clean` | Allow | 间接副作用，产品接受 |
| `cp`/`mv` 覆盖 | Allow | 普通覆盖 |
| `rsync`（无 `--delete`） | Allow | 普通同步 |

这条表是产品契约的一部分：实现者不得为了「更安全」逐项改成 Ask，否则违反新 Auto 的自主性承诺。

## 6. 与现有命令分类器的关系

| 层 | 第一档 `LegacyAuto` | 第二档 `EffectAuto` |
|---|---|---|
| `classify_command()` | 使用：决定 Ask/Allow | **不用于 Ask 判定**；只在需要时复用其 AST/子命令解析来喂不可逆检测器 |
| `command_is_destructive()` | 用于提升提示的 RiskLevel | 不再用于判定；可作为提示文案 |
| `git_command_class()` | 使用 | 只使用 `git_effects.irreversible`；本地程序/不可绑定 remote 仍 Ask（安全边界） |
| 工具 risk | `permits()` / 静态 Destructive 仍 Ask | 不按抽象 risk Ask；工具自己的 `admission_reason` 仍生效 |

注意：`classify_command()` 仍会在第一档产生 `command.dangerous`。第二档不得调用它来决定 Ask，否则 `sudo`、host opener、非工作区 rm 等旧规则会全部回来。

`sudo`/`su`/`doas` 在第二档单独保留 Ask，理由不是「危险分类」，而是「请求 CodeLeveler 不具备的 OS 权限」：这是独立边界，OS 密码提示仍可能出现，运行时不能替用户承诺 root 权限。

## 7. 判定示例（可直接变成测试）

| 调用 | 解析事实 | 新 Auto 结果 | 理由码 |
|---|---|---|---|
| `read_file src/main.rs` | 读 | AutoAllow | — |
| `write_file /Users/me/notes.txt` | 普通写 | AutoAllow | — |
| `python script.py` | argv 无破坏程序 | AutoAllow | — |
| `npm run build` | argv 无破坏程序 | AutoAllow | — |
| `sh -c 'rm -rf src'` | `rm` 目标 `src` 非可重建 | RequireHumanApproval | `irreversible.filesystem_delete` |
| `sh -c 'rm -rf target'`（有 `Cargo.toml`） | 可重建 | AutoAllow | — |
| `sh -c 'rm -rf /tmp/build-123'` | scratch 子路径 | AutoAllow | — |
| `sh -c 'rm -rf "$dir"'` | 目标不可读 | RequireHumanApproval | `irreversible.filesystem_delete` |
| `sh -c 'find . -name "*.tmp" -delete'` | 动态目标 | RequireHumanApproval | `irreversible.filesystem_delete` |
| `git reset --hard` | `irreversible` | RequireHumanApproval | `irreversible.git_history` |
| `git push --force origin main` | `irreversible` | RequireHumanApproval | `irreversible.git_history` |
| `git push origin main` | 非不可逆 | AutoAllow | — |
| `dd if=/dev/zero of=/dev/disk2` | 设备目标 | RequireHumanApproval | `irreversible.disk_destroy` |
| `dd if=in.iso of=out.img` | 普通文件覆盖 | AutoAllow | — |
| `psql -c 'DROP TABLE users'` | 内联 SQL 首关键字 | RequireHumanApproval | `irreversible.persistent_data` |
| `psql -f migration.sql` | 不读文件 | AutoAllow | — |
| `docker compose down -v` | volume | RequireHumanApproval | `irreversible.persistent_data` |
| `mcp__db__drop_table` + destructiveHint=true | annotation | RequireHumanApproval | `irreversible.mcp_destructive` |
| `mcp__fs__read` + readOnlyHint=true | annotation | AutoAllow | — |
| `sudo -n true` | 提权 | RequireHumanApproval | `boundary.privilege_escalation` |
| `web_fetch https://example.com` | 公有网络 | AutoAllow | — |
| `web_fetch http://192.168.1.1` | 私有网络 | RequireHumanApproval | `boundary.private_network` |
| `git -c core.hooksPath=/tmp/x status` | 本地程序 | RequireHumanApproval | `boundary.git_local_program` |
| 用户 `deny run_command` 规则 | 显式规则 | ExplicitDeny | `rule.deny` |

## 8. 不检测清单（必须写进产品文档）

- 脚本/构建工具/包管理器内部的行为：`python script.py`、`make clean`、`npm run postinstall`、`cargo clean`。
- 语言内嵌 payload：`python -c`、`node -e`、`eval`、`sh -c` 之外的字符串重构。
- 通过文件执行的 SQL：`psql -f drop.sql`、`mysql < dump.sql`。
- 未列入闭集的同步/部署/IaC 工具：`aws`、`gcloud`、`ansible`、`helm`、`rsync`（无 `--delete`）等。
- 普通文件覆盖：`cp`、`mv`、`write_file`、`apply_patch`、shell 重定向。
- MCP 服务器谎报 annotations。
- 交互式程序内部的破坏（`fdisk` 交互会话、数据库交互 shell）。
- 已经由 OS 拒绝的操作（只读文件系统、权限不足）——如实报错，不转成审批。

产品文案不得声称「拦截所有不可逆操作」。正确表述：**新 Auto 只对当前调用可直接识别的不可恢复破坏请求批准；脚本内部的间接副作用由用户承担的自主执行风险覆盖。**

## 9. 测试计划（Red → Green）

### 9.1 决策表（新文件）

`crates/leveler-execution/tests/effect_auto_decision_table.rs`

| 测试 | 初始红（功能未实现时） |
|---|---|
| `effect_auto_allows_ordinary_development` | 当前 `Assisted` 对 `rm -rf src` 是 Auto，新合约期望 Ask |
| `effect_auto_asks_for_real_deletion_but_not_build_cache` | `rm -rf src` / `rm -rf target` 当前同判 |
| `effect_auto_asks_for_git_history_loss_only` | `git reset --hard` 当前 Auto |
| `effect_auto_allows_unanalyzable_scripts` | 当前对 `$()` 失败脚本 Ask |
| `effect_auto_asks_for_unreadable_deletion_target` | 已有分类器 Ask；新路径必须保持 |
| `effect_auto_asks_for_inline_database_destroy` | 当前 Auto |
| `effect_auto_asks_for_mcp_destructive_hint` | 当前 MCP 全 Ask；新 Auto 普通 MCP 要 Auto |
| `effect_auto_keeps_explicit_rules_and_boundaries` | 规则/凭据/私有网络仍生效 |
| `effect_auto_full_access_is_unchanged` | Full 全 Auto |

### 9.2 真实执行（`auto_permission_sandbox.rs` 扩展）

- 新 Auto 写工作区外文件成功（`write_file` / `apply_patch` / `sh -c 'echo > ../outside'`）。
- 新 Auto 删除非可重建目标：未批准时文件仍在；批准后删除成功。
- 新 Auto 删除 `target/`：0 审批，目录消失。
- 第一档同场景：工作区外写仍 ASK；`rm -rf src` 仍 Allow（旧 Auto 语义）。
- `--deny-network`：macOS seatbelt / Linux bwrap 真实断网；Windows 返回 BoundaryFailure（需 Windows 宿主）。
- 子 Agent 未 claim：文件未变，工具结果是 Deny + `claim_write_scope`。

### 9.3 反回归

- `crates/leveler-execution/tests/auto_permission_decision_table.rs` 保留给第一档，整体重命名/复制为 legacy 表；不得直接改期望值来「适配」新 Auto。
- `full_permission_*` 全套保持绿。
- `multi_agent_test` 的 claim/unclaimed/full_access 保持绿。

## 10. 与 Grok Auto 的关系

Grok Build 的 Auto 是「确定性 fast path + heuristic + 必要时 LLM 分类」。V2 只取它的**确定性部分**：

- 先走结构化快路径；
- 授权单元贴近资源；
- 失败的分类不能静默放行。

不采用它的 LLM 分类器：上一轮实测每次分类约 3.3–8.4 秒，且产品要求本轮不新增模型调用、不对间接副作用做推断。CodeLeveler 的新 Auto 比 Grok 更「粗」：它只拦可直接识别的不可逆动作，其余交给用户选择的自主性。这是有意的产品差异，不是能力缺口。

## 11. 剩余风险

| 风险 | 影响 | 缓解 |
| 可重建目标启发式误判（项目把 `target/` 当源码目录） | 自动删除真实数据 | 锚点要求 `Cargo.toml`/`package.json` 等；提示写明路径；显式 `ask` 规则可覆盖；文档明示 |
| 服务器谎报 MCP `destructiveHint=false` | 不可逆 MCP 删除自动执行 | 配置层 `ask` 规则；MCP server 是用户选择的信任边界；文档明示 |
| 间接副作用（脚本删库、`Makefile` 删文件） | 自动执行 | 产品接受的 Auto 风险；用 `deny`/`ask` 规则或更严格档位 |
| 删除目标通过符号链接指向真实数据 | 路径分类与删除目标可能不一致 | 目标分类用 `symlink_metadata`；删除由 OS 执行；文档列明路径模型不保证 inode 身份 |
| `TMPDIR=/` 之类的异常环境 | 可重建判定过度放行 | 解析结果必须是 scratch root 的严格后代且 root 不是 `/`；否则按不可读 |
| 新增破坏程序条目导致审批膨胀 | 违背 Auto 自主性 | 闭集评审；每条必须有测试与真实后果；不做自然语言关键词 |
