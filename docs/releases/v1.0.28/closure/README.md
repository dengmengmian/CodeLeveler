# v1.0.28 post-release quality closure

Recorded 2026-11-22. This is a **post-release audit of the published v1.0.28
package**, not a new release gate and not a v1.0.29 preparation. The original
release verdict — **CONDITIONAL PASS**, state `RELEASED` — is unchanged. A
closure result cannot rewrite it.

| Item | Value |
| --- | --- |
| Published release | v1.0.28, tag `v1.0.28`, commit `6464904bdf6ae1abaa03dda3649e42549e0cb361` |
| Original gate | CONDITIONAL PASS (4/4 stages, RC conditional) |
| Closure status | **CONDITIONAL_CLOSED** |
| Confirmed regression | none |
| Confirmed product defect fixed | none |
| Platforms accepted | macOS arm64 34/34; macOS x86_64, Linux x86_64, Windows x86_64 blocked (no host) |
| Performance A/B | INCONCLUSIVE (paired wall +8.5%, 95% CI −6.6%…+24.3%) |
| Flake | FLAKY_UNRESOLVED (1,320 closure runs + 450 prior, 0 failures) |
| Gap audit | 15 of 50 declared gaps closed with new process-level tests; 35 remain open |
| 24-hour soak | IN_PROGRESS; not PASS before 24 h |

## Phase results

| Phase | Area | Status | Evidence |
| --- | --- | --- | --- |
| A | release identity, L3/L4 definitions, token accounting, gaps, warnings | PASS | `phase-a-audit.md` (dev tree) |
| B | published v1.0.27 vs v1.0.28 performance, interleaved A/B | INCONCLUSIVE | [`performance.md`](performance.md) |
| C | published-package platform acceptance | PARTIAL — macOS arm64 PASS, three hosts blocked | [`platforms.md`](platforms.md) |
| D | steer/exit flake investigation | FLAKY_UNRESOLVED | [`stability.md`](stability.md) |
| E | 24-hour soak | IN_PROGRESS | [`stability.md`](stability.md) |
| F | declared-gap audit and offline closure | PASS (15 closed / 35 open, never counted as pass) | [`coverage.md`](coverage.md) |

## Why the closure is conditional, not closed

- The 24-hour soak had not reached 24 hours when this report was written. No
  stability PASS is claimed.
- Windows, Linux and Intel macOS have no attached host. Their published
  packages were hash-verified but not executed; no cross-compile or CI result
  is used as acceptance.
- The published-binary A/B is too wide to separate the arm effect from
  provider drift and host load. It neither confirms a regression nor proves
  parity.
- The historical 20 s test timeout did not reproduce in 1,320 controlled runs;
  it remains unresolved rather than fixed or declared innocent.
- 35 declared feature gaps are still open (6 external-account, 2 installer,
  10 TUI slash, 4 OS scheduler, 13 extended product). They were not counted as
  passing.
- The 15 gap-closure tests are uncommitted in the working tree; they enter the
  regular regression only after an authorized commit.
- The published package's own `CONDITIONAL PASS` conditions remain in force.

## Product fixes

None. No confirmed product defect was found in the published v1.0.28 binary,
so no candidate-tree fix is claimed and no v1.0.29 emergency patch is
recommended by this closure. Three harness defects were found and fixed in the
closure tooling only; they are recorded in [`fixes.md`](fixes.md) and do not
change product code or the published verdict.

## Evidence

The structured evidence is in `post_release.json` (`quality_closure`,
`platform_matrix`, `performance_ab`, `coverage_closure`, `flake`, `soak`).
Raw and intermediate evidence is under the git-ignored development tree
`docs/dev/v1.0.28-post-release/`; the public summaries in this directory link
to the exact files and identities. The release identity and all asset digests
are unchanged.
