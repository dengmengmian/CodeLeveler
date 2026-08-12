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
| `0a54186` | Bilingual layout docs; fix two pre-canonical test harnesses; first regression pass. |
| `c52a667` | **Review closeout (H-1, H-2):** route the sandbox scratch + tool cache through `LevelerHome` (`run/sandboxes`, `cache/tools`; no `codeleveler-private`/`tool-cache`; poisoned `LEVELER_HOME` fails closed); bind the background lease to the scratch's lifetime; strengthen the tripwire (bare `leveler_home_dir(`, `"tool-cache"`). |

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

## Merge closeout (review round)

Review of the pushed branch surfaced two MAJOR findings; both are now CLOSED.

**H-1 — Canonical home authority was incomplete (was: the tool cache escaped
canonical ownership).** `prepare_sandbox_paths()` still resolved its owner via
the raw `leveler_home_dir()` with a multi-candidate fallback, so the tool cache
was created at `<owner>/tool-cache/<hash>` (not `cache/tools`) and could land in
`$HOME/.cache/codeleveler-private` or a temp dir — outside the one authoritative
home. Fixed: it now resolves a single `LevelerHome`, validates the root is
outside the workspace (rejecting even a home reachable *through* a
workspace-planted symlink, on the raw path before any resolve), and derives the
scratch and tool-cache roots from `sandboxes_dir()` / `tool_cache_dir()`
(`run/sandboxes`, `cache/tools`) — no `codeleveler-private`, no bare
`tool-cache`, no silent fallback. A poisoned `LEVELER_HOME` now fails closed
instead of inventing another namespace. The tripwire was strengthened to also
ban the bare resolver `leveler_home_dir(` and the `"tool-cache"` literal in
business `src`.

**H-2 — Background command dropped its lease before the scratch ended.**
`into_scratch()` returned a naked `TempDir`, dropping the `SandboxLeaseGuard`
while a backgrounded command still used the scratch. Fixed structurally: it now
returns an owned `SandboxScratch` carrying both the `TempDir` and the lease, so
`lease lifetime == actual scratch lifetime`. Tests S1–S7 cover synchronous hold,
lease-survives-transfer, drop-reclaims, crash-orphan reclaim, live-lease
survival, and fail-closed-without-a-lock. The previously-accepted residual is
withdrawn — CLOSED.

## Residuals (accepted)

- Global user-config files (`hooks.yaml`, `permissions.yaml`, `trusted.yaml`,
  `agents/`, `skills/`) stay at the home **root** (user-owned config, not the
  six machine namespaces). They now route through `LevelerHome` accessors, so
  the single-authority invariant holds without relocating existing user data.

---

## Regression (post-closeout)

- `cargo fmt --check` — clean.
- `cargo test --workspace --no-fail-fast` — **123 test binaries, 2678 tests, 0
  failures.**
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.

Test harnesses that carried pre-canonical layout assumptions were updated
(not product regressions — they had been stale since the state/socket/cache
dirs moved and were only exposed once the suite ran end to end):
`daemon_e2e.rs` (`home/sock` → `run/sockets`, `home/projects` →
`state/projects`), `permissions.rs` (`home/projects` → `state/projects`), and
the sandbox-cache tests (`home/tool-cache` → `cache/tools`; the poisoned-
`LEVELER_HOME` case now asserts fail-closed instead of a HOME fallback).

## Final status

| Item | Status |
| --- | --- |
| H-1 canonical sandbox+cache authority | CLOSED |
| H-2 background lease bound to scratch lifetime | CLOSED |
| Canonical tool cache path | `~/.leveler/cache/tools/<hash>` |
| Zero Workspace Pollution | enforced (tripwire + real sandbox-init test) |
| `LEVELER_HOME` override | honored; poisoned home fails closed |
| Full regression | 2678 tests / 0 fail; clippy -D clean; fmt clean |
| Blockers / Majors / Minors | 0 / 0 / 0 |
| Merge recommendation | ready for a final human review, then PR |

Not merged; feature branch only, pending human review.
