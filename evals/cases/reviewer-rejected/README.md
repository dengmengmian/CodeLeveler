# Rejected calibration candidates

Kept, not deleted: a case that measured at ceiling is a result about the
model, and re-authoring it later would repeat the spend.

| Case | Control | Why rejected |
| --- | ---: | --- |
| `go-safepath-escape` | 3/3 | Path confinement is not a blind spot for this model. The trap (bare `strings.HasPrefix`) discriminates against a hand-written wrong implementation, but the model wrote the separator-aware check unprompted in all three runs. |
| `go-config-legacy-compat` | 3/3 | Same. Absent-versus-zero is a named pattern; the model reached for pointer fields every time. |

Both cleared the discriminance gate. Neither has headroom.

See `docs/evaluations/MA-VALUE-REVIEWER-TASK-CALIBRATION.md`.
