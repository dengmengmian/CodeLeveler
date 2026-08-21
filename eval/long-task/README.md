# Long-task evaluation (confirmatory)

Hours, not minutes. Run only after a micro batch shows a movement worth the
cost. Hidden acceptance stays in the private control plane.

| id | dir | frozen case | workspace |
| --- | --- | --- | --- |
| LONG_A | `cargo/` | xsv four commands | `codeleveler-dogfood/maa/WA-xsv` |
| LONG_B | `miller/` | miller six verbs | `codeleveler-dogfood/maa/B-miller` |
| LONG_C | `yq/` | yq three encoders | `codeleveler-dogfood/maa/A-yq` |

`cargo/` is the **Rust CLI / cargo-build** shape (frozen LONG_A = xsv), not
the rust-lang/cargo repository.

## Run

```sh
export CONTROL_ROOT="${HOME}/Develop/codeleveler-dogfood-control"
export DOGFOOD_ROOT="${HOME}/Develop/codeleveler-dogfood"
eval/long-task/scripts/run_long.sh LONG_A --model deepseek/deepseek-v4-flash
```

Score the resulting `LEVELER_HOME` with the same EventLog parser:

```sh
python3 eval/scripts/run_micro.py score \
  --home "$CONTROL_ROOT/ma-wa1-frozen-main-reproducibility/homes/EA-1" \
  --out eval/reports/long-a-ea1.json \
  --suite long-task --max-rounds 280
```

Do not copy miller/yq/xsv into this git tree. Do not paste hidden verifiers
here. `manifest.yaml` is the pointer.
