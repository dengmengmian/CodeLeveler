#!/usr/bin/env python3
"""Compare two batch.json files (prompt / timing / model).

    python3 eval/scripts/compare_arms.py control.json treatment.json
    python3 eval/scripts/compare_arms.py a.json b.json --md out.md --csv out.csv
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "lib"))

from metrics import compare_batches  # noqa: E402
from report import compare_markdown, csv_summary  # noqa: E402
from schema import validate_batch  # noqa: E402


def load(path: Path) -> dict:
    doc = json.loads(path.read_text(encoding="utf-8"))
    errors = validate_batch(doc)
    if errors:
        raise SystemExit(f"{path}: " + "; ".join(errors))
    return doc


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("control")
    p.add_argument("treatment")
    p.add_argument("--md")
    p.add_argument("--csv")
    p.add_argument("--json-out")
    p.add_argument("--title", default="Adoption micro comparison")
    args = p.parse_args()
    a = load(Path(args.control))
    b = load(Path(args.treatment))
    cmp_ = compare_batches(a["runs"], b["runs"], spawn_likely_only=True)
    print(json.dumps(cmp_, indent=2))
    md = compare_markdown(a["runs"], b["runs"], args.title)
    if args.md:
        Path(args.md).write_text(md, encoding="utf-8")
    if args.csv:
        Path(args.csv).write_text(csv_summary(a["runs"] + b["runs"]), encoding="utf-8")
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(cmp_, indent=2) + "\n", encoding="utf-8")
    print(f"verdict: {cmp_['verdict']}  fisher_p={cmp_['fisher_p']}  delta={cmp_['delta_spawn_rate']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
