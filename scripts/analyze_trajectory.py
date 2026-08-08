#!/usr/bin/env python3
"""Offline post-edit trajectory analysis for a recorded eval run (C1.5B).

Reads the engine's own persisted event log — nothing is re-run and no model is
called — and answers one question: after the first edit, is the run doing
productive iteration (a failure produces a change, which produces a *different*
failure) or thrashing (the same failure, or exploration that ignores the
failure in hand)?

Usage:
    python3 scripts/analyze_trajectory.py <substring-of-session-dir> [--full]

The substring matches a directory under ~/.leveler/projects; the most recently
modified match wins, so `... ripgrep-total-count` picks the latest run.
"""

from __future__ import annotations

import collections
import glob
import json
import os
import re
import sqlite3
import sys

EDIT_TOOLS = {"apply_patch", "replace"}
READ_TOOLS = {"read_file", "read_symbol"}
SEARCH_TOOLS = {"grep", "find_files", "find_symbol", "list_files", "find_references"}
SHELL_TOOLS = {"shell_command", "run_command"}


def load(pattern: str) -> list[dict]:
    """Flatten one session into ordered actions with their results."""
    matches = sorted(
        glob.glob(os.path.expanduser(f"~/.leveler/projects/*{pattern}*")),
        key=os.path.getmtime,
    )
    if not matches:
        sys.exit(f"no session directory matching {pattern!r}")
    db = f"{matches[-1]}/sessions.db"
    conn = sqlite3.connect(db)
    rows = list(conn.execute("select sequence, type, payload from events order by sequence"))

    results: dict[str, tuple[bool, str]] = {}
    for _, kind, payload in rows:
        if kind != "tool_call_finished":
            continue
        body = json.loads(payload).get("payload") or {}
        results[body.get("call_id")] = (bool(body.get("is_error")), body.get("preview") or "")

    actions: list[dict] = []
    round_no = 0
    for _, kind, payload in rows:
        if kind == "context_snapshot":
            round_no += 1
        if kind != "tool_call_started":
            continue
        body = json.loads(payload).get("payload") or {}
        args = body.get("arguments")
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except json.JSONDecodeError:
                args = {}
        if not isinstance(args, dict):
            args = {}
        is_error, preview = results.get(body.get("call_id"), (False, ""))
        actions.append(
            {
                "round": max(round_no, 1),
                "name": body.get("name"),
                "args": args,
                "is_error": is_error,
                "preview": preview,
                "target": target_of(body.get("name"), args),
            }
        )
    return actions, os.path.basename(matches[-1])


def target_of(name: str, args: dict) -> str:
    if name in READ_TOOLS or name in EDIT_TOOLS:
        patch = args.get("patch", "")
        if patch:
            paths = re.findall(r"\*\*\* (?:Update|Add|Delete) File: (\S+)", patch)
            return ",".join(dict.fromkeys(paths)) or "?"
        return str(args.get("path", "?"))
    if name in SEARCH_TOOLS:
        return str(args.get("pattern") or args.get("query") or args.get("path") or "")
    if name in SHELL_TOOLS:
        cmd = args.get("cmd") or " ".join([args.get("program", "")] + args.get("args", []))
        return " ".join(str(cmd).split())[:110]
    return ""


def shell_class(command: str) -> str:
    """BUILD / TEST / CHECK / FORMAT / INSPECTION / OTHER."""
    text = command.lower()
    if "cargo test" in text or "go test" in text or re.search(r"\btest\b.*--", text):
        return "TEST"
    if "cargo build" in text or "go build" in text or text.strip().startswith("make"):
        return "BUILD"
    if "cargo check" in text or "clippy" in text or "go vet" in text:
        return "CHECK"
    if "fmt" in text:
        return "FORMAT"
    if re.match(r"^(cat|ls|head|tail|env|echo|which|pwd|find|wc|grep|sed|awk|git)\b", text.strip()):
        return "INSPECTION"
    return "OTHER"


def diagnostic(preview: str) -> str:
    """A normalized fingerprint of a failure: the first real error/test line."""
    for line in preview.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith(("error[", "error:", "--- FAIL:", "FAILED", "panicked at")):
            # Drop paths/line numbers so the same error at a new line still matches.
            return re.sub(r"[\w./-]+:\d+(:\d+)?", "<loc>", stripped)[:120]
        if stripped.startswith("failures:"):
            return "failures-block"
    tail = " ".join(preview.split())[-120:]
    return tail or "empty"


def report(actions: list[dict], label: str, full: bool) -> None:
    first_edit = next((i for i, a in enumerate(actions) if a["name"] in EDIT_TOOLS), None)
    print(f"\n=== {label} ===")
    if first_edit is None:
        print("  no edit in this run")
        return
    post = actions[first_edit:]
    rounds_total = actions[-1]["round"]
    pre_rounds = actions[first_edit]["round"]

    print(
        f"  rounds ~{rounds_total} | pre-edit ~{pre_rounds} | post-edit ~{rounds_total - pre_rounds}"
        f" | post-edit calls {len(post)}"
    )

    # --- mutation cadence -------------------------------------------------
    edits = [(i, a) for i, a in enumerate(post) if a["name"] in EDIT_TOOLS]
    gaps = [
        edits[k + 1][1]["round"] - edits[k][1]["round"] for k in range(len(edits) - 1)
    ]
    between = [edits[k + 1][0] - edits[k][0] - 1 for k in range(len(edits) - 1)]
    if gaps:
        ordered = sorted(gaps)
        median = ordered[len(ordered) // 2]
        print(
            f"  mutations {len(edits)} | gap rounds median {median} max {max(gaps)}"
            f" | actions between mutations median {sorted(between)[len(between)//2]} max {max(between)}"
        )

    # --- verification loop ------------------------------------------------
    verify = [a for a in post if a["name"] in SHELL_TOOLS]
    classes = collections.Counter(shell_class(a["target"]) for a in verify)
    print(f"  shell commands {len(verify)} {dict(classes.most_common())}")

    gating = [a for a in verify if shell_class(a["target"]) in {"BUILD", "TEST", "CHECK"}]
    repeated_cmd = 0
    no_mutation_between = 0
    repeated_diag = 0
    seen_cmd: set[str] = set()
    last_diag: str | None = None
    mutated_since_last_verify = True
    first_pass = {"BUILD": None, "TEST": None, "CHECK": None}
    for action in post:
        if action["name"] in EDIT_TOOLS:
            mutated_since_last_verify = True
            continue
        if action["name"] not in SHELL_TOOLS:
            continue
        kind = shell_class(action["target"])
        if kind not in {"BUILD", "TEST", "CHECK"}:
            continue
        if action["target"] in seen_cmd:
            repeated_cmd += 1
        seen_cmd.add(action["target"])
        if not mutated_since_last_verify:
            no_mutation_between += 1
        mutated_since_last_verify = False
        if action["is_error"]:
            fingerprint = diagnostic(action["preview"])
            if fingerprint == last_diag:
                repeated_diag += 1
            last_diag = fingerprint
        else:
            last_diag = None
            if first_pass[kind] is None:
                first_pass[kind] = action["round"]
    print(
        f"  gating commands {len(gating)} | repeated command {repeated_cmd}"
        f" | re-run with no mutation since last {no_mutation_between}"
        f" | same diagnostic twice in a row {repeated_diag}"
    )
    print(f"  first pass: {first_pass}")

    # --- exploration after the first green --------------------------------
    green_round = min([r for r in first_pass.values() if r], default=None)
    if green_round:
        after = [
            a
            for a in post
            if a["round"] > green_round and a["name"] in READ_TOOLS | SEARCH_TOOLS
        ]
        print(f"  reads/searches after first green (r{green_round}): {len(after)}")

    # --- post-edit exploration shape --------------------------------------
    explore = [a for a in post if a["name"] in READ_TOOLS | SEARCH_TOOLS]
    pre_seen = {a["target"] for a in actions[:first_edit] if a["name"] in READ_TOOLS}
    edited = {p for a in post if a["name"] in EDIT_TOOLS for p in a["target"].split(",")}
    revisit = sum(1 for a in explore if a["name"] in READ_TOOLS and a["target"] in pre_seen)
    confirm = sum(1 for a in explore if a["name"] in READ_TOOLS and a["target"] in edited)
    print(
        f"  post-edit exploration {len(explore)}"
        f" | re-reads of files already read pre-edit {revisit}"
        f" | reads of files it just edited {confirm}"
    )

    if full:
        print("  --- timeline ---")
        for action in post:
            mark = "✗" if action["is_error"] else "✓"
            kind = action["name"]
            if kind in SHELL_TOOLS:
                kind = f"{shell_class(action['target'])}"
            note = ""
            if action["is_error"] and action["name"] in SHELL_TOOLS:
                note = f"  :: {diagnostic(action['preview'])[:80]}"
            print(f"   r{action['round']:<3} {mark} {kind:<11} {action['target'][:74]}{note}")


def main() -> None:
    args = [a for a in sys.argv[1:] if a != "--full"]
    full = "--full" in sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    for pattern in args:
        actions, label = load(pattern)
        report(actions, label[-42:], full)


if __name__ == "__main__":
    main()
