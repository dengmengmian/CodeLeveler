# Beta Closure — finalization

**Product candidate: `a00c60c`.** Branch `beta-closure/unattended`. The branch HEAD moves past this
SHA for documentation commits; the *product* candidate is `a00c60c` and nothing after it changes
code.

**`OPEN_BETA_BLOCKER = 0`. `OPEN_BETA_REQUIRED = 0`. `BETA_CLOSURE_CANDIDATE = READY`.**

## What this round closed

| Item | Result |
| --- | --- |
| R007-F4 · TUI contrast and default theme | `VERIFIED_FIXED` — `a00c60c` |
| Control preflight killed unrelated daemons | fixed in the control repo; product untouched |
| N8 · build artefacts in `modified_files` | reclassified `DEFER_POST_BETA`, reasoning recorded |

R007-F4 was implemented here as a local minimal fix. A concurrent session had written equivalent
theme work, but it was never committed, and uncommitted work is not a fix — every branch, worktree,
stash, reflog and dangling commit was checked first.

## R007-F4 in one paragraph

`ThemeId` had no default — it had a first array element, re-spelled as `ThemeId::Ion` at four
fallback sites. Body text was a hardcoded near-white in ion/night and near-black in day, so a user
on the opposite background read text against its own colour, and with no explicit default they got
that without ever choosing a theme. Body-text roles now inherit the terminal foreground in every
named theme, so readability no longer depends on which theme is default; `ThemeId::DEFAULT` (Day) is
the single explicit answer all four fallbacks read. Semantic roles — accent, heading, success,
warning, error, diff, tool, muted, dim — are deliberately unchanged and guarded by a test.

## Validation

`cargo test --workspace --all-targets --no-fail-fast` 2811 passed / 0 failed ·
`cargo clippy --workspace --all-targets -- -D warnings` clean · `cargo fmt --all --check` clean ·
F4 PTY contrast smoke 6/6 across all three palettes · preflight scope regression 6/6 ·
P0 input integrity byte-identical · P1/P3/P4 instruments PASS · Recovery Truth 4/4 ·
toolchain-environment daemon smoke 3/3.

The delta from `90d06db` is three files, all in `leveler-tui`, so the F6 (12/12) and reviewer-launch
(5/5) evidence gathered at `90d06db` stands and was not re-run — a stated exemption, not an
omission.

## Limits that remain true

- The harness reviewer runs on Direct tasks only, and proves a review *happened*; its findings are
  persisted and shown, but nothing parses them.
- N4 — whether a live model acts on `INCOMPLETE_NO_RESULT` — is behavioural and unproven.
- N8 — build artefacts reach the review brief and the review trigger's file count.
- One live task exercised the reviewer. That is proof level C, not a trend.

## Status

Not merged to `main`. No tag. No release. Awaiting a single human review.
