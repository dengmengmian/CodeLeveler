#!/usr/bin/env bash
# Observer wrapper around frozen LONG_A/B/C. Does not touch product runtime.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CONTROL_ROOT="${CONTROL_ROOT:-$HOME/Develop/codeleveler-dogfood-control}"
TASK="${1:?LONG_A|LONG_B|LONG_C}"
shift || true
MODEL="${MODEL:-deepseek/deepseek-v4-flash}"
BIN="${BIN:-leveler}"

case "$TASK" in
  LONG_A) CASE_REL="ma-wa1-taskset-requalification/cases/long-a" ;;
  LONG_B) CASE_REL="ma-wa1-taskset-requalification/cases/long-b" ;;
  LONG_C) CASE_REL="ma-wa1-taskset-requalification/cases/long-c" ;;
  *) echo "unknown task $TASK" >&2; exit 2 ;;
esac

CASE_DIR="$CONTROL_ROOT/$CASE_REL"
if [ ! -d "$CASE_DIR" ]; then
  echo "missing $CASE_DIR (set CONTROL_ROOT)" >&2
  exit 2
fi

BATCH_ID="${TASK}-$(date -u +%Y%m%dT%H%M%SZ)"
HOME_DIR="$ROOT/eval/runs/$BATCH_ID/home"
OUT_DIR="$ROOT/eval/runs/$BATCH_ID"
mkdir -p "$HOME_DIR" "$OUT_DIR"
export LEVELER_HOME="$HOME_DIR"
export LEVELER_EVAL_KEEP_WORKSPACE=1

USER_CFG="${LEVELER_USER_CONFIG:-$HOME/.leveler/config.toml}"
if [ -f "$USER_CFG" ]; then
  cp "$USER_CFG" "$HOME_DIR/config.toml"
fi

set +e
"$BIN" --config-dir "$ROOT/configs" eval run \
  --model "$MODEL" \
  --cases "$CASE_DIR" \
  --json-out "$OUT_DIR/eval_result.json" \
  "$@"
ec=$?
set -e

python3 "$ROOT/eval/scripts/run_micro.py" score \
  --home "$HOME_DIR" \
  --out "$OUT_DIR/batch.json" \
  --arm control \
  --model "$MODEL" \
  --suite long-task \
  --max-rounds 280 \
  --batch-id "$BATCH_ID"
echo "LONG task $TASK eval_exit=$ec batch=$OUT_DIR/batch.json"
exit 0
