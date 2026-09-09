#!/usr/bin/env python3
"""Classify a run's model rounds from the tracing the runtime already emits.

Wall time is round trips times a fixed per-round cost (see
`docs/RUNTIME_LATENCY_CLOSURE.md`), so the question that matters is what each
round bought. This reads a log and answers it; it adds no events and no
production cost.

Needs `RUST_LOG=leveler_agent=debug,leveler_agent_core=info`:

  - `model round finished`      (leveler_agent_core) one line per model request
  - `executing admitted call`   (leveler_agent)      the tool each round ran
  - `closeout decided`          (leveler_agent)      why a quiet round was re-prompted

Usage:  round_taxonomy.py RUN.log [--json OUT.json]
"""

import argparse
import json
import re
import statistics
import sys
from collections import Counter

ANSI = re.compile(r"\x1b\[[0-9;]*m")
FIELDS = re.compile(r"(\w+)=([^\s]+)")

# A round's kind, decided by mechanical evidence only. Nothing here judges
# whether a round was "worth it" — that is not a thing a log can know.
TOOL_ROUND = "tool"
CLOSEOUT_NUDGE = "closeout_nudge"
QUIET_FINAL = "quiet_final"
RETRY = "retry"


def parse(path):
    """Rounds in order, plus the case boundaries the eval runner prints.

    One pass, because attribution is positional: a round's tools are dispatched
    AFTER its `model round finished` line and before the next one. Collecting
    tools separately and zipping by count drifts as soon as anything dispatches
    a tool outside a round.
    """
    cases, rounds = [], []
    pending_nudge, retries = None, 0
    for raw in open(path, errors="ignore"):
        line = ANSI.sub("", raw)
        case = re.search(r"▶ eval .*\((\S+), run", line)
        if case:
            cases.append({"id": case.group(1), "start": len(rounds)})
            continue
        if "executing admitted call" in line:
            f = dict(FIELDS.findall(line.split("executing admitted call", 1)[1]))
            if rounds:
                rounds[-1]["tools"].append(f.get("tool", "?"))
            continue
        if "closeout decided" in line:
            f = dict(FIELDS.findall(line.split("closeout decided", 1)[1]))
            pending_nudge = f.get("action")
            continue
        if "no terminal event (retryable)" in line:
            retries += 1
            continue
        if "model round finished" not in line:
            continue
        f = dict(FIELDS.findall(line.split("model round finished", 1)[1]))

        def num(key):
            try:
                return int(f.get(key, "0"))
            except ValueError:
                return 0

        rounds.append(
            {
                "calls": num("calls"),
                "input": num("input_tokens"),
                "cached": num("cached_input_tokens"),
                "output": num("output_tokens"),
                "ttfb_ms": num("connect_ms"),
                "total_ms": num("total_ms"),
                "tools": [],
                # A nudge is decided after a quiet round and paid for by the
                # round that follows it, so it is recorded on the payer.
                "nudge_before": pending_nudge,
                "retries_before": retries,
            }
        )
        pending_nudge, retries = None, 0
    return cases, rounds


def classify(rounds):
    for idx, r in enumerate(rounds):
        if r["calls"] > 0:
            r["kind"] = TOOL_ROUND
        elif idx + 1 < len(rounds) and rounds[idx + 1]["nudge_before"] and "NudgeOnce" in (
            rounds[idx + 1]["nudge_before"] or ""
        ):
            # A quiet round that the runtime answered with a nudge: the NEXT
            # round exists only because this one did not close the goal.
            r["kind"] = CLOSEOUT_NUDGE
        else:
            r["kind"] = QUIET_FINAL
    return rounds


def summarize(cases, rounds):
    per_case = []
    for n, case in enumerate(cases):
        end = cases[n + 1]["start"] if n + 1 < len(cases) else len(rounds)
        rs = rounds[case["start"] : end]
        if not rs:
            continue
        kinds = Counter(r["kind"] for r in rs)
        per_case.append(
            {
                "id": case["id"],
                "rounds": len(rs),
                "tool_rounds": kinds[TOOL_ROUND],
                "closeout_nudge_rounds": kinds[CLOSEOUT_NUDGE],
                "quiet_rounds": kinds[QUIET_FINAL],
                "tool_calls": sum(r["calls"] for r in rs),
                "max_calls_in_a_round": max((r["calls"] for r in rs), default=0),
                "first_tool": next((r["tools"][0] for r in rs if r["tools"]), None),
                "model_wait_ms": sum(r["total_ms"] for r in rs),
                "ttfb_ms": sum(r["ttfb_ms"] for r in rs),
                "tools": dict(Counter(t for r in rs for t in r["tools"])),
            }
        )
    return per_case


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--json")
    args = ap.parse_args()

    cases, rounds = parse(args.log)
    if not rounds:
        sys.exit("no `model round finished` lines: was RUST_LOG set?")
    classify(rounds)
    per_case = summarize(cases, rounds)

    kinds = Counter(r["kind"] for r in rounds)
    calls = Counter(r["calls"] for r in rounds)
    tool_rounds = [r for r in rounds if r["kind"] == TOOL_ROUND]
    rc = [c["rounds"] for c in per_case]

    print(f"cases                       {len(per_case)}")
    print(f"model rounds                {len(rounds)}")
    if rc:
        print(f"  median rounds/case        {statistics.median(rc):.0f}")
        print(f"  p90 rounds/case           {sorted(rc)[max(0, int(.9 * len(rc)) - 1)]}")
    print(f"  tool rounds               {kinds[TOOL_ROUND]}")
    print(f"  closeout nudge rounds     {kinds[CLOSEOUT_NUDGE]}"
          f"   ({100 * kinds[CLOSEOUT_NUDGE] / len(rounds):.1f}% of rounds)")
    print(f"  quiet / final rounds      {kinds[QUIET_FINAL]}")
    print(f"tool calls per round        {dict(sorted(calls.items()))}")
    if tool_rounds:
        per = sum(r["calls"] for r in tool_rounds) / len(tool_rounds)
        print(f"useful tool calls / round   {per:.2f}")
    print(f"first tool of each case     "
          f"{dict(Counter(c['first_tool'] for c in per_case))}")
    print(f"model wait                  {sum(r['total_ms'] for r in rounds) / 1000:.1f}s"
          f"   (ttfb {sum(r['ttfb_ms'] for r in rounds) / 1000:.1f}s)")
    print()
    print(f"{'case':<24}{'rounds':>7}{'tool':>6}{'nudge':>7}{'quiet':>7}{'calls':>7}  first tool")
    for c in per_case:
        print(f"{c['id']:<24}{c['rounds']:>7}{c['tool_rounds']:>6}"
              f"{c['closeout_nudge_rounds']:>7}{c['quiet_rounds']:>7}{c['tool_calls']:>7}"
              f"  {c['first_tool']}")

    if args.json:
        with open(args.json, "w") as fh:
            json.dump({"cases": per_case, "rounds": rounds}, fh, indent=2)
        print(f"\nwrote {args.json}")


if __name__ == "__main__":
    main()
