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

## Project definitions are inert until trusted

Authority-bearing definitions are gated independently of the executable config
above. This covers `<repo>/AGENTS.md`, every nested `AGENTS.md`,
`.leveler/instructions.md`, `.leveler/rules/*.md`, declarative agents under
`.leveler/agents`, skills under `.leveler/skills` and the compatible project
roots (`.codex/skills`, `.agents/skills`, `.claude/skills`), plus the user-wide
agent and skill roots (including `~/.leveler/agents` and `~/.leveler/skills`).

The gate is content-addressed, not path-addressed. A definition is admitted only
when a human approved the exact bytes on disk:

- The approval is bound to the definition identity (project root, type, canonical
  relative path) and to the SHA-256 of its content. For an agent, both
  `agent.yaml` and `instructions.md` contribute to one composite digest, so a
  trusted manifest with an edited instructions file is not trusted.
- A file that exists but was never approved, or whose bytes changed after
  approval, is **not** put into the model's control context. It is not treated
  as an absent rule: the next turn receives a runtime-fact diagnostic naming the
  path and the typed state (`unapproved` or `changed`).
- A missing, corrupt, or future-schema trust store fails closed. A store that
  lives inside the repository is refused entirely, because that repository's
  own sandboxed execution could rewrite it.
- Git-tracked or Git-committed status is not approval. A clone, a commit, a
  rename, a copy, a runtime restart or a resumed session does not inherit a
  trust record for different bytes.
- The approval surface is the human CLI. `leveler trust` lists pending
  definitions and approves the exact digests it displayed; `leveler trust
  show` lists the state; `leveler trust diff <path>` shows the approved version
  against the current bytes; `leveler trust revoke-definition <path>` removes
  one approval. The agent has no tool that writes a trust record, and neither a
  permission mode nor model text creates one.
- Status is re-evaluated from the store on every turn, and a turn's decisions
  are frozen into that turn's execution context. A definition created or edited
  by a command during a turn cannot become trusted authority inside a request
  that was already composed. Child agents and reviewers inherit the same frozen
  execution context; they do not re-discover definitions through a second path.

The gate controls what the harness loads as **authority**. It does not stop the
model from reading an unapproved file through an ordinary tool and being
influenced by its text; that is the general prompt-injection boundary of tool
output, not an admission of the file as a trusted definition. It also does not
change the OS write boundary: on Windows a confined shell can still create a
previously missing `AGENTS.md` or a missing `.leveler` tree. Such a file is
created but not admitted; the two facts are reported separately.

**FullAccess is a grant of the user's own authority, not a definition
approval.** A FullAccess agent can write any user file, including the trust
store, by design. Do not run an untrusted workspace with FullAccess and expect
the trust gate to constrain the agent. The gate's threat model is bounded
execution and non-user surfaces.

The workspace layer additionally refuses **writes** to the hooks and
permissions files, so the agent cannot grant itself hooks or standing
permissions. Reads stay allowed. `.leveler/config.yaml` stays editable through
the file tools as ordinary project configuration, but any edit invalidates its
trust record, so it cannot inherit a mode approval given to different bytes. A
confined shell command cannot write inside an existing `.leveler` tree — the
agent and skill definitions, the rules, or the config: a definition authored
that way would be loaded by a later session without the confirmation the
`save_agent` / `save_skill` tools require. The same boundary covers every
existing `AGENTS.md` at any depth and every existing `.leveler/instructions.md`
and `.leveler/rules/*.md`, because those are the documents a later turn loads
as authority. macOS denies the paths with Seatbelt rules even when they do not
exist yet. Linux uses the same read-only binds for existing paths, and a
`seccomp` supervisor (started only when a protected path is missing) refuses
creation syscalls that would give a protected name a new inode — including
`AGENTS.md` at any depth below a writable root. The supervisor performs allowed
operations itself and injects their descriptors into the sandbox, so a symlink
swapped in after the check cannot redirect the write. Windows labels existing
protected paths Medium after lowering the write root and additionally holds a
delete-sharing fence on every existing protected file: Mandatory Integrity
Control denies content writes, and the fence denies the `DELETE` access that
delete, rename and replace require, so a Low child in a Low workspace can no
longer delete or replace them. A protected path created after admission is
still not covered there (see the trade-offs below); if it is a definition, the
trust gate above still refuses to load it as authority.

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
- **Policy files created after admission are covered on macOS and Linux, not
  on Windows.** `AGENTS.md`, `.git`, `.leveler/agents` and `.leveler/skills`
  stay write-denied for a confined shell while the workspace is writable. On
  macOS the profile denies any `AGENTS.md` at any depth (case-insensitively)
  through a path regex. On Linux `bwrap` can only bind an existing destination,
  so a missing protected path is fenced by `leveler-linux-confine`: a
  `seccomp` user-notification supervisor resolves every creation syscall
  (`openat`, `mkdirat`, `mknodat`, `symlinkat`, `linkat`, `renameat*`) and
  denies a protected destination; allowed operations are performed by the
  supervisor with the destination's parent pinned and the final component
  opened with `O_NOFOLLOW`, and the resulting descriptor is injected with
  `SECCOMP_IOCTL_NOTIF_ADDFD`. This keeps the workspace untouched (no
  placeholder files) and preserves ordinary writes. If the kernel or process
  inspectors cannot support the fence, the confined command is refused rather
  than run unfenced. `io_uring` submission is disabled inside the fence because
  it would bypass syscall filtering. On Windows every **existing** protected
  file is labelled Medium (content writes denied), and a delete-sharing fence
  held by the trusted runtime denies delete, rename and replace. A protected
  path that does not exist yet has no object for either mechanism: NTFS has no
  name-based creation filter for an unprivileged token, so a confined shell can
  still create a new `AGENTS.md`, or a wholly missing `.leveler` tree. The
  definition-writing tools and their consent remain the authority on such a
  path, and the gap is tracked as an open platform limitation rather than
  claimed closed.

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
