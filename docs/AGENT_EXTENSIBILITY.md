# Custom Agents

Chinese version: [`AGENT_EXTENSIBILITY.zh-CN.md`](AGENT_EXTENSIBILITY.zh-CN.md)

An agent is a directory with two files. `agent.yaml` states what the agent may do; `instructions.md` states how it should do its job. CodeLeveler resolves the agents a project can use, the main agent picks one when a task calls for it (or you name it), and the runtime enforces every bound the definition declares. A new agent needs no code change and no rebuild.

## How to create one

| You are | Do this |
| --- | --- |
| Using the conversation | Say "create a read-only security reviewer agent for this project". CodeLeveler proposes the definition and shows it; nothing is written until you confirm. |
| Using the Web UI | Open a conversation, then Settings → Agents → Create Agent. |
| Editing files | Create `.leveler/agents/<name>/agent.yaml` and `instructions.md`. |
| A team | Commit the project's `.leveler/agents/` directory. |

To see what resolved: `leveler agents list` (or `--json`), `leveler agents show <name>`, `/agents` in the TUI, or Settings → Agents in the Web UI.

## Directory layout

```text
<repo>/.leveler/agents/<name>/agent.yaml         project agents
<repo>/.leveler/agents/<name>/instructions.md
~/.leveler/agents/<name>/agent.yaml              user agents, every project
~/.leveler/agents/<name>/instructions.md         ($LEVELER_HOME/agents when set)
```

Built-in agents ship with the binary: the runtime roles `default`, `explorer`, `worker` and `reviewer`, and the personas `code-explorer`, `code-architect` and `code-reviewer`.

The directory name is the agent's name. Hidden entries and plain files are ignored. A single-file `<name>.md` from earlier builds is not read; `leveler doctor` reports it and the directory to move it to, and nothing is migrated or deleted for you.

## Precedence

```text
project  >  user  >  built-in
```

A hidden definition is still reported (`shadows user`, `shadows builtin`) by every listing.

- **A broken definition does not fall back.** If the project's `foo` is invalid, `foo` is invalid; it is not replaced by the user's `foo`.
- **The runtime roles are reserved.** `default`, `explorer`, `worker` and `reviewer` have structural meaning, so a directory with one of those names is reported as a problem and not loaded. The personas are ordinary definitions and may be overridden.

## `agent.yaml`

```yaml
version: 1                      # required; this build reads version 1 only
name: security-reviewer         # required; must equal the directory name
description: Reviews Rust changes for exploitable security issues.
capability: read_only           # required: read_only | writer | scoped_writer
model: deepseek/deepseek-flash  # optional; omitted = the parent's model
reasoning_effort: high          # optional: minimal|low|medium|high|xhigh|max
skills: [rust-security]         # optional; installed skill names
tools: [read_file, grep, git_diff, find_files]   # optional narrowing
workspace:                      # optional; writer capabilities only
  write_roots: [crates/leveler-web/web]
budget:                         # optional
  max_rounds: 40
  max_duration_secs: 900
```

| Field | Rule |
| --- | --- |
| `version` | Required, and `1`. A file without it is refused, not guessed at. |
| `name` | Lowercase `[a-z][a-z0-9-]*`, at most 64 characters, not ending in `-`, not a Windows device name. It must match the directory. |
| `description` | One line, at most 200 characters. It is what the main agent reads when choosing. |
| `capability` | The runtime class the agent runs under — see below. |
| `model` | `provider/model`. Must be configured, or the agent is unavailable; it is never replaced by another model. |
| `reasoning_effort` | Must be an effort the model offers exactly, or the agent is unavailable. |
| `skills` | Each skill must be installed, or the agent is unavailable. |
| `tools` | Narrows the class's toolset. A `read_only` agent may list only read-only tools; MCP tools cannot be listed; `[]` is an error. Every listed tool is required, so a missing one makes the agent unusable in that session. |
| `workspace.write_roots` | Writer classes only. Repository-relative, `/`-separated, no `..`, no globs, not the repository root. |
| `budget.max_rounds` | 1–1000. |
| `budget.max_duration_secs` | 1–1200. |

Unknown fields fail the definition at every level, so a typo makes the agent invalid instead of being silently ignored. There is no field that can hold a credential.

## Capability classes

A definition does not create a new kind of agent. It runs under one of the existing child contracts, and everything it declares can only narrow that contract:

| `capability` | Runs as | May write |
| --- | --- | --- |
| `read_only` | the Explorer contract | Nothing. No mutating tool is offered, `run_command` included. |
| `writer` | the Default child contract | Only paths it claims with `claim_write_scope` after reading the code, and only inside `write_roots` when set. |
| `scoped_writer` | the Worker contract | Only the exclusive `files` given at spawn, which must lie inside `write_roots` when set. |

```text
agent maximum   = capability class ∩ tools ∩ write_roots   (agent.yaml)
spawn request   = agent + files                              (spawn_agent call)
effective       = what runtime admission allows ⊆ agent maximum
```

A spawn can repeat the agent's class but never change it. Children never receive MCP tools or the agent authoring tools, and cannot spawn children of their own.

## `instructions.md`

Role-specific guidance: what to look for, what to report, how to report it. It is required, non-empty UTF-8 text, at most 64 KiB.

At spawn, the child's first system message is composed in this order:

```text
base prompt → project rules (AGENTS.md, .leveler/instructions.md) → capability-class text
→ agent instructions → each bound skill
```

The child's task stays its own user message. The main agent sees only names and descriptions.

**Instructions are not authority.** They shape how the agent works; they do not change which tools it holds or which files it may write. Do not put runtime rules, tool schemas, permission mechanics, or credentials in them — they are sent to the model.

## Validity and availability

| Status | Meaning | Spawnable |
| --- | --- | --- |
| `available` | Valid, and everything it names is configured here. | yes |
| `unavailable` | Valid, but its model is not configured, a skill is missing, or its effort is not offered. | no |
| `invalid` | The files themselves are wrong: YAML, schema, name, missing instructions, a symlink. | no |

One bad agent does not affect the others or stop CodeLeveler from starting, and `leveler doctor` reports invalid definitions by name. `available` is a configuration check, not a provider check: a pinned model whose API key is missing fails at the child's first request. The Web UI therefore labels this state "已配置" (configured).

## Using an agent

The main agent may choose an agent for part of a task; being listed only makes an agent available. You can also name one: "let security-reviewer check this change". A name that does not resolve is an error listing the available agents — it never falls back to a default child.

The harness's independent Reviewer is not an agent you can request, and an agent whose name contains "reviewer" gains no special authority.

## Running children keep their definition

When an agent is spawned, the resolved definition is recorded on the child's durable start, and its instructions and skills are part of its own first system message. A restarted child continues under exactly that and never re-reads the files:

- editing an agent affects new spawns only;
- deleting an agent does not stop a running child, and its settled history still shows the agent's name;
- a child spawned read-only stays read-only after the file is changed to `writer`.

The definition's fingerprint is for inspection and debugging, not a signature. The registry is re-read when needed; there is no file watcher and no restart required.

## Changing agents safely

Every surface writes through one store: create stages both files and renames the directory into place, update swaps a staged directory in and carries unchanged files over byte for byte, and delete renames the directory away before removing it. There is no rename operation: create the new name and delete the old one. Built-in agents cannot be edited; copy one into a project or user agent of another name.

The agent's own edit tools cannot patch `.leveler/agents/` in place, so a change made from the conversation always goes through this path and is validated when loaded. From the conversation, `save_agent` and `delete_agent` validate the proposal first and then always ask you to confirm, in every permission mode including full access. The confirmation shows the scope, capability, write bounds, tools, model, effort, skills and instructions. A session approval covers only that exact proposal, and an agent write never becomes a standing permission rule.

## Trust

**Project agents are repository-controlled instructions.** Like `AGENTS.md`, they come from whoever wrote the repository. Review `.leveler/agents/` before using an untrusted repository. Whatever an agent's instructions say, its authority is the capability class and bounds above; instructions cannot grant tools or write scope.

User agents live outside the repository. Repository content cannot create them: writing a user agent from the conversation needs your explicit confirmation, and the Web UI writes one only on your action. Remote clients can see a running child's agent name but cannot list or change agents.

## Examples

Read-only reviewer:

```yaml
version: 1
name: security-reviewer
description: Reviews changes for exploitable security issues and reports only high-confidence findings.
capability: read_only
reasoning_effort: high
tools: [read_file, grep, find_files, git_diff, git_status]
```

```markdown
Review the change you are given for vulnerabilities an attacker could use:
injection, path traversal, authorization gaps, secrets in code or logs.
Report each with file:line, the input that triggers it and the consequence.
Report nothing you are not confident in.
```

Frontend writer bounded to one subtree:

```yaml
version: 1
name: frontend-worker
description: Implements small React/TypeScript changes under the web app.
capability: writer
workspace:
  write_roots: [crates/leveler-web/web/src]
budget:
  max_rounds: 60
```

## Not in this version

An agent cannot create another kind of runtime role, hold credentials, or gain authority its capability class does not have. Agent definitions do not change session permission, collaboration mode, or write scope outside the declared bounds.
