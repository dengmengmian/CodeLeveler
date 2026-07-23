# Interface stability

> **Status: DRAFT — not yet a commitment.** This file records what a 1.0
> compatibility promise *would* cover, derived from the code as it stands.
> Every "Frozen" mark below is a proposal until a maintainer signs off. Once
> signed off, drop this banner and link the file from `README.md` and
> `CONTRIBUTING.md`.

## Why this exists

`README.md` currently tells users the command surface and config format may
change before 1.0. That is honest, but it is also the single biggest reason an
early adopter refuses to automate around Leveler: an upgrade that removes a
subcommand or rejects an existing config file breaks their scripts silently.

Freezing costs internal refactoring freedom. It buys the ability to tell users
"your `leveler run` invocation and your `config.toml` will keep working". At
beta, that trade is worth making explicitly rather than by accident.

Three surfaces are in scope: the CLI, the configuration files, and the Rust
API. They need different promises.

## 1. CLI surface

Nineteen top-level subcommands exist today (`crates/leveler-cli/src/cli.rs`,
`enum Command`). Proposed tiers:

| Tier | Subcommands | Promise |
| --- | --- | --- |
| **Frozen** | `run`, `plan`, `discuss`, `resume`, `tui` (also the no-subcommand default) | Name, positional arguments, and semantics do not change before 1.0. Flags may be added; existing flags keep their meaning. |
| **Frozen** | `doctor`, `init`, `upgrade`, `config`, `models`, `model`, `sessions`, `memory`, `permissions` | Same promise. These are what setup scripts and CI call. |
| **Provisional** | `serve`, `web`, `lsp`, `mcp` | Semantics may change while the daemon/transport story settles (see the Windows gap in `README.md`). Breaking changes require a `CHANGELOG.md` entry under a `### Changed` heading. |
| **Unstable** | `eval` | Internal evaluation harness. No promise; may change or disappear without notice. Should say so in its own `--help` text. |

Rationale for the split: the first two tiers are what a user types or scripts
by hand. `eval` is development tooling that happens to ship in the same binary
— freezing it would constrain the harness for no user-facing benefit.

**Global flags** (`--repo`, `--config-dir`, `--readonly-root`, `--verbose`)
apply to every subcommand and should be frozen with the same promise as tier 1.

## 2. Configuration files

Two config files, and they currently have **opposite** compatibility
behaviours. This is the part most likely to bite on upgrade, and it needs a
decision before anything is frozen.

| File | Parser strictness | Effect of an unknown key |
| --- | --- | --- |
| `~/.leveler/config.toml` (global) | `#[serde(deny_unknown_fields)]` on every struct (`crates/leveler-app/src/global_config.rs`) | **Hard error.** Startup fails. |
| `<repo>/.leveler/config.yaml` (project) | Lenient; documented as "Invalid YAML is non-fatal: Leveler falls back to defaults" (`docs/leveler-config-example.yaml`) | Ignored, falls back to defaults. |

### The forward-compatibility trap

Every field in the global config already carries `#[serde(default)]`, so a
*newer* Leveler reads an *older* config fine. That is backward compatibility,
and it holds today.

The reverse does not. Because of `deny_unknown_fields`, an *older* Leveler
reading a *newer* config **fails to start** rather than ignoring the field it
does not know. This matters in three real situations:

- a user downgrades after hitting a regression;
- one `~/.leveler` is shared across machines running different versions;
- a team commits a project config written against a newer release.

"New fields must be optional" — the usual formulation — does not fix this.
Optional-ness is about the newer binary; `deny_unknown_fields` is about the
older one.

**Open decision (D1):** pick one.

1. Keep `deny_unknown_fields` and accept that config is forward-incompatible.
   Document it: "downgrading may require removing newly added config keys."
2. Relax to collecting unknown keys and warning about them. Loses the
   typo-catching that `deny_unknown_fields` gives today (`defualt_model`
   silently ignored instead of reported) — which is a real usability win worth
   naming before trading it away.
3. Keep strictness but add a `schema_version` key, so an old binary can emit
   "this config needs Leveler >= X" instead of a raw serde error.

Option 3 preserves typo-catching and produces an actionable error, at the cost
of one field and the discipline to bump it. It is the recommendation, but it is
the maintainer's call.

**Proposed freeze, once D1 is settled:** existing keys in both files keep their
name, type, and meaning through 1.0. Removing or retyping a key requires a
major version. Adding a key is always allowed.

## 3. Rust API

Leveler publishes 27 crates but is consumed as a binary. Only the traits a
third party would *implement* need a stability promise; the rest are internal
seams that should stay free to change.

| Trait | Crate | Role |
| --- | --- | --- |
| `ProtocolAdapter` | `leveler-model` | Add a vendor wire protocol |
| `Tool` | `leveler-tools` | Add a tool the model can call |
| `EventStore` | `leveler-storage` | Swap session persistence |
| `ModelRuntime`, `CompatibilityMiddleware` | `leveler-model` | Model invocation + per-model quirk shims |
| `Approver`, `AutoReviewer` | `leveler-execution` | Permission decisions |
| `InteractiveRuntimeClient` | `leveler-client-protocol` | Drive a session from another front-end |
| `LocalRuntimeService` | `leveler-local-transport` | Daemon transport |
| `Clarifier`, `TranscriptSink` | `leveler-agent` | Agent-loop hooks |

The first three are the plausible third-party extension points and are the
candidates for "additive changes only before 1.0". The rest are internal
composition seams; freezing them buys nothing today.

Note for anyone drafting this from memory: `leveler-protocol` and
`leveler-provider` define **no** public traits. `leveler-protocol` exports
adapter *implementations* (`OpenAiChatAdapter`, `AnthropicMessagesAdapter`);
the `ProtocolAdapter` trait they implement lives in `leveler-model`.

**Open decision (D2):** is the Rust API a public surface at all? If nobody is
expected to depend on these crates from outside this workspace, the honest
answer is "no promise, use the CLI" — and that is less work to keep true than
a promise nobody needs.

## 4. Recording it

Once D1 and D2 are settled, `CHANGELOG.md` gains a `## Compatibility` section
listing the frozen surfaces, and any PR touching them needs a changelog entry.
Without that enforcement point, this document goes stale within two releases.

## Open decisions summary

| Id | Decision | Blocking |
| --- | --- | --- |
| D1 | Config forward-compatibility strategy (`deny_unknown_fields`) | Freezing the config schema |
| D2 | Whether the Rust API is a public surface | Freezing any trait |
| D3 | Whether `serve`/`web` stay provisional through 1.0 | CLI tier assignment |
