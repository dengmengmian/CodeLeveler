# 参与 CodeLeveler 开发

[English](CONTRIBUTING.md) · [项目首页](README.zh-CN.md)

## 本地构建

用 rustup 管理 Rust，在仓库根目录运行以下命令。工具链版本由 [`rust-toolchain.toml`](rust-toolchain.toml) 固定。

```sh
rustup show active-toolchain
cargo build --locked -p leveler-cli --bin leveler
cargo run --locked -p leveler-cli -- --version
```

## 本地运行浏览器界面

先构建前端，再编译并运行 CLI。CI 使用 Node.js 22。

```sh
npm --prefix crates/leveler-web/web ci
npm --prefix crates/leveler-web/web run build
cargo run --locked -p leveler-cli -- web
```

桌面端见[桌面端构建说明](apps/leveler-desktop/README.md)。

## 修改与验证

1. 修 Bug 先复现。写一个最小回归测试，确认它因预期问题失败。难以用单测覆盖的行为可用可复现的手动步骤或集成测试。
2. 修改后重跑目标测试，按影响扩大到相关包或测试目标。在仓库根目录运行 `./dev help` 查看开发命令，验证范围见 [`AGENTS.md`](AGENTS.md#验证策略)。
3. 公开命令、配置项或契约变化时，同步中英文文档。纯文档改动检查链接，并对照实际命令或配置核实示例。

PR 写清问题、改动和实际验证。Bug 修复附平台和复现步骤。界面改动附截图或可重复的操作步骤。

## 反馈问题

Bug、兼容性问题、功能建议和文档反馈可用 [Issue 表单](https://github.com/dengmengmian/CodeLeveler/issues/new/choose)。报告 Bug 时带上 `leveler --version`、系统和架构、预期与实际结果、可分享的最小复现。安全漏洞按[安全策略](SECURITY.md#reporting-a-vulnerability)私下报告。
