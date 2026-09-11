# Changelog

All notable changes to CodeLeveler are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/) (0.x: minor bumps may break).

## [Unreleased]

### Added
- **The model-visible tool surface is composed, not inherited.** A turn gets
  the core primitives, plus the optional capability packs this host can
  actually offer, plus the harness controls its situation allows, plus MCP
  extensions. Each pack's condition is a mechanical fact — a search key is
  configured, the model's profile declares vision, Node is on `PATH` so the
  browser driver can start — never a judgement about the task or the model.
  `WorkProfile::Economy` composes no pack at all.
- **`write_file(path, content)`** — the canonical whole-file write, on the
  full tool surface. Creating or deliberately replacing a whole file is a
  different intent from changing part of one. It goes through the same guarded
  editor as `apply_patch`, so write scope, compare-and-swap, checkpoint and
  rollback are identical, and overwriting a file that changed since it was
  read is refused.
- **Unified Dogfood Acceptance V1** (`docs/evaluations/`) — a thin release
  gate: seven committed cases, one revision, one model, and a list of
  mechanical counters that must be zero before a baseline is frozen. No eval
  framework, no `eval_mode`, no dogfood prompt or budget; it runs
  `leveler eval run` over case YAML and reads the durable record back. Its
  first run failed on the defect above; the accepted run and the new Beta
  baseline are recorded in `docs/BETA_BASELINE_POST_CLOSURE.md`.
- **Every model call the runtime makes on its own account is recorded.** The
  runtime's own calls wrote nothing to `model_requests`, so a goal session's
  reported cost was short by all of them. They now write the `advisory` lane,
  including on the paths that spend tokens and then fail — a reply that could
  not be parsed is still a reply the provider billed. (The two callers that
  motivated this, contract derivation and the reconciliation judge, were
  themselves deleted in this release; compaction folds remain.)

### Changed
- **`web_search` is Tavily, and `LEVELER_SEARCH_API_KEY` now holds a Tavily
  key.** The tool used to carry two backends (Bing Search and Google Custom
  Search) behind a provider switch. It now makes one request to one API.
  **Migration:** put a Tavily key in `LEVELER_SEARCH_API_KEY` — the variable
  name is unchanged, its meaning is not. `LEVELER_SEARCH_PROVIDER` and
  `LEVELER_SEARCH_CX` are gone and are ignored if still set; a Bing or Google
  key left in `LEVELER_SEARCH_API_KEY` will now fail with an HTTP 401 from
  Tavily. The model's tool contract is untouched: `query` + `count`, default 5,
  max 10. Configuration has a single owner — the host reads the key once, a
  blank value counts as unset, and that one answer decides both whether
  `web_search` is advertised and what it is built with. A host without a key
  registers no `web_search` at all, so the tool no longer checks at call time
  whether it was configured, and no longer advises the model on what to do
  instead when a search fails.
- **The system prompt is a contract, not an operating manual.** It carries
  identity, the authority boundary, runtime state, harness protocol and product
  constraints. It no longer carries a method: when to make a plan and how to
  keep it synchronized, a required verification step before completion, a
  progress-narration template, which tool to prefer for which shape of work,
  how to investigate a question, how to diagnose a failure, when to persist and
  when to stop retrying. 130 lines became 50.
- **Plan capability, not plan enforcement.** `update_plan` is available every
  turn and its state is still persisted and rendered. What is gone is the
  task-complexity classifier that decided a request "needs" a plan, the
  injected reminder that followed, and the reminder that a plan had stopped
  tracking the work. Whether a plan helps is the model's judgement.
- **`apply_patch` matches exactly.** Its description always said the context
  and removed lines must match the file EXACTLY; the matcher had four fallback
  passes below exact. Because a located hunk is applied by splicing the
  caller's lines over the file's, a loose match rewrote real bytes on context
  lines nobody asked to change — trailing whitespace stripped, typographic
  quotes flattened to ASCII, spacing reformatted, inside a call that reported
  success. An inexact patch now fails, the file is untouched, and the error
  shows what the file really contains.
- **`get_task` / `wait_task` / `kill_task` are core.** `run_command` can start
  a background task, and a task the caller cannot observe or stop is an orphan.
- **Sub-agent and budget advisories state mechanics only.** The delegation hint
  describes concurrency, child isolation, the ownership fence and when a
  background call blocks; the budget note states the position and the operation
  that reports being blocked.

- **The harness exposes capability; it does not emulate intelligence.**
  Flattening model capability differences is withdrawn as a product goal. The
  model owns reasoning, the harness owns domain semantics and capability
  exposure, the runtime owns mechanical correctness — see
  `docs/ARCHITECTURE.md` §1.1. Runtime reliability and provider compatibility
  are separate concerns and are unchanged.
- **The seven core primitives are `read`, `ls`, `find`, `grep`, `edit`,
  `write`, `bash`,** and the four read primitives now mean the same thing on
  every machine. `read_file` no longer refuses a file for its size — any file
  is readable in bounded windows — and reports invalid UTF-8 instead of
  handing the model replacement characters. `grep`'s pattern is always a
  regex, with `literal` and `ignore_case` as parameters, and no longer
  degrades to a substring scan when `rg` is missing. `find_files`'s pattern is
  always a glob, using the gitignore/ripgrep anchoring rule, and its candidate
  set no longer changes with whether Git is installed. `list_files` lists the
  direct children of one directory and hides nothing. None of the four shells
  out any more; all four are now replay-safe.
- **`replace` no longer writes text you did not ask for.** Its fuzzy fallback,
  which matched a near-miss string and could not report where it wrote, is
  removed: a near miss is refused and the real text at the anchor is shown.
- **Background tasks are settled by the runtime, not by `wait_task`.** When a
  background process exits, the runtime diffs the workspace and — only under
  an explicit write allowlist — restores what the task was not allowed to
  touch. Enforcement no longer depends on the model choosing to wait.
  Dev-server safety is unchanged: a default background task is accounted and
  never rolled back.
- **The agent loop is a reusable kernel.** `leveler-agent-core` now owns the
  one generic model↔tool loop — rounds, the model round with its retries,
  admission against round/token/cost/duration limits, cancellation, the
  deadline, usage accounting, and semantically neutral stop reasons. It
  depends on `leveler-model` and nothing else in the workspace, and
  `cargo run -p leveler-agent-core --example minimal` runs it with no
  repository, database, or configuration. `leveler-agent` keeps everything
  that makes the loop a *coding* agent and drives it as a harness rather than
  maintaining a second loop; the ToolHost admission pipeline is untouched and
  is still the only authorization point. See `docs/AGENT_KERNEL.md`.
- **Every root directory has one owner.** SQLite migrations moved to
  `crates/leveler-storage/migrations/` (contents byte-identical, so an
  existing database is unaffected), evaluation fixtures to `evals/fixtures/`,
  the evaluation tools to `evals/scripts/`, and the two release guards CI runs
  to `packaging/scripts/`. `leveler-session-wire` folded into
  `leveler-client-protocol::session_wire`: it had no independent security,
  process, release, or compatibility boundary, and all four of its consumers
  already depended on the protocol crate.
- **Task outcome and verification are orthogonal.** `TaskOutcome` is now
  `completed | blocked | budget_limited | failed | interrupted` (legacy
  `verified` / `completed_unverified` read as `completed`), and a new
  `VerificationStatus` (`passed | failed | not_run | unavailable`) travels on
  `TaskFinished`, on the session row (`sessions.verification`, migration
  0023) and on `UiCompletionReport.verification`. Clients gained
  `turn_completed_checks_failed`; `TurnProgress` lost `closeout_deny_rounds`;
  observability lost `repair_started` / `repair_attempts`.
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
- **A turn reads the transcript it can reach, not the whole session.** Every
  chat, resume and goal-continuation turn loaded and deserialized the entire
  `session_messages` table, even when a watermarked context snapshot meant
  only the tail after it could ever be sent. The cost grew with the session on
  every turn, so the oldest sessions paid the most — pure latency, no tokens
  and no behaviour change, which is why nothing surfaced it.

  A load is bounded only when both watermarks agree it is safe. The token
  estimator charges at least one token per four bytes for every kind of
  content, so a `SUM(LENGTH(payload))` under four times the fold threshold
  means the transcript *might* still be sent whole — and a transcript that
  fits is sent whole, because a snapshot is never a permanent replacement for
  a later turn. Only when it provably cannot fit does a watermark bind, and
  then the earlier of the snapshot's and the goal checkpoint's wins: a
  checkpoint splices the transcript from its own ordinal, and starting after
  that would hand it a silently shorter history. Anything unknown — no
  watermark, a legacy snapshot without one — falls back to the full load.

  `RawTranscript` carries the ordinal its first message sits at, so a consumer
  that indexes by ordinal subtracts it instead of guessing, and asking for a
  slice the load cannot serve returns nothing rather than something shorter.
  `extend_budget` deliberately keeps the full load: it hands the transcript
  straight to a resume turn without assembling it, so no snapshot stands in
  for the earlier rows. That path also resends a long session's whole raw
  history, which is a behaviour question, not a load one, and is left alone
  here.

### Fixed
- **A refused escalation now closes the tool call it announced.** A command
  whose one-shot elevation the user denied was answered into the model's
  transcript and nowhere else: the announcing `ToolCallStarted` never got a
  terminal, so `EventLog::dangling_tool_calls` kept returning it and
  `recover_crash_window` — which runs first on every `resume` and every
  interactive chat turn — read it as a call that may have run and left a side
  effect, blocking the session on `RecoveryConfirmationRequired` until
  `acknowledge_crash_window` was run by hand. A command the runtime itself
  refused to run was being reported as one that might have. All three
  pre-admission refusals (no axis named, an axis already denied, the denial
  itself) now answer the model *and* record the call's errored terminal.
  Found by the first run of the Unified Dogfood Acceptance gate.
- **A budget extension no longer resends the whole raw transcript.** Every
  path that supplies a model with prior context assembled it — snapshot merge,
  checkpoint block, fold if still oversized — except `resume_with_limits`,
  which handed `TurnInput::Resume` the raw messages. A long session therefore
  resent its entire history on every budget extension, the one model request
  bounded by nothing. The four paths now share one assembly, so the exception
  cannot come back by omission, and a source tripwire pins it.
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

### Removed
- **`expand_tools`.** Not a surface judgement: it could never work. Nothing
  consumed its output, the registry it claimed to grow is immutable and the
  tool definitions are snapshotted once per turn. It also advertised two
  categories that registered nothing and rejected the one category with an
  implementation.
- **`replace`.** Zero calls across the recorded evidence, including through six
  `apply_patch` context-matching failures — the exact case it existed to
  absorb, where the model retried `apply_patch` every time. `apply_patch` and
  `write_file` cover the intent.
- **The identical-call loop guard.** Refusing a repeat with "do something
  different" read the model's reasoning. A runaway loop is still bounded, by
  the round ceiling, the token, cost and duration budgets, the wall clock and
  cancellation, and the no-progress stop is now unconditional rather than
  switchable from an eval knob.
- **`create_checkpoint`, `restore_checkpoint`, `consolidate_memory` and
  `create_skill` leave the model surface.** Each has an owner that is not the
  model: the runtime already checkpoints before every write and owns rollback
  and recovery, memory consolidation is subsystem maintenance, and creating a
  skill is the user's act. The implementations stay.
- **The `explicit_plan` and `repeated_read_guard` ablation knobs.** Both named
  a mechanism that no longer exists. An eval config that asks for either now
  fails with the reason rather than running two identical arms.

- **The repeated-read guard.** How often a model re-reads an unchanged range
  is a judgement about its reasoning, not a fact about the filesystem, so the
  reader no longer annotates it.
- **`find_files`'s `mode` and `extension` arguments, and `list_files`'s
  `max_depth`.** `grep`'s `glob` and `max_results` are accepted as aliases of
  `include` and `limit`, so calls recorded in an existing event log still
  replay with their meaning intact.
- **The runtime no longer judges the model's semantic completion.** The
  Completion Contract (a model call at goal start that turned the task into
  obligations with proof policies) and the Completion Reconciliation Gate (a
  second model call at `update_goal(complete)` that judged the first) are
  deleted, together with their config (`agents.completion_judge_model`,
  `agents.completion_judge_timeout_seconds` — still accepted, ignored), their
  ledger fields, their `GoalIntercepted` kinds, and the
  `runtime_evidence_complete` / `step_receipts` / `complete_step` proof
  machinery. Default hidden semantic model calls on a coding goal: zero.
- **Automatic verification repair.** A failed post-edit check is reported as
  `VerificationStatus::Failed` beside a completed outcome; the engine no
  longer opens a repair turn on the model's behalf. `RepairStarted` and
  `TurnKind::Repair` survive only to replay old rows.
- **The auto-reviewer heuristic.** `independent_review: auto` (security /
  concurrency path keywords, wide diffs) is gone. The policy is `off`
  (default) or `required`; legacy `auto` reads as off and `always` as
  required.
- **The plan hard gate.** A multi-step task without `update_plan` gets one
  soft reminder; editing and command tools are never refused for a missing
  plan.
- **Keyword task classification** (`classify_task`,
  `task_looks_like_implementation`). "fix bug" with no mutation completes
  when the model says so; the delivery gate's "implementation task has no
  mutation" and "no fresh verification" refusals are gone.
- **Semantic progress watchdogs.** Closeout thrash, observe thrash, the
  stagnation streak and the 45-round engagement advisory are deleted. What
  remains is mechanical: the identical-call loop guard, the all-calls-refused
  streak, budgets and the absolute round ceiling.

### Fixed
- **`resume` refuses a parallel parent session by its kind.** The engine's
  single-writer claim held only because the parallel parent's transcript
  happened to be empty; `resume` now refuses `ExecutionKind::Parallel`
  outright, and the engine comment says what is true — the launcher owns that
  one session's lifecycle. See `docs/ARCHITECTURE.md` §18.11.
- **The verifier's doc comment no longer claims completion authority.** The
  verifier owns verification verdicts; a passing verdict is mechanical
  evidence a task outcome is judged against, not the runtime deciding the
  request was satisfied. Code was already correct; the comment was not
  (§18.8).

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
  10–30 %.
- **Windows has no daemon socket transport.** Sessions and `resume` work;
  `leveler serve` / `web` / `remote projects` / `remote agent` refuse with a
  clear message rather than pretending. `!command` output for a *confined*
  Windows command arrives on completion instead of streaming live.
- **Three Windows tests fail on `main`** (`command_delivery`, `user_shell`,
  `side_effect_barrier_test`). The Windows build, clippy and security canaries
  are green. Diagnosis needs a Windows machine — see
  the beta blocker resolution record, risk 0b.
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
