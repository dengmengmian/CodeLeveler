# Changelog

All notable changes to CodeLeveler are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) (0.x: minor bumps may break).

## [Unreleased]

### Changed
- **A context snapshot is persisted only when the context diverges from the
  transcript.** The drive loop wrote `ContextSnapshot` — the whole model
  context — after every round with a next round. Each one was cloned,
  serialized, scanned for secrets, and fsynced, so a long turn wrote the
  transcript back to the event log once per round and the log grew with the
  square of the turn. Almost all of it was reconstructible: the loop already
  persists its nudges, settlements and directives through the transcript
  sink. Only a compaction fold or a transient injection puts something in the
  context that the transcript does not hold, and only those now snapshot.
  Scoped `AGENTS.md` rules join the transcript like every other injected
  message. `leveler eval` sets the new `context_trace` override, which
  restores the per-round copy that `scripts/analyze_context.py` reads.
- **Turn-start reconciliation reads the event types it pairs on.** Finding
  ghost children, dangling tool calls and a session's reviewer each scanned
  and decoded the whole event log, which included every context snapshot
  above. They use the indexed by-type query and its existing
  `(session_id, type, sequence)` index instead. Behaviour is unchanged; a
  corrupt row of a type a scan depends on still fails closed with the same
  provenance.
- **A chat turn summarizes only the context it is about to fold.** Over the
  pre-request threshold, every chat, resume and goal-continuation turn made a
  summarization call before assembling. Assembly then usually found that the
  latest snapshot plus its tail already fit and discarded the summary — so a
  long session paid for a large model call per turn and threw the result
  away. The briefing is now produced through a `ContextSummarizer` that
  assembly consults only when the merged context is still oversized, over the
  messages actually being folded.
- **`read_file` pages at the budget the registry enforces.** The tool built up
  to 256 KiB and the central cap then cut the result to the turn's budget,
  keeping head and tail. The surviving paging marker said `start_line=N` for
  an `N` past the elided middle, so the model's next page skipped lines it had
  never seen. The tool now pages at the effective budget, so its own marker is
  the only truncation and the pointer names the first unread line.
- **Only the leading system block reaches Anthropic's `system` field.** The
  adapter hoisted every `Role::System` message. The drive loop appends
  standing constraints (nested rules, memory recall) at the tail precisely to
  keep the cached prefix intact, and hoisting them rewrote that prefix on
  every discovery. A later system message is conversation and stays in place.
- **An event append no longer reads its own row back.** `append_owned` did an
  INSERT and then a SELECT; the INSERT returns the row.

### Added
- **Deterministic tool-result trimming before a fold** (`prune_tool_results`,
  off by default). When the context is over budget, the middle of oversized
  tool results that have left the working set can be trimmed in place — no
  model call, no message dropped, every `tool_result` keeping its `call_id`
  and error flag. When that alone brings the request under budget the fold,
  which rewrites the prefix and pays for a summary, does not happen. The value
  on real workloads is unmeasured (C2.2 found the lossless variant reclaimed
  almost nothing), so it ships behind the `prune_tool_results` eval knob until
  an A/B says otherwise.

### Fixed
- **`leveler upgrade` understands pre-releases.** Publishing `0.2.0-beta.1` and
  then running the binary exposed four defects with one root cause: the version
  type discarded any `-suffix` at parse time, and it was used for ordering, for
  display, and for building the release asset's file name.
  - *Ordering.* SemVer puts `0.2.0-beta.1` before `0.2.0`; the parser made them
    equal. When `0.2.0` shipped, **no beta user would ever have been offered
    it** — `should_upgrade` would say no and `--force` would be the only way
    off the beta. Precedence now follows SemVer §11, including numeric
    identifiers comparing numerically (`beta.9` before `beta.10`).
  - *Display.* `upgrade --check` told a `0.2.0-beta.1` user they were on
    `v0.2.0`, contradicting `leveler --version`.
  - *Asset lookup.* The download path looked for `leveler-v0.2.0-…`, which does
    not exist, found nothing, and silently fell back to compiling from source —
    on a machine that may have no Rust toolchain.
  - *Flag capture.* The build-provenance shortcut ran before `clap` and matched
    `--version` **anywhere** on the command line, so
    `leveler upgrade --version <TAG>` printed the binary's own version and
    exited instead of installing that tag — breaking the only documented way to
    install a pre-release from the CLI, on the release that introduced
    pre-releases. A regression against `0.1.4`, which handles it correctly. The
    provenance line is now `clap`'s own `version`, so `clap` decides which
    `--version` belongs to which command instead of a scan of the raw argument
    list guessing at it.

## [0.2.0-beta.1] - 2026-08-22

First public pre-release. Published as a GitHub **pre-release**, so `brew`,
`install.sh` and `leveler upgrade` keep serving the latest stable (`0.1.4`)
unless a beta is asked for by name (`LEVELER_VERSION=v0.2.0-beta.1`).

**macOS and Linux binaries only.** The Windows target is built and its build,
lints and security canaries pass, but three of its tests do not, so the artifact
is deliberately not published. Windows stays on stable `0.1.4`.

**Known limitations** — these are stated, not hidden:
- **Delegation is opportunity-based.** The runtime executes a delegation
  reliably whenever the model elects to collaborate; it does not promise that
  the agent splits a task on its own. Measured adoption on qualified tasks is
  10–30 %. See [`docs/ROADMAP.md`](docs/ROADMAP.md).
- **Windows has no daemon socket transport.** Sessions and `resume` work;
  `leveler serve` / `web` / `remote projects` / `remote agent` refuse with a
  clear message rather than pretending. `!command` output for a *confined*
  Windows command arrives on completion instead of streaming live.
- **Three Windows tests fail on `main`** (`command_delivery`, `user_shell`,
  `side_effect_barrier_test`). The Windows build, clippy and security canaries
  are green. Diagnosis needs a Windows machine — see
  [`docs/BETA_BLOCKER_RESOLUTION.md`](docs/BETA_BLOCKER_RESOLUTION.md) risk 0b.
- **Long-running goals and structured sub-agent workflows are post-Beta.**
  Durable child sessions, a delegation advisor and capability negotiation are
  designed but not shipped.

### Added
- Mobile Beta MVP (`apps/leveler-mobile`, tag `mobile-beta-mvp`): workspace
  Home, agent timeline, `steer_current_turn` while a turn is running,
  artifact cards/preview, and signed `fetch_attachment` (sha256 → media
  store, no public URL). Task Detail is a projection of the open session.
  **Further Mobile feature work is frozen** until real Beta users have used
  this loop; see `docs/MOBILE_FREEZE.md`. Security and pairing fixes remain
  in scope.

### Fixed
- **The release pipeline builds again.** `release.yml` copied a root `NOTICE`
  into all four archives; `NOTICE` had been removed on 2026-08-05 by a sweep of
  unreferenced root files. Because the release workflow only runs on a tag,
  nothing caught it for seventeen days — it surfaced as four failed builds the
  moment a release was wanted. `NOTICE` is restored (it carries the Apache-2.0
  attribution for the sandbox policy adapted from openai/codex, which
  `evals/THIRD_PARTY.md` still promised was there, and which every published
  release through `v0.1.4` shipped), and `scripts/check_release_payload.sh` now
  runs on every push to assert that every file `release.yml` packages exists and
  that both platforms ship the same list.
- **Windows builds again.** Four defects had accumulated behind `cfg(windows)`,
  none of them visible to a macOS or Linux compiler: `run_windows_dispatch` was
  called with the `chunks` argument the `!command` streaming work added to the
  Unix path only; `replace`'s non-Unix commit path still reached for
  `context.checkpoint` after that field moved under `context.execution`; the
  Unix-socket stub in `leveler-local-transport` carried a copy of the real
  `local_waiter_count` body, referring to types the stub does not have; and the
  Unix-only `ProjectRouter` was imported unconditionally by `leveler remote`.
  `leveler remote projects` / `agent` now refuse with a clear message on a
  platform without the daemon socket instead of failing to compile.
- **`!command` streams live on Windows too**, on the unconfined path. A
  confined Windows command (AppContainer) still delivers its output on
  completion — stated in the dispatch and in `README.md` rather than left to be
  discovered.
- **A finished `!command` no longer leaves the session briefly busy.** The
  runtime released the session's turn slot *after* publishing
  `UserShellExited`, so a client that enables its composer on that event could
  have the next message refused with "session … already has an active turn".
  The slot is released before the event is published. Reproduced on Linux,
  where the window is wide enough to fail reliably.
- After a user denies a permission request, the harness no longer auto-nudges
  or `DriveGoalAgain` past that boundary. The agent can still adapt with
  already-available tools; same or broader elevations are not re-prompted
  in the task epoch. Goal mode that cannot proceed becomes `Blocked`, not
  an 8-minute “等待模型” spin.
- `/web` in the default daemon-backed TUI now opens the Web UI against the
  same local runtime the TUI already has (Unix socket → daemon). It no
  longer requires `--in-process` or `leveler web --connect`, and no longer
  calls a local daemon a "remote daemon".

### Changed
- Sandbox tests assert the backend the host actually gets: the SBPL policy tests
  are macOS-only, `bwrap` argv has its own, and one new test pins which backend
  each platform selects. The harness read seal (`READ_DENIED_*`) is macOS-only
  today, and a Linux test now says so out loud instead of leaving it implied.
- `cargo deny check` passes: `leveler-session-wire` was a path dependency
  without a version (a wildcard under `wildcards = "deny"`), and the
  `RUSTSEC-2023-0071` ignore no longer matched any crate in the graph.
- Interface stability is **ADOPTED** rather than draft
  (`docs/STABILITY.md`), and linked from `README.md`: `run` / `resume` / `tui`
  and the setup commands are Frozen; `serve` / `web` / `lsp` / `mcp` / `login` /
  `logout` / `completions` / `trust` are Provisional; `eval` and `remote` are
  Unstable.
- Pre-releases have a channel of their own: `install.sh` takes
  `LEVELER_VERSION` to pin a tag, and the release workflow marks any tag with a
  SemVer pre-release part as a GitHub pre-release, so `releases/latest`, the
  installer and the Homebrew tap keep serving the last stable build.
- The quick start uses `leveler login` — the onboarding the binary already
  ships, which asks the provider which models the key can reach — with the
  hand-written `config.toml` kept as the explicit alternative.
- Empty-session splash is a terminal-native hero card: the Level Mark
  (Master / Compact / Micro), product mission, and three first-run
  commands (`/feature-dev`, `/model`, `/help`). `/plan` stays a slash
  command, not an onboarding step. Brand geometry lives in
  `crates/leveler-tui/src/brand.rs`; see `docs/TERMINAL_BRAND.md`.
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
  `accent` / `status` / `diff` / `brand`) with an opaque canvas. Body text is no longer
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
