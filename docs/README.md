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
| [evaluations/README.md](evaluations/README.md) | Contributors | Eval framework: capability `evals/` vs observer `eval/` |
| [eval-methodology.md](eval-methodology.md) | Contributors | Adoption vs safety denominators; what counts as a spawn |

## Config layers (quick)

1. **Global** `~/.leveler/config.toml` — default model and API providers.
2. **Bundle** `configs/{providers,models}/*.yaml` — optional checked-in profiles.
3. **Project** `.leveler/config.yaml` — per-repo verify commands, mode, ignore, readonly roots.
4. **Permissions / hooks** — under `~/.leveler/` and/or `.leveler/`.

Permission profile wire values: `request_approval` | `assisted` | `full_access`
(CLI: `--permission`; legacy aliases `plan` / `workspace_write` still parse).
