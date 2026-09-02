#!/usr/bin/env python3
"""Offline per-round context cost attribution for a recorded run (C2.1).

Answers one question with the engine's own data: of the tokens the model is
sent each round, which part is fixed prompt, which is tool schemas, which is
conversation, and which is tool output — and how much of it is stale.

Nothing is re-run and no model is called. The source is the persisted
`context_snapshot` event, which carries the exact assembled message list for
every round, plus the `model_requests` table for what the provider actually
billed.

Per-round snapshots exist only for `leveler eval` runs (the eval command sets
the `context_trace` execution override). A production session persists a
snapshot only when its context diverged from the transcript — a compaction
fold or a transient nudge — so this script is not meaningful on those.

Token counts replicate the product's own estimator (leveler-agent
compaction.rs): ascii_bytes/4 + non_ascii_bytes/3, with a flat charge per
image. Using the same function means the numbers here line up with the ones
compaction would see.

Usage:
    python3 scripts/analyze_context.py <substring-of-session-dir> [--curve]
"""

from __future__ import annotations

import collections
import glob
import json
import os
import sqlite3
import sys

IMAGE_BYTE_EQUIV = 4096

READ_TOOLS = {"read_file", "read_symbol"}
SEARCH_TOOLS = {"grep", "find_files", "find_symbol", "list_files", "find_references", "locate_hint"}
SHELL_TOOLS = {"shell_command", "run_command"}
EDIT_TOOLS = {"apply_patch", "replace"}

# The buckets a round's context is split into, in report order.
BUCKETS = [
    "SYSTEM_BASE",
    "OBJECTIVE",
    "CONVERSATION_USER",
    "CONVERSATION_ASSISTANT",
    "TOOL_CALL_ARGUMENTS",
    "TOOL_RESULT_READ",
    "TOOL_RESULT_SEARCH",
    "TOOL_RESULT_SHELL",
    "TOOL_RESULT_EDIT",
    "TOOL_RESULT_OTHER",
    "OTHER",
]


def estimate(text: str) -> int:
    """The product's estimator, replicated exactly."""
    raw = text.encode("utf-8")
    ascii_bytes = sum(1 for b in raw if b < 128)
    wide_bytes = len(raw) - ascii_bytes
    return ascii_bytes // 4 + wide_bytes // 3


def load(pattern: str):
    matches = sorted(
        glob.glob(os.path.expanduser(f"~/.leveler/projects/*{pattern}*")),
        key=os.path.getmtime,
    )
    if not matches:
        sys.exit(f"no session directory matching {pattern!r}")
    conn = sqlite3.connect(f"{matches[-1]}/sessions.db")
    rows = list(conn.execute("select sequence, type, payload from events order by sequence"))
    provider = [
        int(t) for (t,) in conn.execute(
            "select input_tokens from model_requests order by created_at, id"
        )
    ]
    return rows, provider, os.path.basename(matches[-1])


def tool_names(rows) -> dict[str, str]:
    """call_id -> tool name, so a tool result can be attributed to its tool."""
    names: dict[str, str] = {}
    for _, kind, payload in rows:
        if kind != "tool_call_started":
            continue
        body = json.loads(payload).get("payload") or {}
        names[body.get("call_id")] = body.get("name") or "?"
    return names


def bucket_for_result(name: str) -> str:
    if name in READ_TOOLS:
        return "TOOL_RESULT_READ"
    if name in SEARCH_TOOLS:
        return "TOOL_RESULT_SEARCH"
    if name in SHELL_TOOLS:
        return "TOOL_RESULT_SHELL"
    if name in EDIT_TOOLS:
        return "TOOL_RESULT_EDIT"
    return "TOOL_RESULT_OTHER"


def attribute(messages, names) -> tuple[dict[str, int], list[tuple[str, str, int]]]:
    """Split one round's messages into buckets, and return the tool results
    (name, content, tokens) for duplication analysis."""
    totals = collections.Counter()
    results: list[tuple[str, str, int]] = []
    seen_user = 0
    for message in messages:
        role = message.get("role")
        for part in message.get("content", []):
            kind = part.get("type")
            if kind == "text":
                cost = estimate(part.get("text", ""))
                if role == "system":
                    totals["SYSTEM_BASE"] += cost
                elif role == "user":
                    # The first user message states the task; later ones are
                    # nudges, steering and continuation prompts.
                    totals["OBJECTIVE" if seen_user == 0 else "CONVERSATION_USER"] += cost
                    seen_user += 1
                else:
                    totals["CONVERSATION_ASSISTANT"] += cost
            elif kind == "tool_call":
                call = part.get("call", {})
                totals["TOOL_CALL_ARGUMENTS"] += estimate(call.get("name", "")) + estimate(
                    json.dumps(call.get("arguments", {}), separators=(",", ":"))
                )
            elif kind == "tool_result":
                result = part.get("result", {})
                content = result.get("content", "")
                cost = estimate(content)
                name = names.get(result.get("call_id"), "?")
                totals[bucket_for_result(name)] += cost
                results.append((name, content, cost))
            elif kind == "image":
                totals["OTHER"] += IMAGE_BYTE_EQUIV // 4
            else:
                totals["OTHER"] += estimate(json.dumps(part, separators=(",", ":")))
    return totals, results


def report(pattern: str, curve: bool) -> None:
    rows, provider, label = load(pattern)
    names = tool_names(rows)
    snapshots = [
        json.loads(payload)["payload"]["messages"]
        for _, kind, payload in rows
        if kind == "context_snapshot"
    ]
    if not snapshots:
        print(f"{label}: no context snapshots")
        return

    print(f"\n=== {label[-44:]} ===")
    print(f"rounds: {len(snapshots)}  provider-billed requests: {len(provider)}")

    per_round = [attribute(m, names) for m in snapshots]
    finals, final_results = per_round[-1]
    total = sum(finals.values())

    print("\nFINAL-ROUND CONTEXT (the largest request the model saw)")
    print(f"  {'bucket':26}{'tokens':>10}{'share':>8}")
    for bucket in BUCKETS:
        value = finals.get(bucket, 0)
        if value:
            print(f"  {bucket:26}{value:>10,}{value / total * 100:>7.1f}%")
    print(f"  {'TOTAL (local estimate)':26}{total:>10,}")

    # Local estimate vs what the provider billed, per round.
    if provider:
        deltas = []
        for index, (totals, _) in enumerate(per_round):
            if index < len(provider):
                deltas.append(provider[index] - sum(totals.values()))
        if deltas:
            deltas_sorted = sorted(deltas)
            print(
                f"\nPROVIDER − LOCAL (fixed per-round tax: tool schemas, wire format, tokenizer)"
                f"\n  min {min(deltas):,}  median {deltas_sorted[len(deltas_sorted)//2]:,}"
                f"  max {max(deltas):,}"
            )
            print(f"  provider total {sum(provider):,}  local total {sum(sum(t.values()) for t, _ in per_round):,}")

    # Duplication: identical tool output carried more than once in one request.
    seen: dict[str, int] = {}
    duplicate_tokens = 0
    for name, content, cost in final_results:
        key = f"{name}\x00{content}"
        if key in seen:
            duplicate_tokens += cost
        seen[key] = seen.get(key, 0) + 1
    result_tokens = sum(finals.get(b, 0) for b in BUCKETS if b.startswith("TOOL_RESULT"))
    print("\nDUPLICATION IN THE FINAL REQUEST")
    print(f"  tool-result tokens        {result_tokens:>10,}")
    print(f"  byte-identical repeats    {duplicate_tokens:>10,}"
          f"  ({duplicate_tokens / result_tokens * 100:.1f}% of tool results)" if result_tokens else "")
    repeats = collections.Counter(
        name for name, content, _ in final_results if seen.get(f"{name}\x00{content}", 0) > 1
    )
    if repeats:
        print(f"  repeated by tool          {dict(repeats.most_common(5))}")

    # Working set: what a run still needs — the last few exchanges, versus
    # everything that came before.
    RECENT = 12
    recent_msgs = snapshots[-1][-RECENT:]
    recent_totals, _ = attribute(recent_msgs, names)
    recent = sum(recent_totals.values())
    fixed = finals.get("SYSTEM_BASE", 0) + finals.get("OBJECTIVE", 0)
    history = total - recent - fixed
    print("\nWORKING SET (final request)")
    print(f"  fixed prompt + objective  {fixed:>10,}{fixed / total * 100:>7.1f}%")
    print(f"  last {RECENT} messages         {recent:>10,}{recent / total * 100:>7.1f}%")
    print(f"  older history             {history:>10,}{history / total * 100:>7.1f}%")

    if curve:
        print("\nGROWTH CURVE")
        print(f"  {'round':>6}{'total':>10}{'system':>9}{'history':>10}{'reads':>9}{'search':>8}{'shell':>9}")
        marks = [r for r in (1, 5, 10, 20, 30, 40, 50, 60, 80, 100) if r <= len(per_round)]
        if len(per_round) not in marks:
            marks.append(len(per_round))
        for round_no in marks:
            totals, _ = per_round[round_no - 1]
            body = sum(totals.values())
            print(
                f"  {round_no:>6}{body:>10,}{totals.get('SYSTEM_BASE', 0):>9,}"
                f"{totals.get('CONVERSATION_ASSISTANT', 0) + totals.get('TOOL_CALL_ARGUMENTS', 0):>10,}"
                f"{totals.get('TOOL_RESULT_READ', 0):>9,}{totals.get('TOOL_RESULT_SEARCH', 0):>8,}"
                f"{totals.get('TOOL_RESULT_SHELL', 0):>9,}"
            )


def main() -> None:
    patterns = [a for a in sys.argv[1:] if not a.startswith("--")]
    curve = "--curve" in sys.argv[1:]
    if not patterns:
        sys.exit(__doc__)
    for pattern in patterns:
        report(pattern, curve)


if __name__ == "__main__":
    main()
