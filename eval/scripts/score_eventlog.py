#!/usr/bin/env python3
"""Score one sessions.db (or a LEVELER_HOME tree) to stdout JSON."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "lib"))

from eventlog import extract_path, find_session_dbs  # noqa: E402


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("path", help="sessions.db or a LEVELER_HOME directory")
    args = p.parse_args()
    path = Path(args.path)
    if path.is_dir():
        dbs = find_session_dbs(path)
        print(json.dumps([extract_path(db) for db in dbs], indent=2, default=str))
        return 0 if dbs else 1
    print(json.dumps(extract_path(path), indent=2, default=str))
    return 0


if __name__ == "__main__":
    sys.exit(main())
