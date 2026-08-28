# HC-002 Task Contract

Frozen **before any paid HC-002 run**. Do not edit acceptance after seeing results.

Paid execution authorized on freeze `7a263e93` (contains reconciliation `f759ff4a`). The HC-001 freeze `3b400357` is obsolete for this case.

Qualification: `docs/evaluations/HC-002-QUALIFICATION.md`

| Field | Value |
|---|---|
| HC002_CASE_ID | `icg-5-long-task` |
| source | `evals/icg/icg-5-long-task.yaml` |
| repo | `fixtures/repos/navsvc` @ `4ff2da88655ff83466d6653e0b79dc873467b6bd` plus YAML overlay (none for this case) then `eval baseline` commit |
| timeout | **1800 seconds** wall clock, identical for all 6 runs |
| allowed environment | Go toolchain; no network requirement for the judge; unattended harness permissions as in HC-001 |
| prompt | YAML `task:` field, byte-for-byte, no harness-specific suffix |
| prompt sha256 | `b52d7fefe9c8bb01b8c25b6cf69affffa8a1dce2dc90ff3f8e10308c0d1e4ce0` |

## Prompt (canonical)

```
The observability team's backlog, to land together:

1. Records the decoder marked not valid must not appear as summary rows
   at all — though they still count in the stats of item 4.
2. When a record carries an `env` label, its summary row is reported
   separately as `<name>{env=<value>}`; records without the label keep
   their plain name. Rows stay sorted by the name they are printed
   under; the `count=`/`total=` format is unchanged.
3. The summary ends with one extra line `TOTAL count=<sum> total=<sum>`
   summing every row above it — present (with zeroes) even when nothing
   was observed.
4. A `--stats` flag: with `--summary --stats`, after everything else,
   print exactly one final line `records=<N> invalid=<M> distinct=<K>`
   where N counts every record that reached the report stage, M counts
   those the decoder marked not valid, and K counts distinct incoming
   record names among valid records — grouping from item 2 does not
   change K. Without `--stats`, output is unchanged.

Do not change existing tests. `go build ./...` and `go test ./...` must pass.
```

## Visible obligations

- Four asks land together; later work must not destroy earlier work.
- Invalid records: absent from rows, still counted in stats.
- `env` grouping; sort by printed name; format `count=` / `total=` unchanged.
- `TOTAL` after rows, including empty/zero.
- `--stats` last line `records=… invalid=… distinct=…` with the stated N/M/K rules; absent without the flag.
- Do not change existing tests.
- `go build ./...` and `go test ./...` must pass.
- Do not edit `legacy/`.

The prompt names **no file** and does **not** mention agents, plans, compact, or background commands.

## Hidden verification (NOT given to the agent)

The case `expect` is the judge. After the harness exits it:

1. Builds `./cmd/navsvc`.
2. Feeds a four-record jsonl the agent never sees:
   - `req` / prod / 1 (valid)
   - `req` / dev / 4 (valid)
   - `plain` / 9 (valid, no env)
   - `g` / `"zz"` (invalid)
3. With `--summary --stats`, requires **exact** lines:
   - `plain count=1 total=9`
   - `req{env=dev} count=1 total=4`
   - `req{env=prod} count=1 total=1`
   - no row starting `g `
   - `TOTAL count=3 total=14`
   - last line `records=4 invalid=1 distinct=2`
   - second-to-last is the TOTAL line
4. Without `--stats`: no `records=` line.
5. `git diff --quiet -- legacy/`
6. `go test ./...`

`hidden_passed` for HC-002 **is** `expect` exit 0.

This oracle checks **behavior**, not function names or architecture.

## Plausible wrong fixes (PARTIAL vs FAIL)

Maintained `go test ./...` stays green for all of these (tests use `Valid: true` and no `env`). Hidden expect does not.

| Incomplete implementation | Hidden symptom |
|---|---|
| Skip invalid in `Observe` so they also vanish from stats | `records=` / `invalid=` / `distinct=` wrong |
| Group `env` but count K on grouped keys | `distinct=3` instead of `2` |
| TOTAL includes the invalid row | `TOTAL` not `count=3 total=14` |
| Stats line always printed | no-stats path still has `records=` |
| TOTAL after stats, or stats not last | tail-2 / tail-1 checks fail |
| `--stats` not wired in `cmd/navsvc` | cannot produce the stats line |
| Pipeline `filter` on by default instead of report hygiene | rows may look right; stats N/M and existing pipeline tests may also fail |
| Edit `legacy/` | `git diff --quiet -- legacy/` fails |

PARTIAL (frozen): `expect` ≠ 0, but the diff clearly implements at least two of the four asks in the right direction (evidence: matching subset of the CLI checks, or an obvious `env` grouping + validity skip with stats/K still wrong). Record from diff+expect, not from speech.

A local-only n3-style skip that claims “done” is **false completion** if expect is red.

## PASS / FAIL labels

| Label | Rule |
|---|---|
| PASS | `expect` 0; baseline was red; `legacy/` untouched |
| PARTIAL | as above |
| FAIL | harness finished; acceptance not met; not PARTIAL |
| TIMEOUT_FAIL | SIGKILL at 1800s and `expect` ≠ 0 |
| INFRA_FAILURE | adapter/provider/eval environment |
| UNJUDGEABLE_BASELINE_GREEN | abort |

## Forbidden shortcuts

- Editing maintained tests
- Editing `legacy/`
- Relying on pipeline `filter` as the only validity mechanism
- Changing this rubric after seeing outputs
- Harness-specific prompt suffixes

## Token ranking

```
TOKEN_FAIRNESS=LIMITED
TOKEN_RANKING=DISABLED
```

Collect raw reported tokens. Do not rank efficiency by them. Wall clock is the comparable efficiency number.

## Run matrix (prepared, not launched)

1 case × 3 harnesses × 2 reps = 6.

Order:

1. CodeLeveler r1
2. AtomCode r1
3. DSH r1
4. DSH r2
5. AtomCode r2
6. CodeLeveler r2

Machine schedule: `eval/comparative/results/hc-002-prepare-evidence/run-manifest.json` (written by `--prepare-only`).

## Harness identities (unchanged from HC-001 except CodeLeveler SHA)

| Harness | Identity |
|---|---|
| AtomCode | `5.0.9` / `52ca5e6` · `~/.local/bin/atomcode` · real `~/.atomcode/config.toml` · `-p -C -y -v --dev --no-telemetry` |
| DSH | `0.1.2-alpha.1` / `cd5ef8148` · `~/Develop/app/other/deepseek-harness` · isolated `DSH_HOME` · `danger-full-access` |
| CodeLeveler | `7a263e931a4f3907c1a05d7407413d9e6a722924` (includes `f759ff4a`) · `eval/comparative/results/bin/leveler-7a263e93` · `leveler 0.2.0-beta.1 (7a263e931a4f)` · `assisted + --auto-approve` |

Permissions: `assisted + --auto-approve` / `-y` / `danger-full-access`. `PERMISSION_FAIRNESS=ACCEPTABLE`.

`MODEL_UPSTREAM_MATCH=PARTIAL` (same as HC-001).

## Adapter

`--repo` / `-C` / `DSH_HOME` / `LEVELER_HOME` must be absolute. `ADAPTER_PATH_REGRESSION=PASS` is a prepare-only gate.
