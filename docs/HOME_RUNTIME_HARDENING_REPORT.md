# Home & Runtime Hardening — Report

**Status:** complete on `refactor/home-runtime-hardening` (not merged; awaiting human review).
**Goal:** one canonical `~/.leveler` filesystem layout behind a single path
authority, **Zero Workspace Pollution**, and a crash-safe sandbox runtime.

---

## What "Zero Workspace Pollution" means here

CodeLeveler must **never create or mutate CodeLeveler runtime/state
infrastructure inside the workspace** unless the user explicitly asks for a
project-owned asset. It is *not* "no `.leveler/` in the repo": a user's
committed `<repo>/.leveler/{config.yaml,instructions.md,rules,skills,hooks.yaml}`
and `AGENTS.md` are first-class, committable config the runtime only reads.
What must leave the workspace is everything *machine-written*.

---

## Decisions

| # | Decision | Choice |
| --- | --- | --- |
| D1 | Config vs. machine state | Keep user-authored project config committable in-repo; move only machine-written state (web uploads, the ApproveAlways `permissions.yaml`, sessions.db, memory, image store, registry) under the home. |
| D2 | Sandbox lifecycle | Owner **lease** (OS `flock`) held for the sandbox's lifetime, RAII as the primary cleanup, plus a **fail-closed reaper** for crash orphans — opportunistic (once per process), never periodic. |

---

## The layout

`LevelerHome` (`crates/leveler-core/src/home.rs`) resolves the root once
(`$LEVELER_HOME` → `$HOME/.leveler` → `%USERPROFILE%\.leveler` → process-local
temp; **never** a cwd-relative fallback) and is the sole source of every
sub-path.

```
~/.leveler/
├── config.toml          (+ agents/ skills/ hooks.yaml permissions.yaml trusted.yaml — user global config)
├── state/  projects/<id>/ · remote/ · web/{projects.json,uploads/}
├── run/    sockets/ · locks/ · sandboxes/<id>/(+<id>.lock)
├── cache/  tools/
├── runtimes/            (reserved)
└── logs/   leveler.log · crash/ · daemon/<id>.log
```

Lazy: asking for a path never creates it. Project id = readable slug + short
SHA-256 of the canonical repo path.

---

## Changes, by commit

| Commit | What |
| --- | --- |
| `afef887` | Introduce `LevelerHome`, the single path authority (+ unit tests). |
| `e2d9559` | Route project + web + eval paths through it; drop the whole legacy state-migration surface and the `sessions migrate-state` command. |
| `8263a5a` | **Web uploads out of the workspace** — `<repo>/.leveler/uploads` → `state/web/uploads`, injected into the web app state at the composition root; symlink/reparse escape guard re-rooted at the home; escape tests moved to isolated in-crate units. Fixes a discovery regression the web tests caught. |
| `8588f14` | Route every remaining home path through `LevelerHome`; **drop cwd `.leveler` fallbacks** (they wrote state into the launch directory when `$HOME` was unset). New accessors (`agents_dir`, `skills_dir`); crash → `logs/crash`, remote → `state/remote`; canonicalize the web registry to `state/web/projects.json` and give `ProjectManager` the home root so discovery scans the real `state/projects/`. |
| `ea96b89` | **Sandbox lease + fail-closed reaper** under `run/sandboxes/` (D2). |
| `272f7e7` | **Architecture tripwire**: a test fails the build if any crate rebuilds a home path by hand (`from(".leveler")` or `|home| home.join(`). |
| `ddb6202` | Lazy-create and filesystem-level zero-pollution tests. |

---

## Invariants now enforced by tests

- **Single authority.** The tripwire (`no_crate_rebuilds_home_paths_by_hand`)
  fails CI on any hand-built home path across `crates/*/src`.
- **No cwd fallback.** All home resolution goes through `LevelerHome::resolve`,
  whose only fallback is a workspace-external temp home.
- **Lazy.** `accessors_never_create_directories` — touching every accessor on a
  pristine root creates nothing.
- **Repo stays clean.** `resolving_a_layout_writes_nothing_into_the_repository`.
- **Uploads never touch the workspace** (web integration + in-crate unit tests).
- **SUN_LEN.** Socket paths stay under the macOS limit (existing layout test).
- **Sandbox lease lifecycle S1–S4.** Lock create/remove on drop; reaper reclaims
  a free-lock orphan; a live lease survives; a lock-less dir is left (fail-closed).

---

## Residuals (accepted)

- A command **backgrounded** via `into_scratch` releases its lock sidecar when
  its `SandboxPaths` is consumed; if the daemon then crashes, that one scratch
  dir orphans with no lock and the fail-closed reaper won't reclaim it. Rare,
  disposable, and under `run/sandboxes/`. The common (synchronous) path keeps
  its lock to the end, so its crash orphan *is* reclaimed.
- Global user-config files (`hooks.yaml`, `permissions.yaml`, `trusted.yaml`,
  `agents/`, `skills/`) stay at the home **root** (user-owned config, not the
  six machine namespaces). They now route through `LevelerHome` accessors, so
  the single-authority invariant holds without relocating existing user data.

---

## Regression

- `cargo fmt --check` — clean.
- `cargo test --workspace --no-fail-fast` — **123 test binaries, 2675 tests, 0
  failures.**
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.

Two test harnesses carried pre-canonical layout assumptions and were updated
(not product regressions — they had been stale since the state/socket dirs
moved and were only exposed once the suite ran end to end):
`daemon_e2e.rs` (`home/sock` → `home/run/sockets`, `home/projects` →
`home/state/projects`) and `permissions.rs` (`home/projects` →
`home/state/projects`).

Not merged; feature branch only, pending human review.
