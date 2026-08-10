#!/usr/bin/env python3
"""Offline replay of the C2.2 read-supersession rule over a recorded run.

Answers one question before any model is burned: applied to a run that already
happened, how much of the request would the production rule actually retire?

The rule replicated here is the one in `leveler-agent/src/read_supersession.rs`,
including its size guard — an older `read_file` result is retired only when a
*later* read of the same path, at the same content version, returned a line
range that fully contains it, and only when the marker is shorter than the text
it replaces.

Two facts are reconstructed from the recording rather than read from metadata,
because `read_lifecycle` did not exist when these runs were captured:

  returned range      exact — parsed from the `%6d\\t` line prefixes that
                      `read_file` writes into its own output.
  content version     approximated — two reads of a path count as the same
                      version when no edit tool touched that path between them.
                      In production this is a full-file fingerprint, which is
                      strictly stricter, so the replay can only ever
                      over-estimate. Reported separately for that reason.

Usage:
    python3 scripts/replay_supersession.py <substring-of-session-dir> ...
"""

from __future__ import annotations

import collections
import glob
import json
import os
import re
import sqlite3
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from analyze_context import READ_TOOLS, estimate, tool_names  # noqa: E402

EDIT_TOOLS = {"apply_patch", "replace"}
SUPERSEDED_PREFIX = "[read_file result superseded:"
LINE_PREFIX = re.compile(r"^\s*(\d+)\t", re.MULTILINE)


def marker(path: str, start: int, end: int) -> str:
    """Byte-for-byte the string the production projection writes."""
    return (
        f"{SUPERSEDED_PREFIX} a later read of the same unchanged file version "
        f"fully covers lines {start}-{end} of {path}. Use the later read result.]"
    )


def returned_range(content: str) -> tuple[int, int] | None:
    """The line interval a `read_file` result actually carries."""
    numbers = [int(m) for m in LINE_PREFIX.findall(content)]
    if not numbers:
        return None
    return min(numbers), max(numbers)


def load_reads(session_dir: str):
    """Every read result in the final request, in transcript order, plus the
    edit events interleaved with them."""
    conn = sqlite3.connect(f"{session_dir}/sessions.db")
    rows = list(conn.execute("select sequence, type, payload from events order by sequence"))
    names = tool_names(rows)
    paths: dict[str, str] = {}
    for _, kind, payload in rows:
        if kind != "tool_call_started":
            continue
        body = json.loads(payload).get("payload") or {}
        args = body.get("arguments")
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except Exception:
                args = {}
        paths[body.get("call_id")] = (args or {}).get("path") or (args or {}).get("file_path") or ""

    snapshots = [
        json.loads(payload)["payload"]["messages"]
        for _, kind, payload in rows
        if kind == "context_snapshot"
    ]
    return snapshots, names, paths


def replay(messages, names, paths):
    """Walk one request's messages, returning the per-read facts the rule needs."""
    timeline = []  # ("edit", path) | ("read", index into reads)
    reads = []
    for message in messages:
        for part in message.get("content", []):
            if part.get("type") == "tool_call":
                call = part.get("call", {})
                if call.get("name") in EDIT_TOOLS:
                    args = call.get("arguments") or {}
                    timeline.append(("edit", args.get("path") or args.get("file_path") or ""))
            elif part.get("type") == "tool_result":
                result = part["result"]
                call_id = result.get("call_id")
                if names.get(call_id) not in READ_TOOLS or result.get("is_error"):
                    continue
                content = result.get("content", "")
                if content.startswith(SUPERSEDED_PREFIX):
                    continue
                span = returned_range(content)
                if span is None:
                    continue
                path = paths.get(call_id, "")
                # `read_file` flags a mid-line clip in its own trailer; that is
                # exactly the case the rule refuses to reason about.
                clipped_inside_line = "truncated within line" in content
                timeline.append(("read", len(reads)))
                reads.append(
                    {
                        "path": path,
                        "start": span[0],
                        "end": span[1],
                        "clipped_inside_line": clipped_inside_line,
                        "tokens": estimate(content),
                        "bytes": len(content),
                        "version": 0,
                    }
                )

    # Version stamp: bump a path's version every time an edit touches it, so
    # reads on either side of an edit can never be proved equivalent.
    version: dict[str, int] = collections.defaultdict(int)
    for kind, value in timeline:
        if kind == "edit":
            version[value] += 1
        else:
            reads[value]["version"] = version[reads[value]["path"]]
    return reads


def provable_end(read) -> int:
    return read["end"] - 1 if read["clipped_inside_line"] else read["end"]


def superseded_by(earlier, later) -> bool:
    return (
        earlier["path"] == later["path"]
        and earlier["version"] == later["version"]
        and not earlier["clipped_inside_line"]
        and later["start"] <= earlier["start"]
        and provable_end(later) >= earlier["end"]
    )


def project(reads) -> tuple[int, int]:
    """(tokens retired, results retired) under the production rule."""
    retired_tokens = 0
    retired = 0
    for index, earlier in enumerate(reads):
        if not any(superseded_by(earlier, later) for later in reads[index + 1 :]):
            continue
        text = marker(earlier["path"], earlier["start"], earlier["end"])
        if len(text) >= earlier["bytes"]:
            continue  # size guard: the marker would cost more than the content
        retired_tokens += earlier["tokens"] - estimate(text)
        retired += 1
    return retired_tokens, retired


def project_backward(reads) -> int:
    """The C2.1 lower bound, which dominated in the other direction: a read
    whose range is already covered by an *earlier* retained read. Reported for
    comparison only — the production rule does not do this."""
    retired = 0
    for index, later in enumerate(reads):
        if any(superseded_by(later, earlier) for earlier in reads[:index]):
            text = marker(later["path"], later["start"], later["end"])
            if len(text) < later["bytes"]:
                retired += later["tokens"] - estimate(text)
    return retired


def report(pattern: str) -> None:
    matches = sorted(
        glob.glob(os.path.expanduser(f"~/.leveler/projects/*{pattern}*")),
        key=os.path.getmtime,
    )
    if not matches:
        sys.exit(f"no session directory matching {pattern!r}")
    snapshots, names, paths = load_reads(matches[-1])
    if not snapshots:
        print(f"{pattern}: no context snapshots")
        return

    print(f"\n=== {os.path.basename(matches[-1])[-44:]} ===")

    # Final request.
    reads = replay(snapshots[-1], names, paths)
    read_tokens = sum(r["tokens"] for r in reads)
    retired_tokens, retired = project(reads)
    total = 0
    for message in snapshots[-1]:
        for part in message.get("content", []):
            if part.get("type") == "tool_result":
                total += estimate(part["result"].get("content", ""))
            elif part.get("type") == "text":
                total += estimate(part.get("text", ""))
            elif part.get("type") == "tool_call":
                call = part.get("call", {})
                total += estimate(call.get("name", "")) + estimate(
                    json.dumps(call.get("arguments", {}), separators=(",", ":"))
                )

    print("FINAL REQUEST")
    print(f"  read results              {len(reads):>6}  {read_tokens:>9,} tok")
    print(f"  retired by the rule       {retired:>6}  {retired_tokens:>9,} tok"
          f"  ({retired_tokens / read_tokens * 100 if read_tokens else 0:.1f}% of reads,"
          f" {retired_tokens / total * 100 if total else 0:.1f}% of the request)")
    print(f"  C2.1 backward-rule bound  {'':>6}  {project_backward(reads):>9,} tok  (not implemented)")

    # Cumulative across every round: what the projection would have saved on
    # each request the run actually sent.
    baseline = 0
    projected = 0
    for messages in snapshots:
        per_round = replay(messages, names, paths)
        round_reads = sum(r["tokens"] for r in per_round)
        baseline += round_reads
        projected += round_reads - project(per_round)[0]
    print("ALL ROUNDS (read-result tokens carried across every request)")
    print(f"  baseline                  {baseline:>9,}")
    print(f"  projected                 {projected:>9,}")
    print(f"  removed                   {baseline - projected:>9,}"
          f"  ({(baseline - projected) / baseline * 100 if baseline else 0:.1f}%)")


def main() -> None:
    patterns = [a for a in sys.argv[1:] if not a.startswith("--")]
    if not patterns:
        sys.exit(__doc__)
    for pattern in patterns:
        report(pattern)


if __name__ == "__main__":
    main()
