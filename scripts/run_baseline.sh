#!/usr/bin/env bash
# Produce a durable evaluation baseline.
#
# Required env:
#   MODEL_A  stronger / reference model ref (e.g. deepseek/deepseek-v4-pro)
#   MODEL_B  weaker / candidate model ref
#
# Optional env:
#   CASES    case directory (default: evals, recursively loads 30 cases)
#   REPETITIONS runs per case/model (default: 3)
#   OUT_DIR  where to write JSON (default: evals/baselines)
#   LEVELER  path to the leveler binary (default: cargo run -q -p leveler-cli --)
#
# Example:
#   export DEEPSEEK_API_KEY=...
#   MODEL_A=deepseek/deepseek-v4-pro MODEL_B=deepseek/deepseek-v4-flash \
#     ./scripts/run_baseline.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if [[ -z "${MODEL_A:-}" || -z "${MODEL_B:-}" ]]; then
  cat >&2 <<'EOF'
usage: MODEL_A=<ref> MODEL_B=<ref> ./scripts/run_baseline.sh

Both model refs are required so baselines stay comparable across machines.
Optional: CASES=evals REPETITIONS=3 OUT_DIR=evals/baselines
  (use CASES=evals/smoke for a cheap smoke compare)
EOF
  exit 2
fi

CASES="${CASES:-evals}"
REPETITIONS="${REPETITIONS:-3}"
OUT_DIR="${OUT_DIR:-evals/baselines}"
stamp="$(date -u +%Y-%m-%d)"
# Sanitize model refs for filenames (provider/name → provider-name).
slug_a="$(printf '%s' "$MODEL_A" | tr '/:' '--')"
slug_b="$(printf '%s' "$MODEL_B" | tr '/:' '--')"
out="${OUT_DIR}/${stamp}-compare-${slug_a}-vs-${slug_b}.json"

mkdir -p "$OUT_DIR"

if [[ -n "${LEVELER:-}" ]]; then
  run_leveler() { "$LEVELER" "$@"; }
else
  run_leveler() { cargo run -q -p leveler-cli -- "$@"; }
fi

echo "==> leveler eval compare"
echo "    A:     $MODEL_A"
echo "    B:     $MODEL_B"
echo "    cases: $CASES"
echo "    repetitions: $REPETITIONS"
echo "    out:   $out"

run_leveler eval compare "$MODEL_A" "$MODEL_B" \
  --cases "$CASES" \
  --repetitions "$REPETITIONS" \
  --json-out "$out"

echo "==> baseline written: $out"
echo "    Record run conditions in the file's meta (git_sha, created_at, mode)."
echo "    Next: open the JSON, note failed_a / failed_b, and file a short note"
echo "    under docs/ or the PR that introduces the first real numbers."
