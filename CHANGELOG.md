# Changelog

All notable changes to CodeLeveler are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) (0.x: minor bumps may break).

## [Unreleased]

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
