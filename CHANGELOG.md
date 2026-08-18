# Changelog

All notable changes to CodeLeveler are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) (0.x: minor bumps may break).

## [Unreleased]

### Changed
- Empty-session splash is a terminal-native hero card: colored red-panda
  mascot, product mission, and three first-run commands (`/feature-dev`,
  `/model`, `/help`). `/plan` stays a slash command, not an onboarding step.
- Reasoning effort is resolved in one place: model `supported_efforts` +
  CodeLeveler `default_effort`, then user override, then upward-or-clamp
  normalization. Protocol adapters encode the effective value only.
- Input-border status is `{model} [(effort)] · work-mode · permission · session`.
  The model drops the provider prefix unless names collide; reasoning effort
  is the runtime-projected **effective** value, never a TUI guess.
- A bare `/` popup lists only Quick commands (about a dozen everyday
  entries). The rest stay typed-prefix searchable; `/help` still lists all.
- Slash commands with a fixed option set (`/work-mode`, `/collab`, plus the
  existing `/model` `/permission` `/theme` pickers) open the shared selector
  instead of showing CLI-style arguments. The input chip is now
  `model · work-mode · permission · session`.
- `/feature-dev` popup label is now the capability name (功能实现 /
  feature implementation), not the skill's workflow description.
- Slash-command popup rows are laid out in terminal cells (command column +
  gap + clipped description). Conversation CJK underneath is cleared so
  descriptions no longer lose every ideograph after the first.
- TUI themes are a semantic token system (`surface` / `text` / `border` /
  `accent` / `status` / `diff`) with an opaque canvas. Body text is no longer
  `Color::Reset`, so readability does not depend on the terminal profile.
  Default id is `auto` (detect polarity, then load Dark or Light).
  Wire values are `auto` / `dark` / `light` / `high-contrast` only.
- Executor plan gate (C2.3A): a missing structured plan no longer strips
  navigation tools or forces `ToolChoice=update_plan` after explore rounds.
  Multi-step tasks still require `update_plan` before mutations; after a few
  plan-less explore rounds the drive injects a one-time soft advisory only.
  Round budget, loop guard, and search budget are unchanged.

### Security
- In-repo `.leveler/hooks.yaml` and `.leveler/permissions.yaml` no longer take
  effect just because a repository ships them. Hooks run commands before every
  tool call and an in-repo `Allow` rule short-circuits the approval policy, so
  cloning an untrusted repository was enough to execute its commands or grant
  it standing permission. Both are now ignored — with a stderr notice — until
  `leveler trust` records the SHA-256 of the exact contents the user accepted;
  any later edit drops the file back to untrusted. Global
  `~/.leveler/hooks.yaml` / `permissions.yaml` are unaffected.
- The workspace layer refuses writes to those two in-repo files, so the agent
  cannot grant itself hooks or standing permissions. Reads are unchanged.

### Added
- Built-in reasoning profiles now declare `supported_efforts` and a
  CodeLeveler `default_effort` (distinct from the provider default).
  `leveler models show` / `leveler doctor` print the resolved matrix.
  CI fails if a reasoning builtin forgets either field (always-on no-knob
  models use `style: none` instead of inventing a level).
- TUI themes `auto`, `dark`, `light`, and `high-contrast`, with automated
  contrast-ratio gates in CI and `leveler theme preview`.
- `leveler trust` (`allow [--yes]` / `show` / `revoke`) to manage in-repo
  configuration trust per repository.
- TUI surfaces ignored in-repo config directly: the empty-session splash names
  the file and the command that enables it, and the composer border carries a
  marker for as long as the condition holds. The CLI's stderr notice never
  survived the alternate screen.

## [0.1.4] - 2026-07-25

### Added
- Proactive project memory pipeline: system-side candidates from explicit user
  intent (`记住：…` / `remember: …`) and package-manager signals
  (`pnpm-lock.yaml` / `yarn.lock` / `package-lock.json` / `packageManager`).
  Candidates land in `pending/` and become durable only after user accept
  (`leveler memory accept`); reject suppresses re-prompt for the same signal.
- CLI: `leveler memory pending|accept|reject|propose` (plus existing
  list/search/show/forget/remember).
- Turn-start host enqueue of memory candidates (never writes active without
  consent). K36: agent `remember`/`forget`/`consolidate_memory` remain
  approval-gated; accept is not an agent tool.
- Structured memory entry fields (`key` / `kind`) for package-manager and
  preference upserts.
- TUI: clickable http(s) URL detection/styling for transcript links.
- Executor plan-gate repair: force `update_plan` when a structured plan is
  still required after explore rounds.
- Task reports propagate executor `stop_detail` through engine → app for
  clearer incomplete / budget messages.

### Fixed
- Incomplete/budget stop reasons keep the concrete executor detail instead of
  dropping it when mapping reports to runtime events.

## [0.1.3] - 2026-07-25

### Added
- Out-of-the-box multi-agent: concurrent sub-agents emit live attributed tool
  activity; the TUI shows each child's current step and elapsed time.
  `agents.delegation` can hide `spawn_agent` when multi-agent is not wanted.
- Command-progress heartbeat and live elapsed time on running command blocks
  in the TUI (and CLI event renderers for `SubAgentActivity`).
- Three-layer agent budget control: telemetry, sized quotas, and bounded
  extend so hard cases stop for budget reasons instead of silent starvation.
- Early phase progress on the eval path so TTFF is host-side, not first LLM
  token; silent-duration metrics accompany it.
- Eval quality-gate tiers (`quick` / `daily` / `release`), scenario cases, and
  trend reporting via `leveler eval`.
- `find_files` tool (consolidates former `glob` + `repository_search`).
- Interactive chat baseline anchoring and project-gate-only completion
  verdict (`Verified` only when the project's own checks pass).

### Changed
- Completion closeout drops `MissingEvidence` / answer-audit guessing; the
  verdict is driven only by gating checks that actually ran (or by the user).
- Tool surface hardened: shared replace/apply_patch CAS commit, conservative
  fuzzy replace fallback, CRLF/BOM-safe patching, shell/credential path
  refusals shared with `read_file`.
- Security semantics: MCP tools prompt under Assisted/RequestApproval;
  explicit `read_only_subset` allowlist; sensitive paths enforced at every
  layer; opaque shells (`pwsh`/`powershell`/`fish`) classify Dangerous.
- TUI edit diffs show line numbers with cleaner add/remove gutters.
- Orchestrate no longer false-fails on already-green workspaces.

### Fixed
- Windows `replace` permissions mapping for cap-std / checkpoint restore
  (CI clippy and unit tests green).
- Long-running commands no longer look "blocked" while still producing
  heartbeat progress.
- Permission-rules poison recovery and related hygiene cleanups.

## [0.1.1] - 2026-07-21

### Added
- `leveler init`: interactively create `~/.leveler/config.toml` (refuses to
  overwrite; prints a template when not a TTY). Startup itself never writes
  config. The "no models configured" error now points at it.
- Tag-triggered release workflow: three-platform binaries (Linux x64, macOS arm64, Windows x64) with `.sha256`
  checksums attached to a draft GitHub release.
- `leveler upgrade` verifies the release asset against its published sha256
  before installing, and refuses releases without a checksum.
- `leveler resume --confirm-recovery`: the explicit reconciliation flow for a
  crash-recovery stop — closes interrupted tool calls with a user-acknowledged
  marker after the workspace has been inspected.
- Headless `leveler run` gets a default 1-hour wall-clock ceiling
  (`limits.max_duration_seconds` overrides); interactive runs remain
  until-terminal.
- Orchestrated nodes enforce their declared `max_duration` budget.
- Provider `Retry-After` is honored on 429/5xx (capped at 120s); the agent
  retry loop backs off rate limits on second scales with jitter.
- Offline eval smoke test drives a real smoke case end-to-end with a mock
  model in CI — no API key required.

### Changed
- Cancelling mid tool batch now commits completed tool results and spend
  before surfacing the cancellation; unfinished calls are refused in place.
- Rounds in which every tool call was refused count toward the no-progress
  hard stop instead of resetting it.
- Orchestrate resume merges the context snapshot with messages persisted
  after it, instead of replacing them.
- Provider/gateway glitches (`tool_calls` without calls, `stop` alongside
  calls, `length`-truncated tool calls) recover with bounded feedback retries
  instead of aborting the turn.
- Token estimation weighs non-ASCII (CJK) text at ~1 token per character;
  token budgets bind even when a gateway reports no usage.
- Engine pre-request compaction now asks the model for a handoff briefing
  instead of always folding with a bare breadcrumb.
- `git push` / `cargo publish` no longer prompt under the Assisted profile
  (sandbox-first); unattended acceptance checks still refuse them.
- Default provider retry attempts raised from 2 to 4.
- Replaced unmaintained `serde_yaml` with `serde_yaml_ng`.

### Fixed
- Background `run_command` tasks (dev servers, watchers started with
  `background=true`) now survive across turns. The process-lived task registry
  was rebuilt per turn, so its `KillOnDrop` reaped every background process at
  turn end and the next turn no longer knew the task id.
- `run_command` called without a `program` now returns actionable guidance
  (use `shell_command` for a whole command line) instead of a bare
  "program is a required field" schema rejection.
- Windows CI is green: platform-specific test assumptions (POSIX
  coreutils/shell fixtures, path-separator and `\\?\` canonicalization) were
  corrected; no product behavior changed.
- A checkpoint recorded while the database was unavailable could restore to
  an empty conversation; it is now skipped with a warning.
- Workspace snapshot restore surfaces file-deletion failures instead of
  silently leaving the tree inconsistent.
- Large file originals no longer stay resident in memory for checkpoint
  rollback; they spill to disk.
- Event-log lookups used during turn seeding are indexed single-row queries
  instead of full-log scans.
- Linux children receive SIGTERM when the parent dies (`PR_SET_PDEATHSIG`),
  so a force-killed session no longer orphans grandchildren.
- `ProviderConfig`'s `Debug` output redacts the API key.
