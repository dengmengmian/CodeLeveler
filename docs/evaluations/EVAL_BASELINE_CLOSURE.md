# Eval Baseline Closure

Phase A of the Beta Product Closure. The performance investigation left a lab
configuration behind; this closes it, and closes the performance programme with
it.

## Verdict

```text
EVAL_REASONING_BASELINE = high
EVAL_BASELINE_CLOSURE   = PASS
PERFORMANCE_CLOSURE     = CLOSED
CORE_FREEZE             = UNCHANGED
```

## 1. Frozen object

| | |
| --- | --- |
| PRODUCT_HEAD | `98d21bcd843631b1dc130d39927d55487c80d9e5` |
| PRODUCT_TREE (`crates/`) | `cf5fa2b99d5226373f8a5f37d04a0708d3a2fa5e` |
| EVAL_HEAD | `1548db2ab362815ec5930d01136ffca85133682d` |
| Binary | `leveler 0.2.0-beta.1 (98d21bcd8436)`, sha256 `b5d4c312c957ea8b…` |
| Tracked product tree | clean |
| Untracked lab artifacts | none in the repo; the dogfood lab under `~/Develop/app/dengmengmian/dogfood` is outside version control by the outer repo's `app/*` ignore rule |

## 2. What changed, and what deliberately did not

The evaluation baseline moves from `reasoning_effort = "max"` to `"high"`, in
one file: the dogfood lab's CodeLeveler config. Nothing user-facing moves,
because there is nothing user-facing to move.

`reasoning_effort` has **no product default**. `ModelRequest::reasoning_effort`
is `Option<ReasoningEffort>` initialised to `None`, and
`resolve_reasoning_effort` returns `effective: None` when neither a request
override nor a model-config default exists — so a CodeLeveler installed with no
reasoning configuration sends no effort field at all and the provider decides.
`max` existed only as a lab convention. Changing it is an evaluation decision,
not a product change, and it is confined to:

```text
dogfood/config/codeleveler/config.toml      max -> high
dogfood/config/codeleveler/config.effort-max.toml   (new, the retired baseline kept runnable)
```

The retired file exists so `final-comparative-2026-09` and
`progressive-comparative-a1534eb` stay reproducible exactly as they were run.

The baseline applies to CodeLeveler only. AtomCode's reasoning effort is not
settable from its headless CLI at all, and DSH's patch configures none, so
there is no three-way effort convention to state.

## 3. Why `high`

- `max` showed a substantial model-side latency penalty on a controlled medium
  task. In the Stage 5 sweep of `progressive-comparative-a1534eb` — same
  binary, same fixture, one variable, two repetitions each — `max` and `high`
  produced the same round counts (10 and 9 both times) and the same token
  volume (ratio 1.00), and differed only by one pathological round per `max`
  run. Wall clock against AtomCode was 3.47x at `max` and 1.00x at `high`.
- `high` preserved rounds, tokens and correctness in that calibration.
- Long-task latency remains stochastic and the sweep did **not** reproduce
  there: on `yq-doc-count`, `high` was slower than `max` and round count moved
  with wall clock.
- `high` is an evaluation convention. It is not a claim that it is universally
  optimal, and it is not a recommendation to users.

## 4. Smoke

CodeLeveler only, one repetition each, at the new baseline. No competitor arm
was run.

| case | result | wall | rounds | tool calls | input | output |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `rust-first-even` | PASS | 63.7 s | 4 | 6 | 70,704 | 596 |
| `n3-caller-propagation` | PASS | 47.1 s | 8 | 13 | 160,804 | 2,230 |
| `icg-5-long-task` | PASS | 305.4 s | 13 | 28 | 420,568 | 16,076 |

Every run: `stop_class = Completed`, structured completion claim, hidden tests
passed, 0 false completions, 0 user rescues, 0 retries. Tool lifecycle events
paired exactly (6/6, 13/13, 28/28) with one `task_started`/`task_finished` and
one `turn_started`/`turn_finished` per run — no lost events.

Time decomposition, from `model_requests.latency_ms` and `events.created_at`:

| case | model wait | tool execution | harness overhead |
| --- | ---: | ---: | ---: |
| `rust-first-even` | 96.9% | 2.7% | 0.44% |
| `n3-caller-propagation` | 90.8% | 9.0% | 0.24% |
| `icg-5-long-task` | 98.1% | 1.7% | 0.19% |

## 5. What the smoke does not prove

`n3-caller-propagation` fell from 149.0 s and 15 rounds at the previous freeze
to 47.1 s and 8 rounds here. **Three things changed between those two runs** —
the effort baseline, `98d21bc` (the workspace map in the first request) and
`369283e` (the `update_goal` fix) — so no single cause can be assigned. The
number is recorded, not attributed.

Nor does `high` eliminate slow rounds. `icg-5-long-task` still carried rounds
of 158.5 s, 38.4 s and 37.7 s, and `rust-first-even`'s first round took 39.9 s,
all with zero retries. The controlled sweep removed the outliers on `n3`; the
pattern persists elsewhere. Time to first byte held at 3.5-8.0 s median.

## 6. Gate

| check | result |
| --- | --- |
| `cargo fmt --check` | clean |
| `cargo check --workspace --all-targets --all-features` | clean |
| `cargo clippy … -- -D warnings` | clean, 0 warnings |
| `cargo test --workspace --all-features --locked --no-fail-fast` | 3,518 passed, 0 failed, 6 ignored |
| Simple / Medium / Hard | PASS / PASS / PASS |
| False success | 0 |
| Runtime crash | 0 |
| Lost events | 0 |
| Wrong terminal | 0 |

`EVAL_BASELINE_CLOSURE = PASS`. The performance programme is closed. No further
performance investigation is opened on the strength of these numbers.

Evidence: `evals/baselines/beta-phaseA-98d21bc/`.
