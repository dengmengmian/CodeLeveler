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

## In-repo configuration is inert until trusted

Two files a repository can carry are gated, because both take effect before
any approval prompt:

- `<repo>/.leveler/hooks.yaml` — runs commands ahead of every tool call.
- `<repo>/.leveler/permissions.yaml` — an `Allow` rule here short-circuits the
  approval policy.

Cloning a repository is therefore not enough to make either apply. They are
ignored, with a notice on stderr, until you run `leveler trust` for that
repository. Trust records the SHA-256 of the exact bytes you accepted, so a
later edit — by a teammate, by `git pull`, or by the agent — drops the file
back to untrusted. `leveler trust show` reports the current state and
`leveler trust revoke` clears it.

The global `~/.leveler/hooks.yaml` and `~/.leveler/permissions.yaml` are your
own files, outside any repository, and are not gated.

The workspace layer additionally refuses **writes** to both in-repo files, so
the agent cannot grant itself hooks or standing permissions. Reads stay
allowed.

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

Until CodeLeveler reaches 1.0, security fixes are applied to the latest release
and the default branch. Older pre-release versions may not receive backports.
