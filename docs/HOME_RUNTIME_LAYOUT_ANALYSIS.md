# Home & Runtime Hardening — Phase 0 Filesystem Ownership Analysis

Read-only audit (3 parallel agents, cross-crate). No code changed. Baseline
`main @ cf04c38`, branch `refactor/home-runtime-hardening`. Purpose: inventory every
CodeLeveler-owned filesystem path, its owner and lifecycle, and surface the design
decisions/conflicts that must be settled BEFORE the layout refactor.

## 0. Two decisions I need from you before implementing

These are §43 stop-conditions — I will NOT guess them.

### D1 — "Zero Workspace Pollution" scope: machine-written state vs user-authored config
Your §12 lists `.leveler/config.yaml`, `instructions.md`, `rules/`, `skills/` to move
out of the repo. The audit shows these are **user-authored, read-only project assets**
(the loaders only READ them; nothing machine-writes them) — the same category as
`AGENTS.md`, `.editorconfig`, or a committed `CLAUDE.md`. A user may legitimately want
to commit their project's CodeLeveler config to their repo.

The only things CodeLeveler actually **writes into the repo** (true pollution) are:
- `<repo>/.leveler/permissions.yaml` — machine-written `ApproveAlways` rules
  (`permission_rules.rs:398-427`). **Trap:** it shares the exact filename+path with the
  *user-authored* `permissions.yaml` the loader reads; on disk they're indistinguishable.
- `<repo>/.leveler/uploads/` — web attachment upload dir (`repo.rs:714-737`).

**My recommendation:** apply Zero-Workspace-Pollution to **machine-written state only** —
relocate the ApproveAlways writes and `uploads/` out of the repo; **keep** user-authored
`config.yaml`/`instructions.md`/`rules/`/`skills/`/`AGENTS.md` in the repo as first-class
committable assets. For the `permissions.yaml` filename clash, split it: user-authored
stays `<repo>/.leveler/permissions.yaml` (read-only), machine ApproveAlways grants move to
`state/projects/<id>/permissions.yaml` (already partly there — `permission_grants.json`
lives in state today).

**Decision needed:** (a) my split above, or (b) your literal §12 — move ALL project
CodeLeveler config to global `state/projects/<id>/config/`, meaning users can no longer
commit CodeLeveler config with their repo. I recommend (a).

### D2 — Sandbox lifecycle: a reaper needs an owner marker first
Today sandboxes (`codeleveler-sandbox-*`) are RAII `tempfile::TempDir` — cleaned on normal
completion/cancel, but **orphaned permanently on SIGKILL/panic** (no reaper; observed on
disk). A background `cmd &` sandbox is held by the daemon and **stays live after a client
disconnects**. So a `run/sandboxes/` sweeper keyed on mtime-age or TUI-presence would
delete a live daemon-hosted sandbox mid-build. The current sandbox dir carries **no owner
identity** (no pidfile/flock), so no safe reaper can exist yet.

**Decision needed:** approve adding a minimal owner marker (an flock or pidfile inside each
`run/sandboxes/<id>/`, tied to the owning daemon/turn) so the sweeper reaps only when it can
PROVE the owner is dead, and fail-closed otherwise. Without this, the honest move is
"relocate to `run/sandboxes/` but keep RAII-only cleanup (orphans still accumulate on hard
kill)". I recommend adding the owner marker.

---

## 1. Current layout (as-is)

```
~/.leveler/  (= $LEVELER_HOME else $HOME/.leveler)
├── config.toml                         # global user config (2 writers: app + tui)
├── permissions.yaml / hooks.yaml / trusted.yaml   # user-authored rules
├── skills/ · agents/                   # user assets
├── crash/<ts>-<pid>.log
├── leveler.log                         # TUI append log
├── remote/{config.toml,runtime_key,devices.json}  # pairing/identity/creds
├── web-projects.json                   # multi-project registry
├── tool-cache/<workspace-sha>/…        # disposable build caches (persistent)
├── codeleveler-sandbox-<rand>/         # per-command scratch (RAII; orphaned on kill)
├── sock/<repo-hash>.sock  + <hash>.lock  # daemon socket + singleton flock (short: SUN_LEN)
├── locks/<target-hash>.lock            # advisory edit locks
└── projects/<slug>-<hash>/             # PER-PROJECT durable state
    ├── .repository-root                # owner marker
    ├── sessions.db · memory/{active,archive} · permissions.yaml
    ├── permission_grants.json · media/ · artifacts/
    ├── draft.txt · input_history.json · runtime-id(+.lock) · daemon.log
```
Also: `$LEVELER_CONFIG_DIR` / `<repo>/configs/{providers,models}` = dev/product asset
bundle (orthogonal to home); `<repo>/.leveler/{config.yaml,instructions.md,rules,skills,
hooks.yaml}` = user-authored project config; `<repo>/.leveler/{permissions.yaml,uploads}` =
machine-written (the pollution, see D1).

## 2. Target layout (from the brief) — lazy-created, ownership namespace
```
~/.leveler/
├── config.toml
├── state/  { projects/<id>/{config/,sessions.db,memory/,permissions.yaml,media/,
│            artifacts/,draft.txt,input_history.json,.repository-root},  remote/,  web/projects.json }
├── run/    { sockets/, locks/, sandboxes/ }
├── cache/  { tools/ }
├── runtimes/                # reserved (no downloads this phase)
└── logs/   { leveler.log, crash/, daemon/<id>.log }
```

## 3. Path ownership inventory (old → new)

| Concern | Current (creator file:line) | New | Owner | Lifecycle |
|---|---|---|---|---|
| Home resolution | `leveler-core/environment.rs:215` (+ divergent `layout.rs:113`) | one `LevelerHome` in **leveler-core** | core | authority |
| Global config | `<home>/config.toml` `global_config.rs:268` (+tui `theme_config.rs:16`) | unchanged root | app/tui | user config, preserved |
| Project state dir | `<home>/projects/<slug>-<hash>` `layout.rs:147` | `state/projects/<id>` | project | durable |
| sessions.db | `layout.rs:68` | `state/projects/<id>/sessions.db` | storage | durable |
| memory/ | `layout.rs:90` | `state/projects/<id>/memory/` | memory | durable |
| permissions.yaml (machine grants) | `layout.rs:97` + `permission_grants.json` | `state/projects/<id>/` | execution | durable (see D1) |
| media/ · artifacts/ | `interactive.rs:246` · `lib.rs:423` | `state/projects/<id>/…` | app/exec | durable |
| draft.txt · input_history.json | `run_cmds.rs:465-466` | `state/projects/<id>/…` | tui/cli | durable |
| runtime-id (+.lock) | `runtime_identity.rs:25,56` | `state/projects/<id>/` | app | durable identity |
| .repository-root | `layout.rs:253` | `state/projects/<id>/.repository-root` | project | marker |
| project CodeLeveler config | `<repo>/.leveler/*` (user) + writes (machine) | user stays in repo; machine → state (D1) | project/exec | see D1 |
| socket | `<home>/sock/<hash>.sock` `layout.rs:79` | `run/sockets/<hash>.sock` | project/transport | ephemeral (KEEP short: SUN_LEN) |
| daemon flock | `<home>/sock/<hash>.lock` `lib.rs:573` | `run/sockets/<hash>.lock` | transport | never-unlink liveness |
| edit locks | `<home>/locks/<hash>.lock` `layout.rs:136` | `run/locks/<hash>.lock` | project/tools | inode-correct unlink |
| sandboxes | `<owner>/codeleveler-sandbox-*` `host_cache.rs:251` | `run/sandboxes/<id>/` | execution | see D2 |
| tool cache | `<owner>/tool-cache/<sha>` `host_cache.rs:255` | `cache/tools/` | execution | disposable |
| remote state | `<home>/remote/` `config.rs:128` | `state/remote/` | remote-agent | durable creds |
| web registry | `<home>/web-projects.json` `run_cmds.rs:925` | `state/web/projects.json` | web/cli | durable registry |
| daemon.log | `<state_dir>/daemon.log` `run_cmds.rs:272` (truncated) | `logs/daemon/<id>.log` | cli | diagnostic (append/rotate) |
| leveler.log · crash/ | `~/.leveler/{leveler.log,crash/}` `main.rs:103`,`crash.rs:20` | `logs/{leveler.log,crash/}` | cli | diagnostic |
| runtimes/ | — (none) | `runtimes/` reserved | — | future (no downloads) |

## 4. The 16 audit questions
1. **Who resolves LEVELER_HOME?** `leveler-core/environment.rs:215 leveler_home_dir_from` (LEVELER_HOME→HOME/.leveler→USERPROFILE/.leveler→None). NOT single: `layout.rs:113 resolve_leveler_home` adds a temp fallback; ~10 callers add their own (relative `.leveler`, temp, None) — inconsistent.
2. **How many crates build global paths themselves?** ~8 (project, app, tui, skills, agent, cli, execution, remote-agent) — no central "give me dir X" owner.
3. **Is Layout the sole authority?** No — owns only repo_root/config_dir/state_dir; `leveler_home()` is private; config.toml/skills/agents/crash/remote/hooks/trusted/web-projects computed elsewhere.
4. **Who bypasses Layout?** All of the above; worst: `eval_cmd.rs:608` uses raw `HOME/.leveler`, ignoring `LEVELER_HOME`.
5. **Where is a sandbox created?** `host_cache.rs:251 tempdir_in(owner)`, owner = `$LEVELER_HOME`|`~/.cache/codeleveler-private`|temp.
6. **Who deletes it?** RAII `TempDir` on `SandboxPaths`/`BackgroundTask.sandbox_scratch` drop. No crash reaper.
7. **After abnormal exit?** Orphaned permanently (observed). No sweeper today (so zero mis-reap risk today — but that's the D2 gap).
8. **Socket stale cleanup?** `Drop for LocalSocketServer` (inode-guarded) `lib.rs:535`; crash → next bind reclaims via flock probe `lib.rs:579`.
9. **Lock lifecycle?** daemon flock held for life, OS-released on crash, never unlinked; edit lock acquire→flock→unlink-before-release with inode re-check (`replace.rs:939-980`).
10. **Logs distribution?** daemon.log under project state (truncated/spawn); leveler.log + crash/ under home. None are recovery sources (crash recovery = DB).
11. **repo `.leveler` readers/writers?** readers: config.yaml/instructions.md/rules/skills/hooks (user); writers (pollution): `permission_rules.rs:398` permissions.yaml, `repo.rs:714` uploads/.
12. **Legacy migration APIs/callers?** `encode_repo_path_legacy` `layout.rs:177`, `migrate_legacy_state_dir` (auto, `:197`), `migrate_legacy_repo_state`/`legacy_repo_state_paths` (pub, `:221/:245`), CLI `sessions migrate-state` (`cli.rs:663`, `sessions_cmd.rs:212`). 7 tests in layout.rs. No docs. → deletable.
13. **known_repositories scans?** `<home>/projects/*/.repository-root` `layout.rs:268`; sole caller web `projects.rs:517`. Canonicalizes repo path before encoding.
14. **Remote state location?** `<home>/remote/{config.toml,runtime_key,devices.json}` `config.rs:128`.
15. **Web registry location?** `<home>/web-projects.json` `run_cmds.rs:925`.
16. **Tests hardcoding old paths?** None hardcode legacy literals; several compute `home.join("projects")…` (permissions.rs:29, daemon_e2e.rs:166, eval_cmd.rs:610, layout.rs tests, web projects.rs fixtures) — all need the `state/` insertion.

## 5. Architecture — single path authority
- Introduce **`LevelerHome` in `leveler-core`** (lowest crate; execution/tui/all already depend on core; avoids a `leveler-execution → leveler-project` back-edge). It owns the ONE resolution + all `root()/state_dir()/run_dir()/…` accessors.
- `Layout`/`ProjectLayout` (leveler-project) consumes `LevelerHome` and derives per-project paths.
- **Preserve the injected-home seam**: execution + remote-agent take `&LevelerHome`/`&Path`, never resolve themselves (keeps them testable, no back-edges).
- **Keep `$LEVELER_CONFIG_DIR` orthogonal** (it's the dev `configs/{providers,models}` bundle + a supported CLI/env override; NOT part of home).
- Fold the relative-`.leveler` and `eval_cmd` bypasses into the one resolver; pick a single missing-home fallback.
- Tripwire: forbid `home.join("projects"|"sock"|"locks"|"remote"|…)` and `repo_root.join(".leveler")`-writes outside the owner via an architecture test.

## 6. Conflicts / risks (must handle together)
- **D1 config-vs-asset** (decision above) — the headline.
- **D2 sandbox reaper owner marker** (decision above).
- **`state/` intermediary** breaks `socket_path()`'s and `migrate_state`'s two-`parent()` home-derivation (`layout.rs:81`, `sessions_cmd.rs:213`) — must update with the move; plus every `home.join("projects")` site.
- **SUN_LEN**: sockets must stay short; if under `run/sockets/`, re-verify `socket_path_stays_under_sun_len_for_deep_repositories` (`layout.rs:374`). If a custom `LEVELER_HOME` itself overflows → clear error, never spill a socket into the workspace.
- **Lock correctness**: preserve edit-lock unlink-before-release + inode re-check; never let `run/` housekeeping unlink the daemon `*.lock` (its persistence is the liveness signal).
- **Two config.toml writers** (app `GlobalConfig` + tui `theme_config`): centralize the PATH without breaking each one's `toml_edit` format-preservation.
- **Legacy removal**: delete the 4 symbols + CLI subcommand + 7 tests + `repo_state_dir_in` auto-migration branch; no docs affected.
- **Cross-platform**: keep Windows path/id behavior + tests; Unix socket is a platform-specific consumer.

## 7. Recommendation
Proceed to implementation IF you approve D1(a) and D2(add-owner-marker). The refactor is
large but mechanical once the two decisions are fixed: `LevelerHome` in core → thread it
everywhere → move state/run/cache/logs → delete legacy → tripwire + tests (lazy-create,
LEVELER_HOME override, zero-pollution, SUN_LEN, sandbox lifecycle) → docs. No Browser, no
Goal/permission redesign, no cross-version migration.
