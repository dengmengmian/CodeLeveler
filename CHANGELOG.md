# Changelog

All notable changes to CodeLeveler are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) (0.x: minor bumps may break).

## [Unreleased]

### Added
- `leveler init`: interactively create `~/.leveler/config.toml` (refuses to
  overwrite; prints a template when not a TTY). Startup itself never writes
  config. The "no models configured" error now points at it.
- Tag-triggered release workflow: four-platform binaries with `.sha256`
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
