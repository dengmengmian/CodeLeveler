# Fix report

## Product fixes

**None.** This closure found no confirmed defect in the published v1.0.28
package that requires a code change. No candidate-tree fix is claimed, and the
published package's verification result is unchanged.

Consequently this closure does **not** recommend an emergency v1.0.29 patch on
defect grounds. The remaining conditions (blocked platforms, incomplete soak,
inconclusive A/B, unresolved flake, 35 open gaps) are release-scope decisions,
not defects proven against v1.0.28.

## Harness corrections (not product fixes)

| Item | Original observation | Root cause | Correction |
| --- | --- | --- | --- |
| macOS acceptance permission probe | 33 PASS / 1 FAIL: `request-approval` wrote `/tmp/...` with exit 0 and no approval | Probe premise was wrong. `/tmp` is inside `WriteScope::Workspace` ("this root plus the standard temp / cache dirs"); approval classifier and sandbox share `shared_temp_write_roots()`. `/var/tmp` is the excluded path | Probe targets `/var/tmp`; full run 34/34. Original observation and manual reproduction preserved |
| Soak TUI start | First one-cycle smoke: `tui_start` FAIL although `/help` and a steer were captured | Soak process did not set `LEVELER_LANG=zh`; composer predicate knew only the Chinese placeholder | Set the lab language before PTY spawn and accept both shipped placeholders; second smoke 9/9 |
| Soak process trend | First long run's resource sample reported 38 `leveler` processes | Counter matched every `leveler` process on the host, including unrelated acceptance/eval daemons | Scope the count to the soak lab's paths; stop the first run in cycle 0 and restart. Aborted heartbeat kept under `/tmp/pr28-soak-run0-aborted-20261122T131456Z/` |

All are changes under `docs/dev/v1.0.28-post-release/scripts/` only. No
product source, test assertion, timeout or security contract was changed.

## Observations that are not defects

- **Shared temp is writable by design.** Under `request-approval`, a write to
  `/tmp` succeeds without approval because the profile's write scope includes
  the platform temp directory. This is consistent between the approval
  classifier and the sandbox. Users who read "write confined" as
  "workspace-only" may be surprised; documenting the temp/cache roots in the
  user-facing permission description would remove the ambiguity (suggestion,
  not a required fix).
- **One incomplete case in the B3 performance arm.** `realrepo-normalize-dialog`
  stopped before declaring completion although its verification passed. One
  event in 69 B runs and zero in 69 A runs cannot be attributed to the binary;
  it is a quality veto on the A/B, not a confirmed product defect.
- **The historical steer/exit timeout** did not reproduce in 1,320 controlled
  runs. No speculative patch and no timeout increase was applied.

## Verification identity

| Work | Identity |
| --- | --- |
| Published v1.0.28 under test | commit `6464904bdf6a`, macOS arm64 binary `5022619d…` |
| Candidate tree for Phase D/F tests | `main` at `153f0e19`, test binary `e4606d80…` |
| Acceptance run | 2026-11-22T12:54:21Z–12:56:22Z, 34/34 |
| Flake stress | 1,320 runs, 0 failures, 0 timeouts |
| Soak | restarted 2026-11-22T13:15:52Z, IN_PROGRESS at report time |
