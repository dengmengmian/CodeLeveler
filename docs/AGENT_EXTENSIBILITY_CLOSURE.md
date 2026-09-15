# Agent Extensibility Closure

## Status

```text
AGENT_EXTENSIBILITY_CLOSURE=PENDING_CI
```

Implementation, mechanical tests and real-model dogfood are complete on the
local tree. The phase is not closed: the commits have not been pushed and no
CI run (Linux / macOS / Windows) exists for them — see residual 1. Nothing below claims a CI result.

**How does a user create a new agent?**

- **Anyone:** tell CodeLeveler "create a … agent for this project" and confirm
  the proposal it shows.
- **Web/Desktop:** open a conversation, then Settings → Agents → Create Agent.
- **Advanced:** write `.leveler/agents/<name>/agent.yaml` + `instructions.md`.
- **Teams:** commit the project's `.leveler/agents/` directory.

No Rust enum, no rebuild. Reference: [Custom Agents](AGENT_EXTENSIBILITY.md).

## Base

| Item | Value |
| --- | --- |
| Base head | `7e75a7e` (local `main`; see residual 1) |
| Version | `0.2.0-beta.2` — unchanged, no tag, no release |
| Worktree | detached worktree in the session scratchpad, no branch |
| Multi-agent capability closure | PASS (unchanged) |
| Auto-delegation value closure | BLOCKED (unchanged; this phase adds no delegation policy) |

## Architecture

```text
agent.yaml + instructions.md + skills
        ↓
Agent Registry           crates/leveler-agent/src/agent_registry   (Coding Harness)
        ↓
Spawn admission          executor/agent_spawn.rs → ChildProfile::admit_profile
        ↓
Existing durable multi-agent runtime (unchanged lifecycle, ownership, persistence)
```

The Engine gained no agent concept. The registry, its validation and the store
live in the harness crate; the app projects them to clients.

## Reality audit (before)

| Item | Found |
| --- | --- |
| Agent role model | `AgentRole {Default, Explorer, Worker, Reviewer}` + `ChildProfile` policies (tool, workspace, runtime, budget) |
| Named personas | single-file `.leveler/agents/<name>.md`, hand-parsed frontmatter, unknown keys ignored, unparsable `max_rounds` → 0, name check `[A-Za-z0-9_-]` |
| Authority defect | an explicit `role`/`profile` on `spawn_agent` overrode a persona's declared role (read-only persona spawnable as a writer) |
| Tool narrowing | `named_subset` silently ignored unknown tool names; no canonical tool-name list |
| Restart | role label + `ChildSpawnSpec{files, model, tools, max_rounds}`; profile re-resolved from the label; no agent identity, no effort |
| Skills | directory per skill, project > user > built-in, lax frontmatter |
| Model selection | inherit parent, optional persona pin with availability check; effort resolved per role, not per agent; effort not persisted |
| Surfaces | no client could list agents; child events carried only role/profile id |

Reused: `ChildProfile` and its policies, the ownership registry and
`claim_write_scope`, `pinned_model_refusal`, skills loading/rendering, the
approval pipeline, `LevelerHome`, the event log and `ChildSpawnSpec`.
New: agent registry + schema + store + authoring tools, a spawn snapshot, the
agent client protocol, `/agents`, `leveler agents`, Web Settings → Agents.

## File model and schema

| Item | Decision |
| --- | --- |
| Project root | `<repo>/.leveler/agents/<name>/` |
| User root | `<leveler-home>/agents/<name>/` |
| Files | `agent.yaml` (capability) + `instructions.md` (prompt), both required |
| Schema version | `version: 1`, required; other values refused |
| Name | `[a-z][a-z0-9-]*`, ≤ 64, no trailing `-`, no Windows device names, equals directory |
| Description | one line, ≤ 200 chars |
| Capability | `read_only` → Explorer contract, `writer` → Default (late-bound claims), `scoped_writer` → Worker (spawn `files`) |
| Model | `provider/model`, configured or unavailable, never substituted |
| Reasoning | exact supported effort or unavailable (no rounding) |
| Skills | installed or unavailable; bound into the child's first system message |
| Tools | known names only (`MODEL_SURFACE_TOOLS`), read-only list for `read_only`, no MCP, `[]` refused, every listed tool required at spawn |
| Workspace | `write_roots`, writer classes only, repo-relative, no `..`/globs/root |
| Budget | `max_rounds` 1–1000, `max_duration_secs` 1–1200 |
| Unknown fields | fail closed at every level (`deny_unknown_fields`); no credential field exists |
| Path security | agent directory and both files must not be symlinks; canonical containment; a project agents dir resolving outside the project loads nothing |
| Legacy `.md` | not read (user decision) |

## Registry

| Item | Decision |
| --- | --- |
| Owner | `leveler-agent::agent_registry` (harness) |
| Built-in source | compiled: structural roles projected from `ChildProfile`; personas as compiled `agent.yaml` + `instructions.md` through the same parser |
| Precedence | project > user > built-in; losers reported as `shadowed` |
| Reserved | `default`, `explorer`, `worker`, `reviewer` — runtime still has structural code paths for them; directories with those names are problems, not loaded |
| Invalid entry | recorded with reason, never spawnable, still shadows lower precedence (no fallback); other agents unaffected; product starts |
| Availability | `available` / `unavailable(reason)` / `invalid(reason)`; structural validity separate from machine availability |
| Order | sorted by name |
| Reload | read per spawn batch, per turn (catalog), per listing; no watcher, no restart |
| Reviewer | `reviewer` resolves as harness-only; a name containing "reviewer" gains nothing |

## Instructions

| Item | Decision |
| --- | --- |
| Separate file | yes |
| Composition | base prompt → project rules → capability-class text → agent instructions → bound skills; task stays the user message |
| Max size | 64 KiB, UTF-8, no NUL, non-empty |
| Authority | none; the child is told they do not change tools or write scope |
| Restart durability | part of the child's first system message, persisted in its transcript and reused on resume |
| Parent context | per-turn catalog of name + description + class + source, ≤ 4 KiB; full instructions only in the child |

## Runtime integration

| Item | Result |
| --- | --- |
| Legacy role/profile | unchanged; conflicting `profile` + `role` still refused |
| Custom spawn | `spawn_agent(agent="<name>")`; `profile_id` = agent name, `profile_role` = class role |
| Class conflict | a call may repeat the agent's class, never change it (the old override defect is fixed) |
| Effective scope | `write_roots` bound at spawn (`files`) and at `claim_write_scope`; ownership fence unchanged |
| Model override | pinned model through `pinned_model_refusal`, repriced |
| Reasoning override | exact effort applied to the child's requests; recorded per request (migration 0026) |
| Skills | loaded at spawn; missing → refused |
| Tools | class registry ∩ declared tools; harness `update_plan` kept; authoring tools never reach a child |
| Snapshot | `ChildSpawnSpec.agent: ChildAgentSnapshot{name, source, fingerprint, capability, reasoning_effort, skills, write_roots, max_duration_secs}` + spec `model/tools/max_rounds` |
| Restart | `child_for_spec` builds spawn and resume children from the same spec; files never re-read |

## Mutation and authoring

| Item | Result |
| --- | --- |
| Create | staging hidden dir, fsync, rename into place |
| Update | stage full dir, move old aside, rename in, remove old; unchanged files carried byte for byte; no write when nothing changes |
| Delete | rename aside, then remove |
| Conversation | `list_agents`, `save_agent`, `delete_agent`; proposal validated before any prompt; human confirmation in every profile incl. full access; no standing rule; session grant covers only the exact proposal; unattended runs refused |
| Edit tools | `write_file`/`apply_patch` refused on `.leveler/agents/**`, with a message naming `save_agent` (added after dogfood) |
| Clients | `CreateAgent`/`UpdateAgent`/`DeleteAgent` validated and written by the runtime; remote clients denied |
| User-global | written only by a confirmed tool call or the user's own client action |

## UI

| Surface | Result |
| --- | --- |
| Web list/create/edit/delete | Settings → Agents: grouped list, availability, create with confirmation summary, editor, delete, copy built-in persona; 30 new Vitest logic tests; protocol-level E2E against a real runtime (below) |
| Web visual flow | **not verified in a browser** by this session |
| TUI | `/agents`, `/agents <name>`; child roster shows the agent name; driven on a real PTY |
| Mobile | child row and sheet show the agent name and source; 3 new tests |
| CLI | `leveler agents list [--json]`, `leveler agents show`, `doctor` agents line |

## Compatibility

| Item | Result |
| --- | --- |
| Default / Explorer / Worker / Reviewer | existing `child_profile_spawn`, `multi_agent_test`, `ma_restart_truth_test` suites pass; structural built-ins project to identical `ChildProfile`s |
| Old session replay | new event/snapshot fields are `serde(default)` `Option`; legacy JSON parse test |
| Old spawn schema | `role`/`profile`/`files`/`agent` unchanged |
| Protocol | 1.7 → 1.8 additive; schemas and generated TS regenerated |
| Legacy persona files | `.md` no longer read; built-ins migrated to directories; `role` in a definition is now an unknown field |

## Dogfood

Real model `deepseek/deepseek-v4-flash` through `leveler web` (in-process
runtime, full access), driven over its HTTP + WebSocket protocol by a script
acting as the user (approvals answered with `approve_once` after reading the
preview). Temp `LEVELER_HOME`, fixture git repository. Evidence: event log
`sub_agent_*` rows, `model_requests`, files on disk, WS logs.

| Step | Result |
| --- | --- |
| Natural-language creation ("create a project-local agent named rust-test-reviewer … must never modify code") | model chose `save_agent`, `read_only`; preview shown; confirmed; valid `agent.yaml` + 2436-byte instructions; listed `available` |
| Spawn on a real test change | `sub_agent_started.spec.agent = {rust-test-reviewer, project, sha256:56e9…}`; instructions in child system message exactly once; child tools used: `git_status, git_diff, read_file, report_finding`; found the tautological assertion at `src/lib.rs:26`; settled `completed_with_findings`; no file touched |
| Edit by conversation | **defect found:** model edited `instructions.md` with `apply_patch`, bypassing validation and confirmation → fixed (`2289bf4`, `db533a7`); rerun: `apply_patch`/`write_file` refused, `save_agent` confirmed, `agent.yaml` bytes unchanged, instructions updated |
| New spawn after edit | new fingerprint `sha256:f985…`; new instructions in the new child; old child record keeps the old ones; report ends `Confidence: high` |
| Delete by conversation | confirmed; directory gone; `agents show` → not found; both earlier sessions' snapshots still show `rust-test-reviewer` on their settled children |
| Scoped writer (`frontend-worker`, `write_roots: [web]`) | claim `[web/app.ts, src/lib.rs]` refused ("outside this agent's write roots"); claim `web/app.ts` granted; patch applied; `src/lib.rs` unchanged; `turn_completed` |
| User agent + precedence | user `rust-explorer` spawned with `source: user`; project `rust-explorer` added → `shadows user`, spawn `source: project`; a second repo sees only the user one |
| Missing agent | "Agent "security-auditor" not found. Available agents: …"; the model then used `code-reviewer` and said so to the user |
| Pinned model + effort | child requests recorded `deepseek-v4-pro` / `high` (3), parent `deepseek-v4-flash` / `max`; an agent asking `minimal` is `unavailable` with the supported set |
| Web protocol E2E | create → contradictory draft refused (`apply_patch` on read_only) → get → update to writer + `web` → delete; files match each step |
| TUI | real binary on a PTY (`--in-process`): `/agents` and `/agents frontend-worker` render sources, shadowing, unavailability and the definition |

```text
AGENT_CREATED_BY_NATURAL_LANGUAGE=YES
AGENT_FILES_CORRECT=YES
PROFILE_VALIDATED=YES
INSTRUCTIONS_LOADED=YES
HARD_PERMISSIONS_ENFORCED=YES
CUSTOM_AGENT_SPAWNED=YES
RESTART_SAFE=YES (mechanical: ma_restart_truth_test; not exercised with a real model)
EDIT_RELOAD=YES
DELETE_SAFE=YES
```

## Residuals

Non-blocking for the implementation; the first is blocking for closure.

1. **Not pushed, no CI.** Local `main` in the shared checkout carries 11
   commits from another session not on `origin/main`, and `origin/main` has
   `85c59cd`, which local `main` lacks. Pushing these commits would publish or
   rewrite that work. The Linux/macOS/Windows CI run, including Windows path
   and junction behaviour, is outstanding.
2. **Web UI not seen in a browser.** Logic tests, typecheck, build and a
   protocol-level E2E against a real runtime pass; the panel, forms, dialog
   stacking and the narrow rail have not been looked at.
3. **Shell commands can still write `.leveler/agents/`.** The edit-tool refusal
   does not cover `run_command`/`shell_command` — the same limit as
   `.leveler/hooks.yaml`. A definition written that way is still validated on
   load.
4. **Approval wording for consent tools.** The TUI's "始终允许（写入项目规则）"
   option persists no rule for `save_agent`/`delete_agent` (nor for
   `remember`/`forget`) and grants only the exact action for the session.
5. **Availability does not check provider API keys**, only that the model is
   configured.
6. **Legacy `.md` persona files are ignored silently** (no diagnostic), per the
   decision to drop the format.
7. **Restart immutability is proven mechanically only** (`ma_restart_truth_test`),
   not with a real model killed mid-child.
8. **Model substitution with disclosure:** asked for a missing agent, the model
   used a similar one and said so. The runtime never falls back.
9. **Web:** the Agents panel needs an open conversation; an editor can briefly
   show a cached definition. The Vite bundle exceeds 500 kB (warning).
10. **Observed load flakes, not caused by this work:** `tui_path_soak`,
    `reaper_authority`, `run_command::a_valid_call_still_runs` each failed once
    under full parallel load and passed on rerun; the four
    `command::tests` cargo-wrapper tests fail only when `CARGO_TARGET_DIR`
    points outside the test workspace.

## Final gates

```text
Quality (local, HEAD f12c04f)
  git diff --check              ok
  cargo fmt --check             ok
  cargo clippy -D warnings      ok
  cargo test --workspace        4070 passed, 0 failed, 20 ignored
  web check:protocol            in sync
  web typecheck                 ok
  web tests                     228 passed (20 files)
  web build                     ok (bundle size warning)
  flutter analyze               no issues
  flutter test                  81 passed
  cargo deny check              advisories ok, bans ok, licenses ok, sources ok

Safety gates (existing suites green)
  FALSE_VERIFIED_TOTAL=0  OWNERSHIP_VIOLATION=0  LOST_ACCEPTED_CHILD=0
  DUPLICATE_SETTLEMENT=0  OPEN_ORPHAN=0          BUILTIN_REGRESSION=0

AGENT_DIRECTORY_MODEL=PASS
AGENT_YAML_SCHEMA=PASS
AGENT_INSTRUCTIONS_SEPARATE=YES
BUILTIN_AGENT_REGISTRY=PASS
USER_AGENT_DISCOVERY=PASS
PROJECT_AGENT_DISCOVERY=PASS
PRECEDENCE=PASS
INVALID_AGENT_FAILS_CLOSED=YES
AGENT_PATH_SECURITY=PASS
AGENT_SKILL_BINDING=PASS
AGENT_MODEL_SELECTION=PASS
AGENT_REASONING_SELECTION=PASS
AGENT_TOOL_ADMISSION=PASS
AGENT_WORKSPACE_AUTHORITY=PASS
PROMPT_ONLY_PERMISSION_ENFORCEMENT=NO
SKILL_CAN_EXPAND_RUNTIME_AUTHORITY=NO
PROJECT_CONTENT_CAN_SILENTLY_CREATE_USER_AGENT=NO
PARTIAL_AGENT_VISIBLE=NO
AGENT_UPDATE_ATOMIC_ENOUGH=YES
NEW_AGENT_REQUIRES_RUST_ENUM_CHANGE=NO
AGENT_DISCOVERY_CONTEXT_BOUNDED=YES
AGENT_REGISTRY_REMOTE_READ=PASS
CUSTOM_AGENT_SPAWN=PASS
RUNNING_CHILD_PROFILE_IMMUTABLE=YES
RESTART_USES_SPAWN_TIME_PROFILE=YES
CHILD_INSTRUCTIONS_RESTART_STABLE=YES
NATURAL_LANGUAGE_AGENT_CREATION=PASS
WEB_AGENT_MANAGEMENT=PASS (protocol E2E + logic tests; visual flow unverified)
TUI_AGENT_DISCOVERY=PASS
MOBILE_CUSTOM_AGENT_PRESENTATION=PASS
BUILTIN_AGENT_BEHAVIOR_PRESERVED=YES
REAL_AGENT_EXTENSIBILITY_DOGFOOD=PASS
CI=NOT_RUN
AGENT_EXTENSIBILITY_CLOSURE=PENDING_CI
```
