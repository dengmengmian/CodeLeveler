#!/usr/bin/env python3
"""Check what a recorded run actually read, not what it asked to read.

The live eval credits a path as touched when a call naming it returns
successfully. That is honest as far as it goes, but it cannot see the returned
range: `AgentEvent::ToolResult` carries a 1200-character preview, so a
whole-file request clipped by the byte ceiling looks identical to one that
returned everything.

The durable `context_snapshot` event does carry the full result text — it is
what the model was actually sent — and `read_file` numbers every line and
appends a truncation marker when it stops early. So the returned range is
recoverable after the fact, which is where this belongs: no production schema
changes, no new tool metadata.

The canonical definition lives in Rust (`leveler-eval/src/read_coverage.rs`,
where it is unit-tested). This mirrors it for trace replay; keep the two in
step if either changes.

Reports per case, for every path the case declares relevant or impact:

    FULL     returned line 1 through the file's last line, unclipped
    PARTIAL  returned successfully, but not the whole file
    MISS     never returned successfully

Usage:
    python3 scripts/analyze_read_coverage.py [--cases evals/navigation]
                                             [--repo fixtures/repos/navsvc]
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import sqlite3
import sys

import yaml

READ_TOOLS = {"read_file", "read_symbol"}
NUMBERED = re.compile(r"^\s*(\d+)\t", re.M)
TRUNCATION = "… [truncated"


def returned_range(content: str):
    """(first, last, clipped) for a read result, or None if it carried no lines."""
    numbers = [int(n) for n in NUMBERED.findall(content)]
    if not numbers:
        return None
    return min(numbers), max(numbers), TRUNCATION in content


def arguments_of(raw):
    """Tool arguments, tolerating a payload that was stored as a truncated string."""
    if isinstance(raw, dict):
        return raw
    if not isinstance(raw, str):
        return {}
    try:
        return json.loads(raw)
    except Exception:
        match = re.search(r'"path"\s*:\s*"([^"]+)"', raw)
        return {"path": match.group(1)} if match else {}


def session_for(case_id: str) -> str | None:
    matches = sorted(
        glob.glob(os.path.expanduser(f"~/.leveler/projects/*{case_id}*")),
        key=os.path.getmtime,
    )
    return matches[-1] if matches else None


def widest_reads(session_dir: str, wanted: list[str]) -> dict[str, tuple[int, int, bool]]:
    """For each wanted path, the widest range any successful read returned."""
    conn = sqlite3.connect(f"{session_dir}/sessions.db")
    rows = list(conn.execute("select type, payload from events order by sequence"))
    names: dict[str, str] = {}
    args: dict[str, dict] = {}
    for kind, payload in rows:
        body = json.loads(payload).get("payload") or {}
        if kind == "tool_call_started":
            names[body.get("call_id")] = body.get("name")
            args[body.get("call_id")] = arguments_of(body.get("arguments"))

    snapshots = [
        json.loads(payload)["payload"]["messages"]
        for kind, payload in rows
        if kind == "context_snapshot"
    ]
    if not snapshots:
        return {}

    out: dict[str, tuple[int, int, bool]] = {}
    for message in snapshots[-1]:
        for part in message.get("content", []):
            if part.get("type") != "tool_result":
                continue
            result = part["result"]
            call_id = result.get("call_id")
            # A failed read is never evidence, whatever it named.
            if names.get(call_id) not in READ_TOOLS or result.get("is_error"):
                continue
            path = (args.get(call_id) or {}).get("path") or ""
            matched = [w for w in wanted if w and (path.endswith(w) or w in path)]
            if not matched:
                continue
            span = returned_range(result.get("content", ""))
            if span is None:
                continue
            first, last, clipped = span
            prior = out.get(matched[0])
            if prior is None:
                out[matched[0]] = (first, last, clipped)
            else:
                out[matched[0]] = (
                    min(prior[0], first),
                    max(prior[1], last),
                    prior[2] or clipped,
                )
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="evals/navigation")
    ap.add_argument("--repo", default="fixtures/repos/navsvc")
    args = ap.parse_args()

    print(f"{'case':30}{'path':40}{'returned':>13}{'clip':>6}{'verdict':>9}{'lines':>7}")
    totals = {"FULL": 0, "PARTIAL": 0, "MISS": 0}
    per_case = {}
    for path in sorted(glob.glob(f"{args.cases}/*.yaml")):
        case = yaml.safe_load(open(path))
        case_id = case["id"]
        wanted = sorted(
            set((case.get("relevant_paths") or []) + (case.get("required_impact_paths") or []))
        )
        session = session_for(case_id)
        if session is None:
            print(f"{case_id[:29]:30}{'NOT RECOVERABLE FROM EXISTING TRACE'}")
            continue
        reads = widest_reads(session, wanted)
        full = touched = 0
        for want in wanted:
            source = os.path.join(args.repo, want)
            lines = sum(1 for _ in open(source)) if os.path.exists(source) else 0
            if want not in reads:
                verdict, shown, clip = "MISS", "not read", ""
            else:
                first, last, clipped = reads[want]
                touched += 1
                complete = (not clipped) and first == 1 and lines and last >= lines
                full += complete
                verdict = "FULL" if complete else "PARTIAL"
                shown, clip = f"{first}-{last}", str(clipped)
            totals[verdict] += 1
            print(f"{case_id[:29]:30}{want[:39]:40}{shown:>13}{clip:>6}{verdict:>9}{lines:>7}")
        per_case[case_id] = (touched, full, len(wanted))

    print()
    for case_id, (touched, full, n) in per_case.items():
        print(f"{case_id[:31]:32} path-touch {touched}/{n}   fully-read {full}/{n}")
    print(f"\nacross all declared paths: {totals}")


if __name__ == "__main__":
    main()
