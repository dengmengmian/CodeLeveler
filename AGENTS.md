# Working on CodeLeveler

Notes for an AI agent (or a new contributor) changing this repository. This
file indexes conventions rather than restating them — where a rule already
lives in code or in another document, the pointer is the authority and this
file is not.

## Read first

| Document | Covers |
| --- | --- |
| `CONTRIBUTING.md` | PR expectations, the exact check commands, error-type and dependency-direction rules |
| `docs/ARCHITECTURE.md` | Crate layout and how a turn flows through the system (`docs/ARCHITECTURE.zh-CN.md` is the Chinese version) |
| `docs/STABILITY.md` | Draft compatibility proposal (not frozen yet) — read before touching CLI flags or config schema |
| `evals/README.md` | Required before adding evaluation cases |

`CONTRIBUTING.md` already states the three rules most often gotten wrong:
library crates expose typed errors, `anyhow` stays at application boundaries
(`leveler-cli` / `leveler-app` only), and no lower-level crate may take a
dependency edge back to a user-facing layer. Those are not repeated here.

## The unsafe policy has exactly one exception

Twenty-five crates carry `#![forbid(unsafe_code)]`. `leveler-execution` is
deliberately different: `crates/leveler-execution/src/lib.rs` uses
`#![deny(unsafe_code)]`, because `forbid` cannot be relaxed by a scoped
`allow`, and this crate needs exactly one — the Linux `PR_SET_PDEATHSIG`
pre-exec hook in `command.rs` / `background.rs`, the only way to guarantee
grandchildren die when the parent is force-killed.

**Do not "fix" that `deny` into a `forbid`.** It will not compile, and the
reason is documented at the declaration. In the production crates covered by
these crate-level attributes, a new `unsafe` block still fails the build unless
it is explicitly allowed and justified the same way.

## Language conventions

- **Code comments and doc comments: English.** Of ~1270 lines containing
  Chinese, only ~83 are comment lines, and those are mostly glosses on
  user-facing strings.
- **User-visible strings and TUI copy: Chinese is expected** where the product
  surface is Chinese (see `crates/leveler-execution/src/risk.rs`, where
  permission-profile names carry their Chinese label).
- **Test fixtures: either**, as the case requires.
- Top-level docs that need a Chinese version use the `.zh-CN.md` suffix
  alongside the English original, not a mixed-language file.

## Tests

- Integration tests go in `crates/<crate>/tests/`; 11 crates have them.
- Unit tests are inline `#[cfg(test)] mod tests` in the file under test — 181
  source files do this. Keep a test next to what it covers rather than
  inventing a parallel structure.
- A test needing an OS capability that may be absent must use the project's
  existing capability checks, not assume the feature exists.

## Before you propose a refactor

This codebase has several files over 2000 lines, and they attract mechanical
"split this up" suggestions. Two failure modes to avoid:

1. **Do not infer a file's contents from its line count.** Command
   classification lives in `approval.rs`, shell-AST danger analysis in
   `shell_ast.rs`, risk vocabulary in `risk.rs` — not in `command.rs`, which
  is what a line-count-driven reading tends to assume. Read the file.
2. **Line counts include inline tests.** `command.rs` measures 2649 lines when
   ~1501 of them are `mod tests`. Compare production code, not totals.

Also note: in Rust 2018+, `foo.rs` and a `foo/` directory coexist. Adding a
submodule does not require renaming the parent to `foo/mod.rs`.

Splitting a file inside one crate does **not** reduce incremental compile time
— rustc's unit is the crate. Split for readability, and say so; do not claim a
build-time benefit that will not materialize.

## Verifying a change

The commands are in `CONTRIBUTING.md`. Run them; do not report a change as
working on the strength of a successful `cargo build` alone. When a test fails
for an environment reason rather than the change, say which test and why —
silently skipping is worse than a documented gap.
