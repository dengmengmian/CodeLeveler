# Published-package platform acceptance

PASS requires the published binary running on a real host of that platform.
Cross-compilation, Rosetta, CI logs and source builds are not acceptance.

| Platform | Status | Basis |
| --- | --- | --- |
| macOS arm64 | **PASS** | published payload, Apple M4 Max host, 34/34 checks |
| macOS x86_64 | **BLOCKED_ENVIRONMENT** | no Intel host; Rosetta smoke not accepted as native |
| Linux x86_64 | **BLOCKED_ENVIRONMENT** | no x86_64 Linux host; `accept_linux.sh` provided |
| Windows x86_64 | **BLOCKED_ENVIRONMENT** | no Windows host; `accept_windows.ps1` provided |

## Identity

| Platform | Package SHA-256 | Extracted binary SHA-256 |
| --- | --- | --- |
| macOS arm64 | `c4d2a1b8aef92d2b536f32d1b034a3322e88982f2994782119fc061a3e3ca5d5` (tar.xz) | `5022619d7dda605cc8b57bcd7c5d3b27e409dd56460ccd516efb81592dc9615e` |
| macOS x86_64 | `3b8bb71267394cb6923a346e860b01b707f0b0ce333092d5b1e2f50b812edc1f` (tar.gz) | `d4444279fba3760753a3b36e0ce1ee391d359be050a1d95e0f585df24d2a0417` |
| Linux x86_64 | `2cd0dbbe4975e7e4afedee011c02b4ceef8273ae11f78719a8679c43bc3210b3` (tar.gz) | `787e9cb3259ba2ebcdd790cc616cc6b6251fa72d6ad663700159fea4f51ed386` |
| Windows x86_64 | `eeac133f12edadf25a0c696ac5babcebb8fdae4111440b72838cf9de3fef89b9` (zip) | `eb73498f8c3518b145fbc89f710d3b11d6d48b72c78f47aae7853beac7ec24e2` |

All payloads were downloaded from the public GitHub Release. v1.0.27 arm64
(archive `2a681b9f…`, binary `ab841c83…`) was re-downloaded for the upgrade
check. Every digest matched before execution.

## macOS arm64 — 34/34 PASS

Run 2026-11-22T12:54:21Z–12:56:22Z, isolated `HOME`/`LEVELER_HOME`, real provider.

| Check group | Result |
| --- | --- |
| Package and installed binary SHA-256; `1.0.28 (6464904bdf6a)` | PASS |
| 10 CLI startup samples (median 6 ms) | PASS |
| Fresh-home surfaces: config, sessions, models, permissions, background journal, doctor | PASS |
| Real PTY TUI: first paint 1,225 ms, composer, `/help`, one provider prompt, exit 0 | PASS |
| Real tool turn with file side effect; session list/show/JSON | PASS |
| PTY resume of the persisted session (first paint 700 ms) | PASS |
| Goal turn; Plan turn (workspace unchanged) | PASS |
| Background success and non-zero exit surfaced | PASS |
| Permission boundary: request-approval write to `/var/tmp` denied on closed stdin, no file, no hang (`rc=3`) | PASS |
| Unknown model and rejected credential are typed errors, not panics | PASS |
| Upgrade from published v1.0.27: config preserved, prior session readable, new turn under v1.0.28 | PASS |

### Permission-boundary correction

The first run scored 33 PASS / 1 FAIL because its probe targeted `/tmp`.
A `request-approval` turn wrote `/tmp/...` with exit 0 and no approval. Source
inspection showed `/tmp` is inside the documented write boundary:
`request_approval` maps to `WriteScope::Workspace` ("this root plus the
standard temp / cache dirs"), and the sandbox and approval classifier share
`shared_temp_write_roots()`. The test premise was wrong, not the product. The
original observation and the manual reproduction are preserved; the corrected
probe uses `/var/tmp`, which the profile explicitly excludes, and passes.

This is not counted as a product defect. It is recorded because the initial
run's failure must not disappear from the record.

## Blocked platforms

Runnable scripts are in `docs/dev/v1.0.28-post-release/scripts/`:

- macOS x86_64: `accept_macos.py --expected-sha d4444279…` on a real Intel Mac.
- Linux x86_64: `accept_linux.sh` (bash, python3, pyte); records BLOCKED when
  pyte or the previous binary is absent.
- Windows x86_64: `accept_windows.ps1` in non-elevated PowerShell 7;
  `-ExerciseScheduler` covers Task Scheduler registration.

No result on one platform is inferred from another. Goal/Plan crash recovery
is not part of the platform script; the soak driver runs abnormal exit, session
read and `run --resume` separately.
