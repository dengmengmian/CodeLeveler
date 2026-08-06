# Interface stability

> **Status: DRAFT — the tier marks below are proposals, not commitments.**
>
> D1/D2/D3 have *recommended* answers (see "Decisions"), written during
> convergence-plan phase 7 and following common pre-1.0 practice. They are
> **awaiting maintainer sign-off**: freezing a CLI or config surface is the
> project owner's call, not a decision a contributor makes on their behalf.
> Once signed off, change this banner to ADOPTED and link the file from
> `README.md` and `CONTRIBUTING.md`.

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

Top-level subcommands live in `crates/leveler-cli/src/cli.rs` (`enum Command`).
There is **no** product `plan` or `discuss` subcommand — multi-phase orchestrate
was removed; long tasks use `run` / TUI with `--collaboration goal` (or
`/goal`). Proposed tiers:

| Tier | Subcommands | Promise |
| --- | --- | --- |
| **Frozen** | `run`, `resume`, `tui` (also the no-subcommand default) | Name, positional arguments, and semantics do not change before 1.0. Flags may be added; existing flags keep their meaning. |
| **Frozen** | `doctor`, `init`, `upgrade`, `config`, `models`, `model`, `sessions`, `memory`, `permissions` | Same promise. These are what setup scripts and CI call. |
| **Provisional** | `serve`, `web`, `lsp`, `mcp`, `login`, `logout`, `completions`, `trust` | Semantics may change while the daemon/transport/auth story settles (see the Windows gap in `README.md`). Breaking changes require a `CHANGELOG.md` entry under a `### Changed` heading. |
| **Unstable** | `eval` | Internal evaluation harness. No promise; may change or disappear without notice. Should say so in its own `--help` text. |
| **Unstable** | `remote` | Remote control from a paired phone, and the TUI's `/remote`. The wire protocol, the pairing payload and the on-disk `~/.leveler/remote/` layout may all change; a change that invalidates existing pairings means re-pairing every device, and will say so in `CHANGELOG.md`. Not to be described as production-ready until the security gate passes (`docs/design/remote-security-gate.md`). |

Rationale for the split: the first two tiers are what a user types or scripts
by hand. `eval` is development tooling that happens to ship in the same binary
— freezing it would constrain the harness for no user-facing benefit. `remote`
is newer than any of them and its trust model is still being reviewed; a tier
that promised anything would be promising it about code whose own gate document
says do not ship this yet.

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
| `ModelRuntime` | `leveler-model` | Model invocation |
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

## Decisions

**Recommended, pending maintainer sign-off.** Each is the option a typical
pre-1.0 project takes; none is in force until confirmed:

| Id | Recommendation | Rationale |
| --- | --- | --- |
| D1 | *Recommend* option 1: keep `deny_unknown_fields`; config is forward-incompatible and that is documented ("downgrading may require removing newly added config keys", noted in the config examples). | Typo-catching is a real usability win today; a `schema_version` key (option 3) is the 1.0-time upgrade if the downgrade complaint materializes. Existing keys keep name/type/meaning through 1.0; removal or retyping requires a major version; adding keys is always allowed. |
| D2 | *Recommend*: the Rust API is not a public surface before 1.0. No trait is frozen; the supported surface is the CLI + configuration. `ProtocolAdapter` / `Tool` / `EventStore` are the designated candidates for an additive-only promise at 1.0. | Nobody outside the workspace depends on these crates; a promise nobody needs is pure maintenance cost. |
| D3 | *Recommend*: `serve` / `web` (and `lsp`/`mcp`/`login`/`logout`/`completions`/`trust`) stay Provisional through 1.0. Breaking changes require a `### Changed` changelog entry. | The daemon/transport/auth story is still settling (see the Windows gap in `README.md`). |

Proposed deprecation cycle for Frozen surfaces, if D1–D3 are accepted:
deprecate in release N with a changelog entry and a runtime warning where
feasible; remove no earlier than N+2 (or 1.0, whichever is later).

The body below still reads as a proposal ("Open decision (D1)", "Proposed
freeze") — deliberately, because it is one. When the decisions are signed off,
that wording and this section should be reconciled in the same edit.

## Storage: migrations and backup

- Schema migrations are append-only and applied automatically at startup
  (`migrations/README.md` has the authoring rules). Canonical events carry a
  `schema_version`; a newer row than the binary understands is a hard, named
  replay error — never a guessed repair.
- The state to back up is the global Leveler home (default `~/.leveler`):
  `sessions.db` (+ its `-wal`/`-shm` siblings) holds sessions, transcripts,
  and the canonical event log; `config.toml`, `permissions.yaml`, and
  `memory/` hold user configuration and memory. Back up with the runtime
  stopped, or use `sqlite3 sessions.db ".backup backup.db"` on a live
  database — copying only the `.db` file while a daemon is writing loses the
  WAL tail. Restoring an older database into a newer binary is supported
  (migrations re-run); the reverse follows the same rule as config: not
  supported before 1.0.
