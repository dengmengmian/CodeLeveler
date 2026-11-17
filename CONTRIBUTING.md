# Contributing to CodeLeveler

[中文](CONTRIBUTING.zh-CN.md) · [Project home](README.md)

## Build locally

From the repository root, use Rust via rustup. The repository pins the toolchain in [`rust-toolchain.toml`](rust-toolchain.toml).

```sh
rustup show active-toolchain
cargo build --locked -p leveler-cli --bin leveler
cargo run --locked -p leveler-cli -- --version
```

## Run the browser UI locally

Build the frontend before compiling and running the CLI. CI uses Node.js 22.

```sh
npm --prefix crates/leveler-web/web ci
npm --prefix crates/leveler-web/web run build
cargo run --locked -p leveler-cli -- web
```

For desktop work, see the [desktop build instructions](apps/leveler-desktop/README.md).

## Make and verify a change

1. Reproduce a bug before changing behavior. Add a focused regression test and confirm it fails for the expected reason. Use reproducible manual steps or integration tests for behavior that is hard to cover with unit tests.
2. After the change, rerun the focused test and cover the affected package or target as needed. Run `./dev help` from the repository root for the development commands and see [`AGENTS.md`](AGENTS.md#验证策略) for verification scope.
3. Update the English and Chinese docs when a public command, configuration option, or contract changes. For docs-only changes, check links and verify examples against the command or configuration they describe.

Open a pull request with the problem, the change, and the verification you ran. Include the platform and reproduction steps for bug fixes. For UI changes, include a screenshot or the steps needed to see the behavior.

## Report an issue

Use the [issue forms](https://github.com/dengmengmian/CodeLeveler/issues/new/choose) for bugs, compatibility problems, feature requests, and documentation feedback. For a bug, include `leveler --version`, the OS and architecture, expected and observed behavior, and the smallest reproduction you can share. Send security reports privately using the [security policy](SECURITY.md#reporting-a-vulnerability).
