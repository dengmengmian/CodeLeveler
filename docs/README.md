# Documentation index

| Document | Audience | Notes |
| --- | --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Contributors | Authoritative architecture: CURRENT / TARGET / DEBT / FUTURE |
| [ARCHITECTURE.zh-CN.md](ARCHITECTURE.zh-CN.md) | Contributors (中文) | Chinese architecture guide (parity with EN) |
| [TUI_ARCHITECTURE.md](TUI_ARCHITECTURE.md) | Contributors | CURRENT TUI ownership contract (geometry, conversation, presentation) |
| [TUI_ARCHITECTURE_AUDIT.md](TUI_ARCHITECTURE_AUDIT.md) | Contributors | Pre-hardening TUI ownership audit at `eceb271` (historical) |
| [leveler-config-example.yaml](leveler-config-example.yaml) | Users | Project `.leveler/config.yaml` schema |
| [permissions.example.yaml](permissions.example.yaml) | Users | Permission rules file |
| [hooks.example.yaml](hooks.example.yaml) | Users | Pre/post tool hooks |
| [../configs/example.yaml](../configs/example.yaml) | Users | Global + bundle provider/model schema |
| [../README.md](../README.md) | Everyone | English entry |
| [../README.zh-CN.md](../README.zh-CN.md) | Everyone (中文) | Chinese entry |
| [STABILITY.md](STABILITY.md) | Users · Contributors | What is Frozen / Provisional / Unstable across CLI, config, and Rust API |
| [ROADMAP.md](ROADMAP.md) | Everyone | Beta scope and the post-Beta multi-agent phases |
| [multi-agent.md](multi-agent.md) | Users | Sub-agent delegation: what it does, and that the model elects it |
| [multi-agent.zh-CN.md](multi-agent.zh-CN.md) | Users (中文) | 同上 |
| [evaluations/MA-WA1-FINAL.md](evaluations/MA-WA1-FINAL.md) | Contributors | Delegation adoption: what was tested, eliminated, and decided |
| [BETA_RELEASE_READINESS.md](BETA_RELEASE_READINESS.md) | Maintainers | Beta release gate: blockers, required items, release recommendation |
| [BETA_BLOCKER_RESOLUTION.md](BETA_BLOCKER_RESOLUTION.md) | Maintainers | How those blockers were closed, and what verification backs each |
| [evaluations/README.md](evaluations/README.md) | Contributors | Eval framework: capability `evals/` vs observer `eval/` |
| [eval-methodology.md](eval-methodology.md) | Contributors | Adoption vs safety denominators; what counts as a spawn |
| [MOBILE_UI_UX_CLOSURE.md](MOBILE_UI_UX_CLOSURE.md) | Contributors | Mobile UI/UX closure: workspace, not chat (v1.0) |
| [MOBILE_UI_UX_CLOSURE.zh-CN.md](MOBILE_UI_UX_CLOSURE.zh-CN.md) | Contributors (中文) | 同上 |
| [MOBILE_RUNTIME_ALIGNMENT.md](MOBILE_RUNTIME_ALIGNMENT.md) | Contributors | Mobile M8–M12: steer, artifacts, coverage |
| [MOBILE_RUNTIME_ALIGNMENT.zh-CN.md](MOBILE_RUNTIME_ALIGNMENT.zh-CN.md) | Contributors (中文) | 同上 |
| [MOBILE_BETA_CLOSURE.md](MOBILE_BETA_CLOSURE.md) | Contributors | Mobile Beta: fetch_attachment + Task Detail |
| [MOBILE_BETA_CLOSURE.zh-CN.md](MOBILE_BETA_CLOSURE.zh-CN.md) | Contributors (中文) | 同上 |
| [MOBILE_FREEZE.md](MOBILE_FREEZE.md) | Contributors | Mobile feature work frozen at `mobile-beta-mvp` |
| [MOBILE_FREEZE.zh-CN.md](MOBILE_FREEZE.zh-CN.md) | Contributors (中文) | 同上 |

## Config layers (quick)

1. **Global** `~/.leveler/config.toml` — default model and API providers.
2. **Bundle** `configs/{providers,models}/*.yaml` — optional checked-in profiles.
3. **Project** `.leveler/config.yaml` — per-repo verify commands, mode, ignore, readonly roots.
4. **Permissions / hooks** — under `~/.leveler/` and/or `.leveler/`.

Permission profile wire values: `request_approval` | `assisted` | `full_access`
(CLI: `--permission`; legacy aliases `plan` / `workspace_write` still parse).
