#!/usr/bin/env python3
"""Tool surface inventory and retrospective usage baseline (T0).

Two halves, both mechanical:

  inventory  — what the model can see, parsed from the registry and the
               `impl Tool for` blocks, never hand-written.
  usage      — what the model actually called, read from the persisted
               `tool_call_started` / `tool_call_finished` events in every
               LEVELER_HOME session database.

Nothing is instrumented for this script: it reads facts the runtime already
persists. A metric the EventLog cannot answer is reported as a gap rather
than estimated.

Sessions are classified by the project directory name:

  REAL     a session run against a real checkout on this machine
  DOGFOOD  a session run against a scratch dogfood workspace
  EVAL     a session run by the evaluation harness (temp fixture repo)

Aggregates only. No prompts, arguments, or file contents are emitted, so the
output is safe to commit.

Usage:
    python3 evals/scripts/tool_surface_baseline.py [--home DIR] [--repo DIR]
                                                   [--json OUT.json]
"""

import argparse
import glob
import hashlib
import json
import os
import re
import shutil
import sqlite3
import sys
import tempfile
from collections import Counter, defaultdict
from datetime import datetime

TOOL_EVENTS = ("tool_call_started", "tool_call_finished")


# ── inventory ────────────────────────────────────────────────────────────────

def _block(text, signature):
    i = text.index(signature)
    return text[i:text.index("\n}", i)]


def inventory(repo):
    src = os.path.join(repo, "crates/leveler-tools/src")
    reg = open(os.path.join(src, "registry.rs")).read()

    core = re.findall(r"Arc::new\(tools::(\w+)\)", _block(reg, "pub fn core_registry()"))
    full = re.findall(r"Arc::new\(tools::(\w+)\)", _block(reg, "pub fn full_registry()"))
    browser = re.findall(r"Arc::new\(tools::(\w+)\)", _block(reg, "fn register_browser("))

    expand = {}
    for m in re.finditer(r'"(\w+)" =>\s*\{(.*?)\n        \}',
                         _block(reg, "pub fn expand_tool_category("), re.S):
        expand[m.group(1)] = re.findall(r"Arc::new\(tools::(\w+)\)", m.group(2))

    known = set()
    expand_src = os.path.join(src, "tools/expand_tools.rs")
    if os.path.exists(expand_src):
        m = re.search(r"const KNOWN: &\[&str\] = &\[(.*?)\];", open(expand_src).read(), re.S)
        if m:
            known = set(re.findall(r'"(\w+)"', m.group(1)))

    facts = {}
    files = glob.glob(f"{src}/tools/**/*.rs", recursive=True) + [f"{src}/mcp.rs"]
    for path in files:
        if not os.path.exists(path):
            continue
        text = open(path).read()
        for m in re.finditer(r"impl Tool for (\w+)\s*\{", text):
            body = text[m.end():]
            nxt = body.find("\nimpl Tool for ")
            if nxt > 0:
                body = body[:nxt]

            def flag(fn, default=False):
                f = re.search(rf"fn {fn}\(&self\)\s*->\s*bool\s*\{{\s*(true|false)", body)
                return f.group(1) == "true" if f else default

            risk = re.search(
                r"fn risk\(&self\)\s*->\s*RiskLevel\s*\{\s*(?://[^\n]*\n\s*)*RiskLevel::(\w+)", body)
            name = re.search(r'fn name\(&self\)[^{]*\{\s*"([^"]+)"', body)
            facts[m.group(1)] = dict(
                source=os.path.relpath(path, repo),
                name=name.group(1) if name else None,
                risk=risk.group(1) if risk else "?",
                parallel_safe=flag("supports_parallel"),
                replay_safe=flag("replay_is_side_effect_free"),
                mutates_files=flag("mutates_files"),
                runs_command=flag("runs_command"),
            )

    injected = os.path.join(repo, "crates/leveler-agent/src/injected_tools.rs")
    control = re.findall(r'pub\(crate\) const \w+_TOOL: &str = "([^"]+)"', open(injected).read())

    return dict(core=core, full=full, browser=browser, expand=expand,
                expand_known=sorted(known), facts=facts, control=control)


# ── usage ────────────────────────────────────────────────────────────────────

def classify(project_dir):
    if "leveler-eval" in project_dir:
        return "EVAL"
    if "dogfood" in project_dir:
        return "DOGFOOD"
    return "REAL"


def read_sessions(home, workdir):
    """Copy each session database (with its WAL) and read it. Copying keeps the
    live databases untouched; without the WAL, recent sessions read as empty."""
    calls, finished, sessions, requests = [], [], [], []
    for db in sorted(glob.glob(f"{home}/state/projects/*/sessions.db")):
        project = os.path.basename(os.path.dirname(db))
        kind = classify(project)
        local = os.path.join(workdir, project + ".db")
        shutil.copy(db, local)
        for suffix in ("-wal", "-shm"):
            if os.path.exists(db + suffix):
                shutil.copy(db + suffix, local + suffix)
        try:
            con = sqlite3.connect(local)
            for sid, model, profile, skind, outcome, created in con.execute(
                    "SELECT id, model, work_profile, kind, outcome, created_at FROM sessions"):
                sessions.append(dict(cls=kind, project=project, sid=sid, model=model,
                                     profile=profile, kind=skind, outcome=outcome,
                                     created=created))
            for sid, typ, payload, ts, seq in con.execute(
                    "SELECT session_id, type, payload, created_at, sequence FROM events "
                    f"WHERE type IN {TOOL_EVENTS} ORDER BY session_id, sequence"):
                try:
                    p = json.loads(payload).get("payload", {})
                except ValueError:
                    continue
                row = dict(cls=kind, project=project, sid=sid, seq=seq, ts=ts,
                           name=p.get("name"), call_id=p.get("call_id"),
                           agent_id=p.get("agent_id"), risk=p.get("risk"))
                if typ == "tool_call_started":
                    calls.append(row)
                else:
                    row["is_error"] = bool(p.get("is_error"))
                    row["preview"] = p.get("preview") or ""
                    finished.append(row)
            for sid, itok, otok, latency in con.execute(
                    "SELECT session_id, input_tokens, output_tokens, latency_ms FROM model_requests"):
                requests.append(dict(cls=kind, sid=sid, input=itok, output=otok, latency=latency))
            con.close()
        except sqlite3.Error as e:
            print(f"skipped {project}: {e}", file=sys.stderr)
    return calls, finished, sessions, requests


def durations(calls, finished):
    """End-to-end wall time per call. This spans admission, any approval wait
    and in-batch scheduling, so it is an upper bound on execution time, not a
    measurement of it."""
    started = {c["call_id"]: c for c in calls}
    out = defaultdict(list)
    for f in finished:
        c = started.get(f["call_id"])
        if not c:
            continue
        try:
            ms = (datetime.fromisoformat(f["ts"]) - datetime.fromisoformat(c["ts"])).total_seconds() * 1000
        except ValueError:
            continue
        if ms >= 0:
            out[c["name"]].append(ms)
    return out


def report(inv, calls, finished, sessions, requests, out_json=None):
    names = {s: f["name"] for s, f in inv["facts"].items() if f["name"]}
    core_names = {names[s] for s in inv["core"] if s in names}
    default_names = {names[s] for s in set(inv["core"]) | set(inv["full"]) | set(inv["browser"])
                     if s in names}
    control = set(inv["control"])

    print("# Tool surface inventory\n")
    print(f"core_registry()      {len(core_names)}")
    print(f"full_registry()      {len(default_names)}   (the default registry)")
    print(f"control tools        {len(control)}   injected by the executor")
    print(f"expand categories    handled {sorted(inv['expand'])}")
    print(f"                     advertised {inv['expand_known']}")
    dead = sorted(set(inv["expand_known"]) - set(inv["expand"]))
    if dead:
        print(f"                     advertised but registers nothing: {dead}")

    print(f"\n{'tool':19} {'core':>5} {'risk':>15} {'par':>4} {'rply':>5} {'mut':>4} {'cmd':>4}  source")
    for _, f in sorted(inv["facts"].items(), key=lambda kv: kv[1]["name"] or ""):
        if f["name"] not in default_names:
            continue
        print(f"{f['name']:19} {'YES' if f['name'] in core_names else '-':>5} {f['risk']:>15} "
              f"{'Y' if f['parallel_safe'] else '-':>4} {'Y' if f['replay_safe'] else '-':>5} "
              f"{'Y' if f['mutates_files'] else '-':>4} {'Y' if f['runs_command'] else '-':>4}  "
              f"{f['source']}")

    fin_by_id = {f["call_id"]: f for f in finished}
    dur = durations(calls, finished)

    for label, group in (("REAL + DOGFOOD", {"REAL", "DOGFOOD"}), ("EVAL", {"EVAL"})):
        sel = [c for c in calls if c["cls"] in group]
        sess = {s["sid"] for s in sessions if s["cls"] in group}
        active = {c["sid"] for c in sel}
        print(f"\n\n# Usage — {label}\n")
        print(f"sessions {len(sess)} ({len(active)} with tool calls)   "
              f"calls {len(sel)}   distinct tools {len({c['name'] for c in sel})}")
        models = Counter(s["model"] for s in sessions if s["cls"] in group)
        profiles = Counter(s["profile"] for s in sessions if s["cls"] in group)
        print(f"models {dict(models)}")
        print(f"profiles {dict(profiles)}")
        if not sel:
            continue

        per_tool = Counter(c["name"] for c in sel)
        used_in = defaultdict(set)
        errors, child = Counter(), Counter()
        for c in sel:
            used_in[c["name"]].add(c["sid"])
            if c["agent_id"]:
                child[c["name"]] += 1
            f = fin_by_id.get(c["call_id"])
            if f and f["is_error"]:
                errors[c["name"]] += 1

        print(f"\n{'tool':19} {'calls':>6} {'sess':>5} {'per':>6} {'err':>4} {'err%':>6} "
              f"{'child':>6} {'p50ms':>7} {'p95ms':>8}  surface")
        for name, n in per_tool.most_common():
            s = len(used_in[name])
            v = sorted(dur.get(name, []))
            p50 = f"{v[len(v)//2]:.0f}" if len(v) >= 3 else "-"
            p95 = f"{v[int(len(v)*0.95)]:.0f}" if len(v) >= 3 else "-"
            surface = ("core" if name in core_names else
                       "control" if name in control else
                       "optional" if name in default_names else "UNKNOWN")
            print(f"{name:19} {n:6} {s:5} {n/s:6.1f} {errors[name]:4} "
                  f"{100*errors[name]/n:5.1f}% {child[name]:6} {p50:>7} {p95:>8}  {surface}")

        never = sorted(default_names - set(per_tool))
        print(f"\nnever called, but visible by default: {len(never)} of {len(default_names)}")
        for i in range(0, len(never), 5):
            print("  " + ", ".join(never[i:i+5]))
        unknown = sorted(set(per_tool) - default_names - control)
        if unknown:
            print(f"called but not in the registry (hallucinated or since removed): {unknown}")

        rs = [r for r in requests if r["cls"] in group]
        if rs:
            lat = sorted(r["latency"] for r in rs if r["latency"])
            print(f"\nmodel requests {len(rs)}   input tokens {sum(r['input'] or 0 for r in rs)}   "
                  f"output tokens {sum(r['output'] or 0 for r in rs)}   "
                  f"latency p50 {lat[len(lat)//2] if lat else 'n/a'}ms")

    if out_json:
        # Project directories and session ids carry local paths. Replace them
        # with stable short digests so the artifact can be committed and still
        # be joined across the two halves.
        def anon(s):
            return hashlib.sha256(s.encode()).hexdigest()[:12]

        json.dump(dict(inventory=inv,
                       usage=[dict(cls=c["cls"], project=anon(c["project"]),
                                   sid=anon(c["sid"]), seq=c["seq"], name=c["name"],
                                   is_child=bool(c["agent_id"]), risk=c["risk"])
                              for c in calls],
                       sessions=[dict(cls=s["cls"], project=anon(s["project"]),
                                      sid=anon(s["sid"]), model=s["model"],
                                      profile=s["profile"], kind=s["kind"],
                                      outcome=s["outcome"], created=s["created"])
                                 for s in sessions]),
                  open(out_json, "w"), indent=1)
        print(f"\nwrote {out_json}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--home", default=os.environ.get("LEVELER_HOME", os.path.expanduser("~/.leveler")))
    ap.add_argument("--repo", default=os.getcwd())
    ap.add_argument("--json")
    args = ap.parse_args()

    inv = inventory(args.repo)
    with tempfile.TemporaryDirectory() as tmp:
        calls, finished, sessions, requests = read_sessions(args.home, tmp)
    report(inv, calls, finished, sessions, requests, args.json)


if __name__ == "__main__":
    main()
