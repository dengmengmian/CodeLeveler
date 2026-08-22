#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
python3 -m unittest discover -s eval/tests -v
echo "EVAL_SELFTEST = PASS"
