# Documentation index

| Document | Audience | Notes |
| --- | --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Contributors | Stable crate boundaries and runtime flows |
| [ARCHITECTURE.zh-CN.md](ARCHITECTURE.zh-CN.md) | Contributors (中文) | Chinese architecture guide |
| [leveler-config-example.yaml](leveler-config-example.yaml) | Users | Project `.leveler/config.yaml` schema |
| [permissions.example.yaml](permissions.example.yaml) | Users | Permission rules file |
| [hooks.example.yaml](hooks.example.yaml) | Users | Pre/post tool hooks |
| [../configs/example.yaml](../configs/example.yaml) | Users | Global + bundle provider/model schema |
| [../README.md](../README.md) | Everyone | English entry |
| [../README.zh-CN.md](../README.zh-CN.md) | Everyone (中文) | Chinese entry |

## Config layers (quick)

1. **Global** `~/.leveler/config.toml` — default model and API providers.
2. **Bundle** `configs/{providers,models}/*.yaml` — optional checked-in profiles.
3. **Project** `.leveler/config.yaml` — per-repo verify commands, mode, ignore, readonly roots.
4. **Permissions / hooks** — under `~/.leveler/` and/or `.leveler/`.

Permission profile wire values: `request_approval` | `assisted` | `full_access`
(CLI: `--permission`; legacy aliases `plan` / `workspace_write` still parse).
