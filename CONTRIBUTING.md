# Contributing to CodeLeveler

Thank you for contributing. Small, focused pull requests are the easiest to
review and maintain.

## Before you start

- Search existing issues and pull requests for related work.
- Open an issue before a large behavioral change or public API redesign.
- Do not include credentials, local runtime state, generated evaluation output,
  or external fixture repositories in a commit.
- Keep provider-specific behavior in provider configuration, protocol adapters,
  or compatibility middleware rather than the agent runtime.

## Development setup

Install the Rust toolchain declared in `rust-toolchain.toml`, then run:

```sh
cargo build --workspace
cargo test --workspace
```

Optional platform dependencies are described in the root README. Tests that
require an unavailable OS capability should use the project's existing
capability checks rather than assuming the feature exists.

## Pull requests

Include:

- A concise explanation of the problem and the chosen behavior.
- Tests covering the change and its important boundaries.
- Documentation updates when commands, configuration, or user-visible behavior
  changes.
- Any platform limitations or compatibility impact.

Before opening the pull request, run the complete checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Library crates should expose typed errors. Keep `anyhow` at application
boundaries, preserve `#![forbid(unsafe_code)]`, and avoid adding a dependency
edge from a lower-level crate back to a user-facing layer.

## Evaluation cases

Read `evals/README.md` before adding cases. Keep cases self-contained, verify
fail/pass behavior, preserve any required attribution, and do not commit
external fixture repositories or generated results.

## License

By contributing, you agree that your contribution is licensed under the
project's Apache License, Version 2.0.
