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

## Supported versions

Security fixes are applied to the latest release and the default branch. Older
versions may not receive backports.
