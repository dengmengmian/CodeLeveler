# HC-002 Qualification

Prepared while Completion Reconciliation is still open. **No paid HC-002 run has been launched.**

```
HC002_CANDIDATE=icg-5-long-task
EXPECTED_COMPLEXITY=HIGH
EXPECTED_FILES_TOUCHED=2-4
EXPECTED_RELEVANT_MODULES=4
MULTI_HYPOTHESIS_OPPORTUNITY=YES
VERIFICATION_DEPTH=HIGH
DELEGATION_OPPORTUNITY=YES
EXPECTED_HARNESS_DISCRIMINATION=MEDIUM
HC002_PAID_RUNS=HOLD
```

MEDIUM is enough to proceed once a new CodeLeveler SHA is accepted. It is not inflated to HIGH: the repo is the same small `navsvc` as HC-001, and a strong `deepseek-v4-flash` pass may still finish all four asks in minutes. Discrimination is expected from **interaction failures and trajectory**, not from guaranteed PASS/FAIL splits.

If the six paid runs look like another n3 (same patch, same ~1 minute, same path), set `HC002_HAS_DISCRIMINATION=NO` and stop. Do not open Phase B.

---

## Why not n3

HC-001 (`n3-caller-propagation`) executed cleanly and discriminated nothing: 6/6 PASS, 37–62s, same two-line validity skip. Scoring set EXCLUDED.

## Why icg-5 rather than yq / icg-6r

| Candidate | Role | Verdict |
|---|---|---|
| `icg-5-long-task` | four interacting product asks, no file named, later work can destroy earlier work | **HC-002 primary** |
| `yq-doc-count` | 477-file real repo, discover a flag | fallback if icg-5 is still too easy; better as search / false-negative probe than as the general discriminator |
| `icg-6r` | impossible-task honesty | **forbidden** as HC-002 engineering case |
| `scale-s800` | HIGH delegation | too large for a first discriminator; keep for Phase B mix |

## What the case actually is

Not a one-bug hunt. A backlog of four obligations that share one rendering path:

1. Invalid records must not be **summary rows**, but they **still count in stats** (item 4).
2. `env` label splits the row key to `<name>{env=…}`; sort by printed name; `count=`/`total=` format unchanged.
3. A `TOTAL count=… total=…` line after the rows (zeroes allowed when empty).
4. `--stats` prints `records=<N> invalid=<M> distinct=<K>` last; N includes invalid; K is distinct **incoming** valid names; grouping must not change K; without the flag, output unchanged.

Prompt names **no file**. Existing `go test` only pins valid-only ungrouped counting (`TestSummaryCountsPerName`). `legacy/` is a forbidden distractor. Pipeline `filter` is opt-in (`DropInvalid` is not in defaults).

## Qualification checklist

| Requirement | icg-5 |
|---|---|
| Multi-round exploration | Yes — CLI flag, report package, decoder `Valid`, labels |
| Cross-module | Yes — `internal/report` + `cmd/navsvc` |
| 3–5 relevant files | `summary.go`, `main.go`, possibly `aggregate.go` / types |
| Multiple hypotheses | Yes — see wrong-fix table in the contract |
| Non-obvious root cause | Interaction, not a hidden line: invalid-in-stats vs invalid-in-rows; K vs grouped keys |
| Implementation + regression | `go build` / `go test` plus CLI oracle |
| Initial wrong hypothesis | n3-style “skip invalid everywhere” makes maintained tests still green and **breaks stats** |
| Length for divergence | 1800s budget; 10–30 minute-class work; faster finish is allowed |
| Verification depth | High — four interactions, no-stats path, `legacy/` fence |
| Optional delegation | Yes (MEDIUM) — four asks look splittable; they share one renderer, so naive spawn can clobber |

Not solvable by grep → one file → two lines → one test.

## Why discrimination is only MEDIUM

- Same small fixture as n3. Strong models can read a numbered spec and implement it.
- Native CodeLeveler ICG history includes this case in a green suite; solvability is not in doubt.
- The **oracle** is still discriminative: partial implementations fail hidden CLI checks while `go test ./...` stays green.
- Trajectory (search, re-reads, verify `--stats` vs not, extra tests, spawn) can discriminate even on a 6/6 PASS.

## Fallback if paid HC-002 is still too easy

Do not force a Phase B matrix. Next discriminator: `yq-doc-count` (search-heavy, unnamed files) or a real third-party bug, not another navsvc overlay.

## Current Beta state (do not ignore)

```
EXISTING_CODELEVELER_BETA_REQUIRED=1   # icg-6r Completion Truth
OPEN_BETA_REQUIRED=1
HC002_PAID_RUNS=HOLD
```

Working tree currently has **uncommitted** Completion Reconciliation files (`crates/leveler-agent/src/reconciliation.rs` and related). That is CC’s lane. GORK does not treat dirty `main` as an Eval baseline.
