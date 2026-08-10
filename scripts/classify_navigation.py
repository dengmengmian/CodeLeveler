#!/usr/bin/env python3
"""Offline classification of what each navigation call actually bought (C2.3B).

Rounds and token counts say how much a run explored; they do not say whether
the exploring was worth anything. This walks a recorded session in order and
labels every search and read by the information it added at the moment it ran:

  NEW_TARGET      first contact with a path the case declares relevant
  NEW_IMPACT      first contact with a path the change has to reach
  NEW_FILE        a file this run had not looked at before
  NEW_REGION      a part of an already-read file that was not read before
  DUPLICATE       a search returning only places already known, or a read of a
                  region already in context
  LOW_VALUE       a search that returned nothing, or that surfaced only files
                  the run never went on to read or edit

The labels are computed after the fact from the event log and are never shown
to the agent. `relevant` / `impact` come from the eval case's metrics-only
path lists.

Usage:
    python3 scripts/classify_navigation.py <session-substring> \\
        [--relevant a.go,b.go] [--impact a.go,c.go]
"""

from __future__ import annotations

import argparse
import collections
import glob
import json
import os
import sqlite3
import sys

SEARCH_TOOLS = {"grep", "find_files", "find_symbol", "find_references", "list_files", "locate_hint"}
READ_TOOLS = {"read_file", "read_symbol"}
EDIT_TOOLS = {"apply_patch", "replace"}

# A path mentioned anywhere in a tool result line, as `path:line:text` or bare.
def paths_in(text: str) -> set[str]:
    found = set()
    for line in text.splitlines():
        head = line.split(":", 1)[0].strip()
        if head and ("/" in head or "." in head) and " " not in head:
            found.add(head)
    return found


def load(pattern: str):
    matches = sorted(
        glob.glob(os.path.expanduser(f"~/.leveler/projects/*{pattern}*")),
        key=os.path.getmtime,
    )
    if not matches:
        sys.exit(f"no session directory matching {pattern!r}")
    conn = sqlite3.connect(f"{matches[-1]}/sessions.db")
    rows = list(conn.execute("select sequence, type, payload from events order by sequence"))
    return rows, os.path.basename(matches[-1])


def walk(rows):
    """Tool calls in order, each with its name, arguments and result text."""
    started: dict[str, tuple[str, dict]] = {}
    results: dict[str, str] = {}
    order: list[str] = []
    for _, kind, payload in rows:
        body = (json.loads(payload).get("payload") or {})
        if kind == "tool_call_started":
            args = body.get("arguments")
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except Exception:
                    args = {}
            started[body.get("call_id")] = (body.get("name") or "?", args or {})
            order.append(body.get("call_id"))
        elif kind == "tool_call_finished":
            results[body.get("call_id")] = body.get("preview") or ""
    return [(cid, *started[cid], results.get(cid, "")) for cid in order if cid in started]


def classify(calls, relevant: set[str], impact: set[str]):
    seen_paths: set[str] = set()
    read_ranges: dict[str, list[tuple[int, int]]] = collections.defaultdict(list)
    search_results: set[str] = set()
    queries: set[str] = set()
    labels: list[tuple[str, str, str]] = []
    first_edit = None
    touched_relevant: set[str] = set()
    touched_impact: set[str] = set()
    # Files the run went on to read or edit, so a search can be judged by
    # whether anything it surfaced was actually used.
    later_used = {
        (a.get("path") or a.get("file_path") or "")
        for _, name, a, _ in calls
        if name in READ_TOOLS | EDIT_TOOLS
    }

    for index, (_, name, args, result) in enumerate(calls):
        path = args.get("path") or args.get("file_path") or ""
        for group, store in ((relevant, touched_relevant), (impact, touched_impact)):
            for candidate in group:
                if candidate in json.dumps(args) or candidate == path:
                    store.add(candidate)

        if name in EDIT_TOOLS:
            if first_edit is None:
                first_edit = index + 1
            seen_paths.add(path)
            continue
        if name not in SEARCH_TOOLS | READ_TOOLS:
            continue

        if name in SEARCH_TOOLS:
            query = json.dumps(
                {k: v for k, v in args.items() if k in ("pattern", "query", "symbol", "name")},
                sort_keys=True,
            )
            hits = paths_in(result)
            fresh = hits - search_results
            if not hits:
                label = "LOW_VALUE"
            elif query in queries and not fresh:
                label = "DUPLICATE"
            elif not fresh:
                label = "DUPLICATE"
            elif fresh & (relevant | impact):
                label = "NEW_TARGET" if fresh & relevant else "NEW_IMPACT"
            elif fresh & later_used:
                label = "NEW_FILE"
            else:
                label = "LOW_VALUE"
            queries.add(query)
            search_results |= hits
            labels.append((name, label, query[:60]))
            continue

        start = args.get("start_line") or 1
        end = args.get("end_line") or 10**9
        covered = any(s <= start and e >= end for s, e in read_ranges[path])
        if covered:
            label = "DUPLICATE"
        elif path in relevant and path not in seen_paths:
            label = "NEW_TARGET"
        elif path in impact and path not in seen_paths:
            label = "NEW_IMPACT"
        elif path not in seen_paths:
            label = "NEW_FILE"
        else:
            label = "NEW_REGION"
        read_ranges[path].append((start, end))
        seen_paths.add(path)
        labels.append((name, label, path))

    return labels, first_edit, touched_relevant, touched_impact


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("session")
    ap.add_argument("--relevant", default="")
    ap.add_argument("--impact", default="")
    ap.add_argument("--list", action="store_true", help="print every call")
    args = ap.parse_args()

    relevant = {p for p in args.relevant.split(",") if p}
    impact = {p for p in args.impact.split(",") if p}
    rows, label = load(args.session)
    calls = walk(rows)
    labels, first_edit, touched_relevant, touched_impact = classify(calls, relevant, impact)

    counts = collections.Counter(kind for _, kind, _ in labels)
    total = sum(counts.values())
    print(f"\n=== {label[-46:]} ===")
    print(f"navigation calls: {total}   first edit at tool call: {first_edit}")
    for kind in ("NEW_TARGET", "NEW_IMPACT", "NEW_FILE", "NEW_REGION", "DUPLICATE", "LOW_VALUE"):
        n = counts.get(kind, 0)
        if n:
            print(f"  {kind:14}{n:>4}  ({n / total * 100:.0f}%)")
    informative = sum(counts.get(k, 0) for k in ("NEW_TARGET", "NEW_IMPACT", "NEW_FILE", "NEW_REGION"))
    print(f"  {'informative':14}{informative:>4}  ({informative / total * 100:.0f}% of navigation)")
    if relevant:
        print(f"relevant recall: {len(touched_relevant)}/{len(relevant)}"
              f"  missed: {sorted(relevant - touched_relevant) or 'none'}")
    if impact:
        print(f"impact recall:   {len(touched_impact)}/{len(impact)}"
              f"  missed: {sorted(impact - touched_impact) or 'none'}")
    if args.list:
        for i, (tool, kind, detail) in enumerate(labels, 1):
            print(f"  {i:>3} {tool:16} {kind:12} {detail}")


if __name__ == "__main__":
    main()
