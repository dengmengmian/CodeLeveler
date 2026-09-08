#!/usr/bin/env python3
"""Render markdown + CSV from one or more batch.json files."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "lib"))

from report import csv_summary, markdown_report  # noqa: E402
from schema import validate_batch  # noqa: E402


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("batches", nargs="+", help="batch.json files")
    p.add_argument("--md", required=True)
    p.add_argument("--csv")
    p.add_argument("--title", default="CodeLeveler eval report")
    args = p.parse_args()
    named = {}
    all_runs = []
    for path in args.batches:
        doc = json.loads(Path(path).read_text(encoding="utf-8"))
        errors = validate_batch(doc)
        if errors:
            raise SystemExit(f"{path}: " + "; ".join(errors))
        name = (doc.get("arm") or {}).get("name") or doc.get("batch_id") or path
        named[str(name)] = doc["runs"]
        all_runs.extend(doc["runs"])
    Path(args.md).write_text(markdown_report(title=args.title, batches=named), encoding="utf-8")
    if args.csv:
        Path(args.csv).write_text(csv_summary(all_runs), encoding="utf-8")
    print(f"wrote {args.md}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
