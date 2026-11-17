# Security policy

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability.

Use GitHub's **Report a vulnerability** flow on the repository's Security tab
to send a private report. Include the affected version or commit, platform,
configuration, reproduction steps, impact, and any suggested mitigation.

If private vulnerability reporting is unavailable, contact a repository
maintainer privately through the contact information on their GitHub profile.
Do not include API keys, access tokens, private source code, or other secrets in
the initial report.

You should receive an acknowledgement within seven days. Timelines for a fix and
disclosure depend on severity, affected platforms, and whether downstream users
need time to update.

## Scope

Security-sensitive areas include:

- Workspace path and symbolic-link boundaries.
- Command permissions, sandbox capability reporting, and process-tree cleanup.
- Tool-call validation and approval handling.
- Credential redaction in logs, events, session storage, and artifacts.
- Provider and MCP transport authentication.
- Session isolation and local runtime transport.

Reports about a provider's service, an operating-system sandbox implementation,
or a third-party dependency may need to be coordinated with that upstream
project.

## Privacy and telemetry

CodeLeveler does not collect or report product telemetry or user work data to
the CodeLeveler project or a central service by default. This includes source
code, file contents, conversations, prompts, model output, command output,
environment information, usage analytics, and crash context.

This policy does not prohibit network use required by features the user
explicitly enables. Model providers, MCP servers, web search, remote execution,
and update checks may contact their configured destinations as required for
their function. Those paths must remain explicit and scoped to the data needed
for that capability; they must not become hidden telemetry channels.

Streamable HTTP MCP is one of those explicit paths. The URL, header names, and
bearer variable name come from the user's config. Header and bearer values are
read from the environment at load and are not written back to the config, the
session log, or tool-argument display. HTTPS verifies the server certificate;
there is no setting that disables that check. Redirects that would send
credentials to another origin are not followed. Tool descriptions from an MCP
server stay on that tool. They are not copied into the system prompt and they
do not change the permission profile. OAuth 2.1 is not implemented.

Any future diagnostics, crash reporting, or usage analytics must be explicit
opt-in and disableable. Credentials and secrets must never be included as
telemetry.

## In-repo configuration is inert until trusted

Three files a repository can carry are gated, because each takes effect before
any approval prompt:

- `<repo>/.leveler/hooks.yaml` — runs commands ahead of every tool call.
- `<repo>/.leveler/permissions.yaml` — an `Allow` rule here short-circuits the
  approval policy.
- `<repo>/.leveler/config.yaml` — its `mode:` can raise the default permission
  profile for a new session.

Cloning a repository is therefore not enough to make them apply. They are
ignored until you run `leveler trust` for that repository. One direction is
always safe: a project config may **tighten** its default mode (for example to
`request_approval`) without trust, because narrowing authority never widens it.
Raising the default to `full_access` needs a trust record for the exact bytes.
The skip is always visible: a notice on stderr for CLI runs, and in the TUI
both the empty-session splash and a marker on the composer border. Trust records
the SHA-256 of the exact bytes you accepted, so a later edit — by a teammate, by
`git pull`, or by the agent — drops the file back to untrusted. `leveler trust`
records the digest it displayed; a file that changes while the confirmation is
open is not trusted. `leveler trust show` reports the current state and
`leveler trust revoke` clears it.

The global `~/.leveler/hooks.yaml` and `~/.leveler/permissions.yaml` are your
own files, outside any repository, and are not gated.

The workspace layer additionally refuses **writes** to the hooks and
permissions files, so the agent cannot grant itself hooks or standing
permissions. Reads stay allowed. `.leveler/config.yaml` stays editable as
ordinary project configuration, but any edit invalidates its trust record, so
it cannot inherit a mode approval given to different bytes. Confined shell
commands also cannot write `.leveler/agents` or `.leveler/skills` on macOS
(the kernel denies the path even when the directory does not exist yet) and on
Linux when those directories exist: a definition authored that way would be
loaded by a later session without the confirmation the `save_agent` /
`save_skill` tools require. Windows has no per-path deny in the current write
backend, so that shell path remains unenforced there.

## Approval, rules and resource boundaries

An approval decides whether a call may run; it does not silently widen the
resource boundary the call was admitted under.

- A standing `Allow` rule and an automated reviewer remove the question. They
  keep the profile's write and network scopes.
- Consent to the exact call executes the action the person saw. That can lift
  the workspace write fence for the approved effect, including a foreign-path
  conflict the prompt named; it never clears an explicit `--deny-network`.
- When the call itself is a network request (a network-class tool or an MCP
  tool), the approval prompt carries that destination decision; the sandbox
  then enforces what the prompt granted. Ordinary commands under a profile
  that restricts the network run without it, even after approval.
- `Loopback` is enforced as loopback by seatbelt (macOS). Backends that cannot
  express a loopback-only boundary deny all network rather than widening to the
  Internet.

## Known trade-offs (by design)

These are deliberate boundaries, documented so they are not mistaken for
oversights. Reports that only restate them will be closed as known.

- **Shell reads are not filtered.** The file tools refuse sensitive paths
  (`.env`, `.ssh`, `.aws`, key material), but `run_command`/`shell_command` can
  read any file the OS sandbox allows — `cat .env` succeeds and its output
  reaches the model. The sandbox confines *writes* and network, not reads. Keep
  secrets out of workspaces you point an agent at, or run with a stricter OS
  sandbox profile.
- **`leveler-tools` alone is not a security boundary.** Dangerous-command
  classification and approval prompts live in the executor layer
  (`leveler-execution` / `leveler-agent`). Embedding the tool registry without
  that layer means no `rm -rf` / `sudo` gate — only the OS sandbox applies.
- **Hardlink aliases defeat a path-based write fence.** The OS fences confine
  *paths*, not inodes. On macOS the sandbox refuses to create a hardlink to a
  file outside the workspace, but an alias that already exists (created before
  confinement, by a Full session, or by the user) can be written through and
  changes the outside inode. Linux and Windows behavior is not verified on a
  real host. The file tools reject hardlink aliases; the shell path model does
  not claim inode identity. Do not place hostile pre-existing links in a
  workspace you point an agent at.
- **Desktop automation is not network-confined.** `computer_act` drives the
  user's applications outside the process sandbox, so `--deny-network` cannot
  bound an app it launches. The action still needs per-call approval in
  restricted modes; this is a product boundary, not a shell/HTTP path.
- **Tool output artifacts are bounded and redacted, not encrypted.** Large
  tool results are stored under the state directory's `artifacts/` as
  content-addressed files; secret values are redacted before writing and the
  directory is swept to a 512 MiB quota (oldest first). A swept artifact is
  reported as unavailable rather than silently replaced. A local process that
  can read the state directory can read retained non-secret output.
- **Policy files created after admission are not covered on every backend.**
  `AGENTS.md`, `.git`, `.leveler/agents` and `.leveler/skills` stay write-denied
  for a confined shell while the workspace is writable. On macOS the profile
  denies any `AGENTS.md` at any depth (case-insensitively) through a path
  regex, so creating one after admission is denied too. Linux re-binds existing
  paths read-only; a missing protected path is left absent because bwrap can
  only mount an existing destination, so a root `AGENTS.md`, `.git`, or
  definition directory created after admission is not covered. This was
  reproduced on a real Linux kernel (Ubuntu 22.04 container, privileged bwrap):
  a confined shell created `AGENTS.md` and `.leveler/agents/...` and both
  persisted, while existing protected paths survived write, delete and
  recreate attempts. Materializing those destinations was tried and reverted:
  it changed the workspace for the duration of every confined command and broke
  tools that detect VCS metadata from the presence of `.git`. Windows labels existing protected paths
  Medium after lowering the write root (unverified on a real Windows host); a
  protected path created after admission is not covered. The file tools still
  refuse definition edits and require consent through the definition-writing
  tools.

## Resource grants

Session- and project-scoped resource grants (exact capability + resource
identity) are listed and revoked from the CLI:

```
leveler permissions grants [--session <id>] [--json]
leveler permissions revoke-grant <id>
```

Revocation is project-scoped by handle. An already-admitted call keeps its
frozen policy; the next admission no longer matches the revoked grant.

## Supported versions

Security fixes are applied to the latest release and the default branch. Older
versions may not receive backports.
