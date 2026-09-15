# v0.2.0-beta.3 Release Record

## Status

```text
V0_2_0_BETA_3_PRODUCT_ACCEPTANCE=PASS
V0_2_0_BETA_3_RELEASE=PASS
BETA3_RELEASE_FROZEN=YES
```

This record fixes what the `v0.2.0-beta.3` tag contains and what was proven
about it. It does not freeze `main`. `AGENT_EXTENSIBILITY_CLOSURE.md` is left
as written: at closure the Web UI had not been seen in a browser, and this
record is where that residual was closed.

## Identities

These are distinct commits, each with its own evidence:

| Identity | Commit | Evidence |
| --- | --- | --- |
| Agent Extensibility closure docs | `aa20256` | CI `34950029475`, attempt 1, success |
| Agent product acceptance fixes | `da9345f`, `7540cf5` | tested on an isolated copy of `53caa60` + both fixes; real browser, TUI and model acceptance on that build |
| Final integration head (codeleveler-b6's TUI Conversation & Input Control closure on top) | `0acc120` | local full gate + CI `34961010347`, attempt 1, success |
| Pre-release head | `0acc120` (no integration fix was needed) | same |
| Release candidate (`release: prepare v0.2.0-beta.3`) | `6bd4f78` | CI `34963007027`, attempt 1, success |
| Release candidate tree | `624462e638ef456cca9def6262a229a49b605ba2` | |
| Tag | `v0.2.0-beta.3`, annotated (`4e38f79`) → `6bd4f78` | Release workflow `34964954244`, attempt 1, success |

## 1. Product acceptance

The Agent Extensibility closure had mechanical tests and a protocol-level Web
E2E, but nobody had looked at Settings → Agents. Acceptance was run against the
real products with a temporary `LEVELER_HOME` and a fixture repository.

| Surface | How | Result |
| --- | --- | --- |
| Web Settings → Agents | `leveler web` in headless Chrome 153, driven with real clicks and keystrokes, screenshots read at each step | list, create (preview, confirmation, files), detail, edit (`agent.yaml` bytes unchanged, instructions updated, next spawn uses them), no-op save (inode and mtime unchanged), delete (files gone, future spawn not found, history still named) — PASS |
| Web states | same | empty (no session), invalid (`unknown tool`), unavailable (unconfigured model, spawn refused with the reason, no fallback), shadowed (project over user), 64-character name, 6 KB Chinese/English/code-block instructions preserved, reserved name, bad name, duplicate name, user-scope create/delete, built-in copy — PASS after fixes |
| Natural-language authoring | Web conversation, `deepseek/deepseek-v4-flash`: "给这个项目创建一个只读的 Rust 安全审查 Agent…" | model chose `save_agent` (no edit tool), proposal shown, approved once, files valid, listed without restart; spawned on a real Rust change: snapshot `rust-security-reviewer`/project, instructions in the child's first system message once, child tools `git_status, git_diff, read_file, report_finding`, working tree unchanged, settled `completed_with_findings`; deleted through `delete_agent` with confirmation — PASS |
| TUI approval | real `leveler tui --in-process` on a PTY, full-access | `save_agent` still asks; three options, no "始终允许"; `w` inert; Esc denies; nothing written — PASS |

### Defects found and fixed

| Defect | Fix |
| --- | --- |
| `write_file`/`apply_patch` refused `.leveler/agents/**` only by exact spelling; on macOS `.LEVELER/agents/<name>/agent.yaml` wrote a definition past `save_agent` | directory names compared ASCII-case-insensitively (`da9345f`) |
| Legacy `<name>.md` personas were ignored silently | a `.md` with a frontmatter fence directly in an agents directory is reported as not loaded, with the target directory; READMEs are not reported (`da9345f`) |
| TUI, CLI prompt and Web `a` offered "always allow (write project rule)" for every approval; for `save_agent`, `delete_agent`, `remember`, `forget`, `write_file` it became a session grant | `UiApprovalRequest.always_persists` from `ApprovalRequest::always_persists` (the executor's own `always_rules_for`); clients offer "always" only when true (`7540cf5`) |
| Web editor opened an invalid definition as a blank, saveable form | reason, location and delete only (`7540cf5`) |
| Delegated list named a settled custom child by its class (`explorer`) | `UiAgentObservation.agent` (`7540cf5`) |
| Agents panel: English jargon permissions, Built-in listed first, "available" claimed more than a configuration check, delete confirmation named no files | plain-word permissions, precedence order with source meaning, 已配置, files named (`7540cf5`) |

```text
WEB_AGENT_MANUAL_ACCEPTANCE=PASS (Chrome 153 headless, http://127.0.0.1:<port>/ → Settings → Agents)
NATURAL_LANGUAGE_AGENT_ACCEPTANCE=PASS
TUI_APPROVAL_WORDING_TRUTH=PASS
LEGACY_PERSONA_DIAGNOSTIC=PASS
AGENT_MODEL_AVAILABILITY=CONFIG_ONLY (Web says 已配置; docs say so)
SHELL_AGENT_WRITE_BYPASSES_VALIDATION=NO
AGENT_EDIT_PATH_BYPASS=0
```

## 2. Integration

After acceptance, codeleveler-b6 landed its closure on the same `main`:
`53caa60` (never fold prose), `066d24a` (command output streaming and proven
stop), `ff96679` (protocol 1.10), `50b42c4` (command rows with live output and
per-command stop), `218f7e6` (待发送), `0acc120`.

Range `7540cf5..0acc120`: 67 files. It touches the executor dispatch path,
engine events, client protocol, remote policy and the TUI; it does not touch
the agent registry, authoring, approval derivation, workspace path authority,
the approval overlay or the Web Agents panel. Classification:
`POTENTIAL_INTERACTION`, covered by the regression slice below.

| Agent regression slice (on `0acc120`) | Tests |
| --- | --- |
| registry, precedence, invalid fail-closed, store, legacy diagnostic | 39 passed |
| custom spawn, read-only authority, writer scope, snapshot | 16 passed |
| `save_agent` / `delete_agent` | 7 passed |
| restart truth | 17 passed |
| multi-agent | 84 passed |
| Web agent protocol | 5 passed |
| `always_persists` projection | 9 passed |
| Delegated agent identity | 17 passed |
| agent path authority | 24 passed |
| approval / permission rules | 37 + 23 passed |
| CLI approver | 1 passed |
| TUI approval overlay / snapshots / reducer (custom child identity) | 10 / 36 / 271 passed |
| protocol schemas + transcript golden | 101 passed |
| Web / mobile | 236 / 81 passed |

Protocol 1.10: `check:protocol` in sync, schema export and golden tests pass,
Flutter golden replay passes.

```text
AGENT_ACCEPTANCE_REGRESSION=PASS
PROTOCOL_GENERATED_ARTIFACTS_CURRENT=YES
APPROVAL_PERSISTENCE_TRUTH=PASS
```

## 3. Gates

### Final integration head `0acc120`, local, default target directory

```text
git diff --check / cargo fmt --check    ok
cargo clippy -D warnings                ok
cargo test --workspace                  4113 passed, 1 failed, 20 ignored   (first attempt)
web check:protocol / typecheck / test / build   ok / ok / 236 / ok
flutter analyze / test                  no issues / 81
cargo deny                              ok
FIRST_ATTEMPT_STABLE=NO
```

The one failure, `leveler-tools tools::apply_patch::tests::stale_delete_rolls_back_earlier_commits_without_clobbering_external_changes`,
is a pre-existing test race, not a product regression:

- no file on its code path changed since `aa20256` (`apply_patch.rs`,
  `workspace/editor.rs`, `layout.rs`);
- the test takes the target's lock at `target_lock_path(temp_dir()/…/second.rs)`,
  while the editor locks the canonical path. On macOS `TMPDIR` is
  `/var/folders/…` and canonicalizes to `/private/var/folders/…`, so the hashes
  differ, the test's lock never blocks the editor, and the test passes only when
  its busy loop writes before the patch's second delete;
- it reproduced once in 65 `leveler-tools` lib runs on `0acc120`; 80 runs on
  `aa20256` did not reproduce it, which at this rate does not distinguish the two;
- Linux CI's temp dir is already canonical, and every CI run passed it.

### Pre-release CI `34961010347` (`0acc120`, attempt 1): success

`fmt · clippy · test` on ubuntu, macOS and Windows (Windows security canaries,
Edge browser acceptance; Chrome browser acceptance and remote control
acceptance on ubuntu and macOS; installer checksum canaries and release payload
on ubuntu), `leveler-web UI · contract · typecheck · test · build`,
`leveler-mobile · analyze · test`, `deny · audit`.

### Release candidate `6bd4f78`

```text
local: clippy ok; cargo test --workspace 4114 passed, 0 failed, 20 ignored
leveler --version                      0.2.0-beta.3 (6bd4f78ce95c)
CI 34963007027 attempt 1               success (same six jobs)
```

Version: workspace package and every internal dependency pin in `Cargo.toml`;
`cargo update --workspace` changed only the 32 workspace package versions in
`Cargo.lock`. Current references updated in both READMEs, `install.sh`, the
Homebrew formula note and the release workflow comment; historical
`0.2.0-beta.2` references (its changelog section, eval notes, the version
ordering test) unchanged.

## 4. Release

| Item | Result |
| --- | --- |
| Tag | `v0.2.0-beta.3`, annotated, target `6bd4f78`, no force |
| Workflow | `Release` run `34964954244`, attempt 1: four builds + draft release success |
| Assets | expected 8, actual 8 (4 archives + 4 `.sha256`) |
| Checksums | all four match; the Windows `.sha256` ends in CRLF, so `shasum -c` on Unix reports the file name unreadable — the hash itself matches (same as beta.2) |
| Layout | `leveler` (+ `leveler-confine.exe` on Windows), `README.md`, `LICENSE-APACHE`, `NOTICE`; Mach-O arm64, Mach-O x86_64, ELF x86-64, PE32+ |
| Version smoke | macOS arm64 and macOS x86_64 (Rosetta) run: `leveler 0.2.0-beta.3 (6bd4f78ce95c)`; Linux and Windows not executed here, version string present in both binaries |
| Runtime identity | release arm64 `leveler tui` started its own daemon (`serve --ready-json`) and passed the build-identity check; header shows v0.2.0-beta.3 |
| Packaged agent smoke | temp home + temp repo with `.leveler/agents/release-smoke/`: `agents list`, `agents show`, `agents list --json` → discovered, project, available |
| GitHub release | "CodeLeveler v0.2.0-beta.3", draft false, prerelease true, not latest |
| Stable channel | `releases/latest` = `v0.1.4` before and after |

## 5. Residuals

None blocks this release.

1. `SubAgentFinished.contribution.profile_id` still names the capability class
   for a declared agent; no client reads it.
2. A `read_only` agent cannot run commands, but a model-written description can
   say it "runs tests"; runtime authority is unaffected.
3. CLI and TUI listings still say `available`; availability is a configuration
   check with no API-key check.
4. Shell commands can write `.leveler/agents/`; definitions are validated when
   loaded.
5. `stale_delete_rolls_back_earlier_commits_without_clobbering_external_changes`
   races on macOS (non-canonical lock path in the test).
6. The Windows release checksum file uses CRLF.
7. A very long `LEVELER_HOME` makes the daemon socket path exceed `SUN_LEN`; the
   TUI reports it and names `--in-process`.
8. codeleveler-b6 did not run a live-model PTY session of the new TUI command
   controls, and stopping a command is unconfirmed on a Windows host.
9. Automatic delegation value is not proven; no mobile agent editor; no
   marketplace or agent graph.
