# Published-binary performance A/B: v1.0.27 vs v1.0.28

Classification: **INCONCLUSIVE**. No regression was assumed; none is
confirmed. Method was fixed before reading the final arm.

## Controls

| Control | Value |
| --- | --- |
| A binary | published v1.0.27 macOS arm64, SHA-256 `ab841c8379832e0e6520547f44b36f5f5cb4b9660793605355e5320dea43384b` |
| B binary | published v1.0.28 macOS arm64, SHA-256 `5022619d7dda605cc8b57bcd7c5d3b27e409dd56460ccd516efb81592dc9615e` |
| Cases | 23 Release Lab L3 cases, run `20261121-113252-c86573f209ca` |
| Model | `deepseek/deepseek-flash`; thinking and reasoning_effort `max` |
| Config | identical lab `config.toml` per fresh `LEVELER_HOME`; auto-update disabled |
| Project | fresh empty working directory per arm; cases create their own fixtures |
| Order | A B B A A B (three arms per side); adjacent pairs damp drift |
| Machine | Apple M4 Max, macOS 26.6.2, same host |

The provider exposes no pinnable server-side revision. The client pins model
id and request parameters only; provider-side drift cannot be excluded.
Feasibility runs preceded the measured arms and are excluded from the analysis;
only A1–B3 enter the statistics (`feasibility_runs_excluded: true` in
`evidence/ab-summary.json`).

## Arm aggregates

| Arm | Binary | Quality | wall p50 s | wall p95 s | rounds p50 | input p50 | output p50 | total p50 | tools |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A1 | v1.0.27 | 100 | 21.74 | 242.34 | 7 | 111,504 | 2,330 | 113,794 | 328 |
| B1 | v1.0.28 | 100 | 26.68 | 183.91 | 8 | 132,600 | 2,960 | 134,697 | 373 |
| B2 | v1.0.28 | 100 | 23.09 | 217.29 | 7 | 118,671 | 2,980 | 121,586 | 363 |
| A2 | v1.0.27 | 100 | 24.23 | 155.52 | 8 | 126,744 | 2,633 | 128,668 | 334 |
| A3 | v1.0.27 | 100 | 23.68 | 176.16 | 8 | 130,400 | 2,723 | 133,231 | 359 |
| B3 | v1.0.28 | 98 | 26.04 | 217.98 | 8 | 135,956 | 2,848 | 138,787 | 399 |

Between-arm dispersion is 10.5–13.8% on wall p50 and 14.4–19.6% on wall sum.

## Paired result (B vs A, per-case medians)

| Metric | Point | 95% bootstrap CI | Cases up / down | Sign-test p |
| --- | --- | --- | --- | --- |
| wall | **+8.5%** | **[−6.6%, +24.3%]** | 13 / 10 | 0.6776 |
| rounds | 0.0% | [−12.5%, +14.3%] | 9 / 11 | 0.8238 |
| total tokens | −1.9% | [−5.8%, +6.2%] | 11 / 12 | 1.0 |
| input tokens | −2.0% | [−6.3%, +5.9%] | 11 / 12 | 1.0 |

Clock-adjacent pairs: A1→B1 +10.9%, A2→B2 +2.0%, A3→B3 +3.5% wall; rounds
0.0% in all three.

## Attribution

Median log decomposition: wall +8.5%, of which rounds 0.0% and per-round
latency +6.0%. Input per round +4.7%. Tool calls rose from 328–359 to 363–399
per arm while total tokens fell slightly. No single mechanism accounts for the
point estimate; per-case wall changes range from −33.1% to +66.2%.

## Quality veto

A: 69/69 cases passed, quality 100 in all three arms. B: 68/69 passed. One B3
case (`realrepo-normalize-dialog`) ended incomplete (rounds 13, 90.3 s); its
own verification passed (exit 0) but the model stopped before declaring
completion. One event in 69 B runs and zero in 69 A runs; it is not attributed
to the binary, but under the pre-registered rule it vetoes an improvement or
parity claim.

## Supplemental process measures

| Dimension | v1.0.27 | v1.0.28 |
| --- | --- | --- |
| CLI startup p50 / p95, ms | 5.90 / 6.29 | 5.81 / 6.60 |
| `--version` peak RSS, KiB | 14,640 | 14,512 |
| one-turn reply wall p50, s | 1.59 (3/3) | 1.56 (3/3) |
| one-turn peak process-tree RSS p50, KiB | 32,448 | 33,488 |
| TUI first paint p50 / p95, ms | 111 / 167 | 110 / 166 |
| TUI `/help` render p50, ms | 758 | 757 |

## Supplemental work counters

Descriptive only; no per-counter significance test was pre-registered. Each
side is 69 case-runs (23 cases × 3 arms) with identical cases and inputs, from
`evidence/ab-summary.json`.

| Counter | v1.0.27 A | v1.0.28 B |
| --- | ---: | ---: |
| TTFF p50, median of arm p50s | 72 ms | 75 ms |
| model time per case p50, median of arm p50s | 20.4 s | 22.8 s |
| cached input p50, median of arm p50s | 103,168 | 108,160 |
| cached share of input (mean of arm shares) | 90.5% | 90.2% |
| tool calls (sum) | 1,021 | 1,135 |
| edit failures | 1 | 1 |
| verification failures | 12 | 16 |
| recovery rounds | 40 | 58 |
| repeated reads | 49 | 68 |
| repeated searches | 3 | 2 |
| runtime-forced continuations | 51 | 40 |
| goal interceptions | 19 | 26 |
| compactions | 0 | 0 |
| false completions | 0 | 0 |

Retries are not separately instrumented; recovery rounds are the available
proxy. Tool-result byte size is not carried by the artifact, so it is not
attributed. CPU was not captured as a per-arm series (the startup
`process_resources` sample is 0.0 user/sys at `--version` granularity); the
soak samples TUI CPU for v1.0.28 only. That is a measurement gap, not a result.

## Why not the other labels

- Not `CONFIRMED_REGRESSION`: CI includes zero, p=0.68, point +8.5% is below
  the +10% rule, and B3 quality fell.
- Not `NO_SIGNIFICANT_DIFFERENCE`: the CI [−6.6, +24.3] is not wholly inside
  ±10%.
- Not `CONFIRMED_IMPROVEMENT`: the point estimate is positive and quality
  fell.

Confounders: no provider revision pin; A3/B3 ran while host load was ~14–17
from another session; provider cache/scheduling can shift wall without a code
change (input is 98%+ cached within a case).
