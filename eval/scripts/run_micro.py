#!/usr/bin/env python3
"""Backward-compatible entry: forwards to eval/micro/adoption/runner/run.py."""

from __future__ import annotations

import runpy
import sys
from pathlib import Path

sys.argv[0] = str(Path(__file__).resolve().parents[1] / "micro" / "adoption" / "runner" / "run.py")
runpy.run_path(sys.argv[0], run_name="__main__")
