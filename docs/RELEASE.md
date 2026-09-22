# CodeLeveler 1.0.6

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

A WebUI packaging and Apple Terminal compatibility release on top of 1.0.5.
Installed stable builds can pick it up automatically, or run `leveler update`
/ `/update`.

## Fixed

- Release builds now compile the WebUI before the Rust binary, so `leveler web`
  serves the embedded frontend instead of returning “WebUI assets are not
  built”
- The release contract fails when the frontend install/build steps are absent
  or run after Rust compilation
- Source-install instructions now include the required Node.js frontend build
  before `cargo install`
- Apple Terminal before macOS 26 now receives a nearest-colour xterm-256
  palette, avoiding broken RGB rendering while preserving dark/light theme
  polarity and contrast calculations
- The active-goal header now keeps its detail affordance one column away from
  the terminal's right edge

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may
  block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that
  isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while
  it runs

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
