#!/usr/bin/env bash
# One-night single-knob ablation matrix: flash × evals/l1-hard × 4 resolver
# knobs (post-tier-retirement names; legacy require_* still accepted).
# Each run carries its own control+ablated pair — never compare arms across
# network conditions (0e2ae4a). Proxy env is stripped: through the local proxy
# the same suite runs ~5× slower and throws StreamInterrupted infra failures.
#
# Usage:  DEEPSEEK_API_KEY=sk-... ./scripts/run_ablation_matrix.sh
# Rerun after an interrupt: knobs whose final .json exists are skipped; the
# cut-off knob's .partial.jsonl stays on disk for forensics (the driver does
# NOT resume mid-knob — it restarts that knob).
set -uo pipefail
cd "$(dirname "$0")/.."

: "${DEEPSEEK_API_KEY:?export DEEPSEEK_API_KEY first}"

LEVELER=target/release/leveler
MODEL=deepseek/deepseek-v4-flash
CASES=evals/l1-hard
KNOBS=(explicit_plan step_summary completion_evidence repeated_read_guard)

for knob in "${KNOBS[@]}"; do
  out="evals/baselines/ablate-flash-${knob//_/-}.json"
  if [ -f "$out" ]; then
    echo "== skip $knob ($out exists)"
    continue
  fi
  echo "== $(date '+%F %T') ablate $knob -> $out"
  env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
      -u all_proxy -u ALL_PROXY \
    "$LEVELER" eval ablate "$knob" \
      --model "$MODEL" --cases "$CASES" --direct \
      --json-out "$out" \
    || echo "!! $knob failed (exit $?) — continuing with next knob"
done
echo "== $(date '+%F %T') matrix done"
