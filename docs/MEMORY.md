# Cross-session memory

[中文](MEMORY.zh-CN.md)

CodeLeveler can remember durable facts across sessions. Memory is native to the
runtime: an observation inbox captures facts after a turn settles, consolidation
turns those observations into Markdown topics, and every turn's prompt carries a
bounded index of the topics the agent may read.

Memory is **on by default**. The complete pipeline runs unless the rollout is
explicitly disabled with `rollout = "off"`; an `off` rollout captures nothing,
opens no database, and creates no directory.

## Configure it

The rollout lives in the global config (`~/.leveler/config.toml`, or
`$LEVELER_HOME/config.toml`). The default is `active`, so an existing config
needs no `[memory]` section to use memory; set `off` to opt out.

```toml
[memory]
rollout = "active"
```

| Rollout | What it does |
| --- | --- |
| `off` | No capture, no consolidation, no reads, no store creation. |
| `record_only` | Captures observations as files; does not expose them or run Dream. |
| `shadow` | Runs capture and Dream plans without committing curated topic changes. |
| `active` | The complete pipeline and the only rollout that exposes memory to the model. The default. |

Other keys, all optional:

```toml
[memory]
rollout = "active"
model = "deepseek/deepseek-flash"   # capture/Dream route; unset reuses the session model
capture_enabled = true
automatic_dream_enabled = true
manual_dream_enabled = true
batch_dream_enabled = false
batch_dream_max_run_secs = 1800
batch_dream_max_calls_per_batch = 6
batch_dream_max_batch_note_bytes = 98304
compact_index_enabled = false
file_writes_enabled = true
archived_retention_days = 30
job_retention_days = 14
```

A rollout edit is live configuration: after the runtime reloads the file (the
config watcher, or `/reload`), the Memory page and the next capture/Dream
decision use the new value. `/remember` also works while the rollout is `off`:
it writes exactly the note the user asked for so the note survives a later
re-enable.

## Where it lives

Everything is under the CodeLeveler home, never in the repository:

```text
<LevelerHome>/memory/
  global/                         # notes shared by every workspace
  workspaces/<slug>-<hash8>/      # one directory per repository identity
    MEMORY.md                     # generated, bounded index (do not edit)
    topics/*.md                   # curated Markdown topics
    observations/_inbox/*.md      # captured observations
    archive/                      # consolidated or expired observations
    memory_state.sqlite           # ranges, jobs, leases, tombstones, audit
    index.sqlite                  # internal full-text convergence index
```

A workspace's identity is its git remote when it has one, so two clones of the
same repository share memory; otherwise the canonical path is used. A temporary
working directory has no workspace scope, so it never accumulates memory — only
the global scope is available there.

`MEMORY.md` is regenerated from the topic set. The index is bounded: it lists
topic paths and one-line descriptions so a large store cannot crowd out the
conversation. The agent reads a topic with the ordinary `read_file` tool before
relying on it.

## What the agent sees

When the rollout is `active`, each turn's system context receives a
`## Cross-session memory` section containing the generated index and the scope
roots. The instruction tells the model that memory is historical context, not
the current plan: it must verify recalled paths, commands and repository facts
with live tools and prefer current workspace evidence when the two disagree.
Memory files are ordinary Markdown; there is no separate retrieval API and no
vector database.

## Capture

After every settled user turn the runtime queues the ranges the session has not
captured yet. One capture job covers a contiguous range of settled turns. The
extraction call runs on a route pinned in `[memory].model` or the session's
model, with thinking disabled and a forced tool call; it emits structured
observations, never free-form control instructions.

Observations are written as Markdown files with a YAML header (session, turn
range, kind, topic hint, prompt version) and a ledger row in SQLite. Capture
never touches the model's conversation and never blocks the turn: a memory
failure is reported as activity, and the user's work already succeeded.

Jobs are durable. A worker claims a job with a five-minute lease, so a crashed
process's job is not stolen by a restarted process until the lease expires; the
next turn in that session reconciles the gap and completes the job. Committing
is idempotent: a job that already committed the same outcome returns the stored
outcome instead of writing again.

## Consolidation (Dream)

Dream reads pending observations and edits the topic set: it creates or updates
topics, keeps descriptions short, and archives consolidated observations. A
durable row in `memory_state.sqlite` (`consolidation_lock`) serializes Dream
across processes: one owner, one generation, a six-hour lease. A lease whose
owner PID is no longer alive is treated as expired, so a crashed Dream does not
block the next one for hours. Maintenance, index promotion and tombstone
reconciliation all use the same lease rule.

Triggers:

- `/dream` or `d` on the memory page runs one consolidation pass.
- `D` on the memory page runs a batch Dream: several bounded passes with a
  deferred-capacity stop, a call budget and a byte budget.
- Automatic Dream runs after capture settles, when the rollout allows it.
- `/flush` drains this session's capture queue now (busy if a worker owns it).

## Reading and managing memory

### TUI

`/memory` opens the cross-session memory page. It shows both scopes, the file
list, pending observations, archives, tombstones and the Dream lease state.

| Key | Action |
| --- | --- |
| `↑` `↓` / `j` `k` | Move the selection |
| `←` `→` / `h` `l` | Switch global / workspace scope |
| `Tab` / `BackTab` | Cycle panes |
| `/` | Filter files |
| `Enter` / `Ctrl+F` | Read the selected file fullscreen; `Ctrl+F`, `Enter` or `Esc` returns to the list |
| `PageUp` / `PageDown` | Page the detail pane |
| `x` twice | Forget the selected file (the second press confirms) |
| `y` | Copy the selected path |
| `t` | Pause / resume memory for this session without editing the configuration |
| `r` | Refresh |
| `d` | Run Dream |
| `D` | Run batch Dream |
| `f` | Refresh the index |
| `Esc` | Back |

`t` affects only the current session: the configured rollout stays the authority,
the page cannot promote `off`, and nothing is written back to `config.toml`.
While paused the prompt index is not injected and no new capture job is claimed;
resuming captures the turns that ended during the pause. The pause lives in
this process: a new session, and the same session reopened after a restart,
start from the configuration again.

### CLI

```sh
leveler memory status                 # rollout, scopes, counters, files, lease
leveler memory status --json          # machine-readable
leveler memory show workspace topics/build.md   # preview; BLAKE3 on stderr
leveler memory remember "Use pnpm, not npm"     # append a global note
leveler memory forget workspace topics/build.md --hash <BLAKE3>
```

Inside a session: `/memory`, `/remember <text>`, `/dream`, `/flush`.

## Forgetting

Forget binds to the exact file hash printed by `memory show`. The runtime writes
a tombstone row first, then removes the file; a crash between the two is
reconciled on the next store open. The tombstone and audit ledger keep the
record of the removal, and the path is excluded from the index, from capture and
from consolidation, so a new session cannot resurrect a forgotten fact from the
observation inbox.

Consolidated observations are retained under `archive/` as runtime evidence for
Dream convergence and audit. The agent's file tools cannot read that tree: a
tool read is refused and matching search results are omitted, even when a
forgotten fact still exists there, so forgetting holds for later sessions. The
user's own preview may open an archived file while it is retained.

Retention is configurable: archives expire after 30 days and terminal job
metadata after 14 days by default.

## How it fits the runtime

Memory is owned by the runtime, not by a turn or by the model. Sessions are the
capture unit: each session keeps its own cursor, and a job covers a settled turn
range from that session. Resuming a session resumes its capture cursor; a new
session in the same workspace sees the same topics but starts with no pending
range of its own. Capture and Dream run as background work with their own
leases, so closing the TUI does not lose a queued job and a crashed process is
handled by the lease rules above.

A `/goal` turn is captured like any other settled turn. Memory never
participates in the goal completion gate, never decides the next step, and never
turns model prose into control flow: it supplies an index and ordinary Markdown
files, and the model decides whether to read them. When memory is off, the
runtime still runs the session, the goal, tools and compaction exactly as it
does without memory; only capture, Dream and the prompt index are absent.

## Diagnostics

`leveler memory status` is the first check: it reports the rollout, each scope's
pending/running/retry/failed/completed capture jobs, pending observations, the
file list, tombstone count and the Dream lease state. `--json` exposes the same
projection to scripts. A failed capture job remains counted and retryable until
it reaches its terminal state; an expired Dream lease is reported as `expired`
while a live one reads `active`. Issue rows in the TUI and the session report's
`memory_capture`/`memory_dream` counters point at the same durable records, so a
UI symptom can always be traced to a job or an observation file.

`leveler doctor` covers configuration, provider and workspace problems; it is
not a memory-specific command.

## Tests

The memory engine's durable semantics are tested in its own crate:

```sh
cargo test -p leveler-memory
```

Host integration (capture through the app runtime, disabled rollout, restart
and reconciliation) lives in the app's memory tests:

```sh
cargo test -p leveler-app --test memory_integration
cargo test -p leveler-app --test memory_disabled_integration
```

The TUI memory page has behavior tests in the TUI crate:

```sh
cargo test -p leveler-tui --lib memory
```

Memory tests use temporary homes and never read or write `~/.leveler`.

## Limits

- Memory never stores whole conversations. It stores extracted observations and
  curated topics.
- There is no vector index and no knowledge graph.
- Capture and Dream are model calls and cost tokens; the session report breaks
  them out as `memory_capture` and `memory_dream` alongside `round` calls.
- A temporary working directory has no workspace memory, and memory is not
  shared across different repositories unless they share a git remote.

