# Gate 1 · Secret Propagation Safety (F6) — Closeout

**Result: PASS.** F6 moves from `OPEN_BETA_BLOCKER` to `VERIFIED_FIXED`.

Baseline `c3bf11b` · branch `beta-closure/unattended` · commits `234755f`, `c68c403`, `fd5bd3c`.

## Security invariants now held

| # | Invariant | How |
| --- | --- | --- |
| A | A concrete secret value the model does not need never enters model-visible tool output | Value-position-aware sanitization at `ToolHost::dispatch_raw` — the single boundary every tool result crosses |
| B | …and therefore never enters the provider request | Same string feeds `messages` → `ModelRequest`; asserted directly against captured provider payloads |
| C | Once identified, a value cannot re-enter durable storage even as prose | Session-scoped registry consulted at the F2 structure-aware durable boundary |
| D | Source code, field names and type names are never damaged | Detection is position-aware, not keyword-based; pinned by negative tests and a real daemon smoke |

## Architecture

```
registry.execute → raw ToolOutput
      │
      ├─ detect secret VALUES (env / export / JSON / YAML / CLI flag /
      │  Authorization / URL userinfo / connection string / provider-key shape)
      ├─ register values under ctx.session_scope
      └─ return SANITIZED content
             ├─► model messages ─► provider request
             └─► events / session_messages / snapshots
                        └─ F2 structure-aware write + registry scrub
```

No new crate, no change to `Executor::new` or the factory, no tool-name lists, no second state machine. `ToolContext.session_scope` already existed, so the registry needed no new plumbing.

## Why the obvious fix was rejected

Relocating `redact_secrets` to this boundary is refuted by measurement at `c3bf11b`: it rewrites `let password = config.password;` → `let password = [REDACTED];` and `pub password: String` → `pub password: [REDACTED]`, while missing `API_PASSWORD=…`, `export TOKEN=…`, and `postgres://user:pw@host`. Detection had to become value-position aware first.

## Defects the gate's own smoke caught (not visible to unit tests)

1. **Durable records still used the old keyword scrubber.** The agent received correct source, wrote a correct answer about `pub password: String`, and the *durable write* mangled that answer into `password: [REDACTED]`. Durable string leaves now use the same value-position detector — which also gives stored records the `.env`/`export`/URL coverage the old scrubber lacked. Self-identifying provider key shapes (`sk-`, `ghp_`, `AKIA`) were carried across so coverage never regressed.
2. **A real panic on multi-byte text.** Slicing at a scheme keyword could land inside a multi-byte character, so Chinese prose around a credential aborted the sanitizer. Guarded and pinned.
3. **Markdown inline code read as a credential.** `` `password: String` `` was redacted and the closing backtick swallowed — markdown is exactly how an assistant writes about types. Backticks now delimit values; a real secret inside backticks is still redacted with delimiters intact.

## Verification

- **Detector**: 62 `leveler-core` tests — positive positions, negative source-code cases, structure preservation, small values, URL, ENV, JSON, auth headers, multi-byte, provider-key shapes, registry scope/bounds/isolation.
- **Boundary**: `secret_values_never_reach_the_model_or_the_provider` asserts against the captured provider payload; `source_code_mentioning_credentials_reaches_the_model_intact` asserts code arrives byte-identical.
- **Durable**: `registered_session_secret_is_scrubbed_from_assistant_prose`, `registered_secrets_do_not_cross_sessions`.
- **Workspace**: 2774 passed / 0 failed (98 binaries) · clippy `-D warnings` clean · fmt clean.
- **Reverse validation**: disabling Layer A turns the provider test RED; disabling Layer B turns the prose-echo test RED. Both restored GREEN.
- **Daemon smokes (12/12)**: A credentials file · B shell output · C disconnect/resume · D source-code integrity. Every smoke used a unique secret and scanned events, messages and turn payloads directly.
- **P0–P4**: 7/7. Recovery Truth PASS (62 events, 0 malformed, resume replayed).

## Binary provenance

Built from a clean worktree at each candidate HEAD with an isolated target dir, installed and codesigned. Final: HEAD `fd5bd3c`, `git status --porcelain` empty, sha256 `d026e6d9bf1c6ef0…`.

An investigation artefact worth recording: two smoke failures were initially attributed to the product but were **stale daemons still serving the previous binary**. Re-running against cleared state passed. Any future gate must clear per-repo daemons before re-measuring.

## Residual risk (honest, by design)

This gate closes plaintext propagation of **identified** values. It does **not** claim to solve exfiltration:

- base64/hex/encoded transformations of a secret
- a model paraphrasing a value character-by-character or in pieces
- credentials in shapes no position rule recognises (a bare high-entropy token with no key)
- secrets the **user** types into a goal before anything has identified them — the user message itself is user-supplied input; the registry only protects values already detected
- historical rows written before this gate (no retroactive rewrite by design)

## Explicit non-scope

No DLP, no taint engine, no secrets manager, no vault, no policy DSL, no encrypted store, no EventLog v2, no `SecretRef` execution capability.
