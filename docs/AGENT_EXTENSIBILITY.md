# Custom Agents

An agent is a directory with two files. `agent.yaml` states what the agent may
do; `instructions.md` states how it should do its job. CodeLeveler resolves the
agents a project can use, the main agent picks one when a task calls for it (or
you name it), and the runtime enforces every bound the definition declares.
A new agent needs no code change and no rebuild.

## How to create one

| You are | Do this |
| --- | --- |
| Using the conversation | Say "create a read-only security reviewer agent for this project". CodeLeveler proposes the definition and shows it; nothing is written until you confirm. |
| Using the Web UI | Open a conversation, then Settings → Agents → Create Agent. |
| Editing files | Create `.leveler/agents/<name>/agent.yaml` and `instructions.md`. |
| A team | Commit the project's `.leveler/agents/` directory. |

To see what resolved: `leveler agents list` (or `--json`), `leveler agents show
<name>`, `/agents` in the TUI, or Settings → Agents in the Web UI.

## Directory layout

```text
<repo>/.leveler/agents/<name>/agent.yaml         project agents
<repo>/.leveler/agents/<name>/instructions.md
~/.leveler/agents/<name>/agent.yaml              user agents, every project
~/.leveler/agents/<name>/instructions.md         ($LEVELER_HOME/agents when set)
```

Built-in agents ship with the binary: the runtime roles `default`, `explorer`,
`worker` and `reviewer`, and the personas `code-explorer`, `code-architect` and
`code-reviewer`.

The directory name is the agent's name. Entries whose name starts with `.` are
ignored — the store uses them for staging — and so are plain files. The
single-file `.leveler/agents/<name>.md` format of earlier builds is no longer
read. A file in exactly that shape (`<name>.md` with a `---` frontmatter fence,
directly in an agents directory) is reported by every listing and by
`leveler doctor` as not loaded, with the directory to move it to. Nothing is
migrated, renamed or deleted for you.

## Precedence

A project agent hides a user agent of the same name, which hides a built-in:

```text
project  >  user  >  built-in
```

The hidden definition is still reported (`shadows user`, `shadows builtin`) by
every listing.

Two rules keep precedence from surprising you:

- **A broken definition does not fall back.** If the project's `foo` is invalid,
  `foo` is invalid: it is not replaced by the user's `foo`. Spawning it is
  refused with the reason.
- **The runtime roles are reserved.** `default`, `explorer`, `worker` and
  `reviewer` have structural meaning in the runtime (the Worker's pre-claimed
  scope, the harness-launched Reviewer), so a directory with one of those names
  is reported as a problem and not loaded. The personas are ordinary
  definitions and may be overridden.

## `agent.yaml`

```yaml
version: 1                      # required; this build reads version 1 only
name: security-reviewer         # required; must equal the directory name
description: Reviews Rust changes for exploitable security issues.
capability: read_only           # required: read_only | writer | scoped_writer
model: deepseek/deepseek-v4-pro # optional; omitted = the parent's model
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
| `name` | `[a-z][a-z0-9-]*`, at most 64 characters, not ending in `-`, not a Windows device name. Lowercase only, so `Foo` and `foo` cannot be two agents on one filesystem and one on another. |
| `description` | One line, at most 200 characters. It is what the main agent reads when choosing, on every turn. |
| `capability` | The runtime class the agent runs under — see below. |
| `model` | `provider/model`. Must be configured on the machine, or the agent is unavailable; it is never replaced by another model. |
| `reasoning_effort` | Must be an effort the model offers exactly, or the agent is unavailable. The runtime's usual rounding to a neighbouring effort does not apply. |
| `skills` | Each skill must be installed (project, user or built-in), or the agent is unavailable. |
| `tools` | Narrows the class's toolset. Each name must be a known tool; a `read_only` agent may list only read-only tools; MCP tools cannot be listed. `[]` is an error — omit the field for the full set. Every listed tool is required: if this session does not have one (for example `web_search` without a search key), the agent cannot run here. |
| `workspace.write_roots` | Writer classes only. Repository-relative directories or files, `/`-separated, no `..`, no globs, not the repository root. |
| `budget.max_rounds` | 1–1000. |
| `budget.max_duration_secs` | 1–1200 (the runtime's child wall-clock cap). |

Unknown fields fail the definition at every level. A typo such as
`workspace: { wirte_roots: [...] }` or `api_key: ...` makes the agent invalid
rather than silently ignored — and there is no field that can hold a
credential.

## Capability classes

A definition does not create a new kind of agent. It runs under one of the
runtime's existing child contracts, and everything it declares can only
narrow that contract:

| `capability` | Runs as | May write |
| --- | --- | --- |
| `read_only` | the Explorer contract | Nothing. No mutating tool is offered at all, `run_command` included. |
| `writer` | the Default child contract | Only paths it claims with `claim_write_scope` after reading the code — and, when `write_roots` is set, only inside them. |
| `scoped_writer` | the Worker contract | Only the exclusive `files` it is given at spawn — which must lie inside `write_roots` when set. |

```text
agent maximum   = capability class ∩ tools ∩ write_roots   (agent.yaml)
spawn request   = agent + files                              (spawn_agent call)
effective       = what runtime admission allows ⊆ agent maximum
```

A spawn can repeat the agent's class (`role="explorer"` for a `read_only`
agent) but never change it: `spawn_agent(agent="security-reviewer",
role="default")` is refused.

Children never receive MCP tools or the agent authoring tools, and cannot
spawn children of their own.

## `instructions.md`

Role-specific guidance: what to look for, what to report, how to report it.
It is required, must be non-empty UTF-8 text, and is at most 64 KiB.

At spawn, the child's first system message is composed in this order:

```text
base prompt → project rules (AGENTS.md, .leveler/instructions.md) → capability-class text
→ agent instructions → each bound skill
```

The child's task stays its own user message. The main agent sees only names
and descriptions (a per-turn catalog capped at 4 KiB); full instructions reach
only the child.

**Instructions are not authority.** They shape how the agent works; they do not
change which tools it holds or which files it may write, and the child is told
so. Do not put runtime rules, tool schemas or permission mechanics in them —
the runtime enforces those — and **do not place credentials in agent
instructions**: they are sent to the model.

## Validity and availability

Every listing gives each agent one status:

| Status | Meaning | Spawnable |
| --- | --- | --- |
| `available` | Valid, and everything it names is configured here. | yes |
| `unavailable` | Valid, but its model is not configured, a skill is missing, or its effort is not offered. | no |
| `invalid` | The files themselves are wrong: YAML, schema, name, missing instructions, a symlink. | no |

One bad agent does not affect the others, and does not stop CodeLeveler from
starting. `leveler doctor` reports invalid definitions by name.

`available` is a configuration check: the model is configured, the effort is
offered, the skills are installed. It does not check that the model's provider
has a working API key; a pinned model whose key is missing fails at the child's
first request, with the provider's error. The Web UI therefore labels this
state "已配置" (configured), not "available".

## Using an agent

The main agent may choose an agent for part of a task; being listed only makes
an agent available, it does not make it used. You can also name one:
"let security-reviewer check this change". A name that does not resolve is an
error listing the available agents — it never falls back to a default child.

The harness's independent Reviewer is not an agent you can request, and an
agent whose name contains "reviewer" gains no special authority: its findings
are a child's result like any other.

## Running children keep their definition

When an agent is spawned, the resolved definition is recorded on the child's
durable start (name, source, fingerprint, class, effort, skills, write roots,
budget, model, tools), and its instructions and skills are part of its own
first system message. A restarted child continues under exactly that, and never
re-reads the files:

- editing an agent affects **new** spawns only;
- deleting an agent does not stop a running child, and its settled history
  still shows the agent's name;
- a child spawned read-only stays read-only after the file is changed to
  `writer`.

The **fingerprint** (`sha256:…`) covers every behaviour-shaping field and the
instructions, not the location, so the same directory copied into another
repository has the same fingerprint. It is for inspection and debugging, not a
signature.

The registry is re-read when needed (each spawn batch, each turn's catalog,
each listing); there is no file watcher and no restart required.

## Changing agents safely

Every surface writes through one store:

- **create** writes both files into a hidden staging directory, then renames it
  into place — the registry never sees half an agent;
- **update** stages the whole new directory, swaps it in, and removes the old
  one; a file whose content does not change is carried over byte for byte, so a
  hand-written `agent.yaml` keeps its comments when only the instructions
  change, and nothing is written when nothing changes. A UI save that does
  change the manifest rewrites it in schema field order;
- **delete** renames the directory away before removing it.

There is no rename operation: create the new name and delete the old one.
Built-in agents cannot be edited; copy one (`leveler agents show code-reviewer`,
or "duplicate" in the Web UI) into a project or user agent of another name.

The agent's own edit tools cannot patch `.leveler/agents/` in place
(`write_file`/`apply_patch` are refused there, like `.leveler/hooks.yaml`), so a
change made from the conversation always goes through this path. A shell
command is not covered by that refusal — the same limit as the hooks file —
and a definition written that way is still validated when it is loaded. The
refusal matches the directory case-insensitively, so `.LEVELER/Agents/…` is
refused too (on macOS and Windows it is the same directory).

From the conversation, `save_agent` and `delete_agent` validate the proposal
first — a contradiction such as a read-only reviewer asking for `apply_patch`
is refused without asking you — and then always ask you to confirm, in every
permission mode including full access. The confirmation shows the scope,
capability, write bounds, tools, model, effort, skills and instructions. A
session approval covers only that exact proposal. Nobody-present runs cannot
write agents. An agent write never becomes a standing permission rule, so the
approval prompt does not offer "always allow" for it (nor for `remember` /
`forget`).

## Trust

**Project agents are repository-controlled instructions.** Like `AGENTS.md`,
they come from whoever wrote the repository. Review `.leveler/agents/` before
using an untrusted repository. Whatever an agent's instructions say, its
authority is the capability class and bounds above; instructions cannot grant
tools or write scope.

User agents live outside the repository. Repository content cannot create
them: writing a user agent from the conversation needs your explicit
confirmation, and the Web UI writes one only on your action. Remote (mobile)
clients can see a running child's agent name but cannot list or change agents.

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

A marketplace or remote install, signed or archived agent packages, code or
WASM plugins, agents that spawn or talk to other agents, inheritance between
definitions (`extends:`), per-agent lifecycle hooks, remote instructions URLs,
agent-defined tools, a Mobile editor, and a file watcher.
