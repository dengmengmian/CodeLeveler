#!/usr/bin/env python3
"""Run the multi-agent product closure task set across binaries/arms.

    python3 evals/scripts/multi_agent_ab.py run --out DIR --model deepseek/deepseek-v4-flash \
        --arm baseline=/path/to/beta2/leveler \
        --arm single=/path/to/current/leveler:single \
        --arm multi=/path/to/current/leveler --runs 2
    python3 evals/scripts/multi_agent_ab.py report --out DIR

Observer only: each run is the shipped `leveler run` on a fresh workspace in
an isolated LEVELER_HOME. The only arm input is the shipped
`agents.delegation` key (`:single` writes `false`). Arm order rotates per
slot. A recorded run is never re-run; a run the driver itself did not finish
is kept as an invalid attempt and listed in the report.

Scoring: `expect` is the independent oracle; `go test ./...` on the final
tree before hidden tests are written re-checks the product's own
verification claim. RECOVERY cases are SIGKILLed once mid-run and resumed
with `leveler run --resume`.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import yaml

EVAL_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EVAL_ROOT / "lib"))

from ab import aggregate, arm_order, judge_run, unit_results  # noqa: E402
from child_lifecycle import child_lifecycle, request_usage  # noqa: E402
from coordination import coordination  # noqa: E402
from eventlog import extract_timeline  # noqa: E402
from runner import ARM_MULTI, ARM_SINGLE, default_user_config, prepare_home  # noqa: E402
from spawn_metric import connect_ro, event_rows  # noqa: E402

SUITES = {
    "product_closure": (EVAL_ROOT / "cases" / "multi_agent_closure",
                        EVAL_ROOT / "suites" / "multi_agent" / "product_closure" / "catalog.json"),
    "value_threshold": (EVAL_ROOT / "cases" / "multi_agent_threshold",
                        EVAL_ROOT / "suites" / "multi_agent" / "value_threshold" / "catalog.json"),
}
RUN_TIMEOUT_S = 1800
CHILD_PROGRESS_BEFORE_KILL = 2
PARENT_PROGRESS_BEFORE_KILL = 6


def now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_arm(spec: str) -> dict:
    name, _, rest = spec.partition("=")
    binary, _, mode = rest.partition(":")
    if not name or not binary or mode not in ("", "single"):
        raise SystemExit(f"bad --arm {spec!r}: want name=/path/to/leveler[:single]")
    version = subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.strip()
    return {"name": name, "binary": binary, "single": mode == "single", "version": version}


def materialize(case: dict, ws: Path) -> None:
    for rel, body in case["files"].items():
        path = ws / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
    git = ["git", "-c", "user.email=eval@codeleveler", "-c", "user.name=eval"]
    subprocess.run(["git", "init", "-q"], cwd=ws, check=True)
    subprocess.run(["git", "add", "-A"], cwd=ws, check=True)
    subprocess.run([*git, "commit", "-qm", "base"], cwd=ws, check=True)


def session_db(home: Path) -> Path | None:
    dbs = sorted(home.glob("state/projects/*/sessions.db"), key=os.path.getmtime)
    return dbs[-1] if dbs else None


def progress(home: Path) -> tuple[bool, int, int]:
    """(child started, child tool results, parent tool results) so far."""
    db = session_db(home)
    if db is None:
        return False, 0, 0
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=2)
        rows = con.execute(
            "select type, json_extract(payload, '$.payload.agent_id'), json_extract(payload, '$.payload.role') "
            "from events where type in ('sub_agent_started', 'tool_call_finished')").fetchall()
        con.close()
    except sqlite3.OperationalError:
        return False, 0, 0
    # A harness-launched reviewer is not the model's delegation.
    started = any(t == "sub_agent_started" and role != "reviewer" for t, _, role in rows)
    rows = [(t, a) for t, a, _ in rows]
    child = sum(1 for t, a in rows if t == "tool_call_finished" and a)
    parent = sum(1 for t, a in rows if t == "tool_call_finished" and not a)
    return started, child, parent


def launch(cmd: list[str], ws: Path, env: dict, log: Path) -> subprocess.Popen:
    # Own process group, so a kill also takes the commands the run started and
    # nothing keeps writing the tree under the resumed run or the checks.
    return subprocess.Popen(cmd, cwd=ws, env=env, stdout=open(log.with_suffix(".jsonl"), "w"),
                            stderr=open(log.with_suffix(".err"), "w"), start_new_session=True)


def kill_group(proc: subprocess.Popen) -> None:
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def wait(proc: subprocess.Popen, deadline: float) -> int | str:
    try:
        return proc.wait(timeout=max(1, deadline - time.time()))
    except subprocess.TimeoutExpired:
        kill_group(proc)
        proc.wait()
        return "timeout"


def changed_files(ws: Path) -> list[str]:
    raw = subprocess.run(["git", "status", "--porcelain", "-z", "--untracked-files=all"], cwd=ws,
                         capture_output=True).stdout.decode("utf-8", "replace")
    entries = raw.split("\0")
    out, i = [], 0
    while i < len(entries):
        entry = entries[i]
        if len(entry) > 3:
            out.append(entry[3:])
            if entry[0] in "RC":  # a rename's source path follows as its own entry
                i += 1
        i += 1
    return sorted(out)


def check(cmd: list[str], ws: Path) -> tuple[bool | None, str]:
    """Pass/fail of a command, or None when it did not finish in time."""
    try:
        done = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=600)
    except subprocess.TimeoutExpired as timeout:
        return None, f"timeout after {timeout.timeout}s"
    return done.returncode == 0, done.stdout + done.stderr


def observe(record: dict, home: Path) -> None:
    """Fill a run record from its session's durable log."""
    db = session_db(home)
    if db is None:
        record["error"] = "no session database"
        return
    con = connect_ro(str(db))
    timeline = extract_timeline(con)
    record["task_outcome"] = timeline["task_outcome"]
    record["verification_status"] = timeline["verification_status"]
    record["rounds"] = timeline["rounds"]
    record["children"] = [
        {"id": p["id"], "role": p.get("role"), "read_only": p.get("read_only")}
        for _s, p in event_rows(con, "sub_agent_started") if p.get("id")
    ]
    terminals: dict[str, dict] = {}
    for _s, p in event_rows(con, "sub_agent_finished"):
        if p.get("id") and p["id"] not in terminals:
            terminals[p["id"]] = {"outcome": p.get("outcome"), "stop": p.get("stop"), "ok": p.get("ok")}
    record["child_terminals"] = terminals
    record["lifecycle"] = child_lifecycle(con)
    usage = request_usage(con)
    record["usage"] = usage["total"]
    record["usage_by_lane"] = {"parent": usage["parent"], "child": usage["child"]}
    record["coordination"] = coordination(con)
    con.close()


def run_one(arm: dict, case: dict, meta: dict, rep: int, model: str, root: Path) -> dict:
    base = root / "work" / arm["name"] / case["id"] / str(rep)
    if base.exists():
        shutil.move(str(base), str(base.with_name(f"{rep}.invalid-{int(time.time())}")))
    home, ws = base / "home", base / "ws"
    ws.mkdir(parents=True)
    materialize(case, ws)
    user_cfg = default_user_config()
    prepare_home(home, ARM_SINGLE if arm["single"] else ARM_MULTI, user_cfg if user_cfg.is_file() else None)
    env = dict(os.environ, LEVELER_HOME=str(home))
    env.pop("NODE_OPTIONS", None)

    record = {"case": case["id"], "category": meta["category"], "bucket": meta.get("bucket"),
              "arm": arm["name"], "rep": rep,
              "binary_version": arm["version"], "model": model, "started_at": now(), "host": platform.node()}
    common = ["--repo", str(ws), "--model", model, "--auto-approve", "--output", "jsonl"]
    t0 = time.time()
    deadline = t0 + int(meta.get("timeout_s") or RUN_TIMEOUT_S)
    proc = launch([arm["binary"], "run", case["task"], *common, "--collaboration", "goal",
                   "--max-rounds", str(case.get("max_rounds") or 60)], ws, env, base / "run1")
    exits = []
    if meta.get("kill_and_resume"):
        kill_point = None
        while proc.poll() is None and time.time() < deadline:
            started, child_done, parent_done = progress(home)
            if started and child_done >= CHILD_PROGRESS_BEFORE_KILL:
                kill_point = "child_progress"
            elif not started and parent_done >= PARENT_PROGRESS_BEFORE_KILL:
                kill_point = "parent_progress"
            if kill_point:
                kill_group(proc)
                break
            time.sleep(0.5)
        exits.append(wait(proc, deadline))
        record["kill_point"] = kill_point
        db = session_db(home)
        session = None
        if kill_point and db is not None:
            con = sqlite3.connect(db)
            session = con.execute("select id from sessions order by updated_at desc limit 1").fetchone()[0]
            con.close()
            resume = [arm["binary"], "run", "--resume", session, *common]
            proc = launch(resume, ws, env, base / "run2")
            exits.append(wait(proc, deadline))
            err = (base / "run2.err").read_text(errors="replace")
            if exits[-1] != 0 and "confirm-recovery" in err:
                # What a user does after reading the stop: acknowledge and go on.
                record["recovery_confirmed"] = True
                proc = launch([*resume, "--confirm-recovery"], ws, env, base / "run3")
                exits.append(wait(proc, deadline))
    else:
        exits.append(wait(proc, deadline))
    record["exits"] = exits
    record["wall_s"] = round(time.time() - t0, 1)

    record["changed_files"] = changed_files(ws)
    record["visible_checks_pass"], _ = check(["go", "test", "./..."], ws)
    record["expect_pass"], output = check([case["expect"]["program"], *case["expect"]["args"]], ws)
    record["unit_results"] = unit_results(output)
    (base / "expect.out").write_text(output)

    observe(record, home)
    record["finished_at"] = now()
    return record


def cmd_run(args) -> int:
    root = Path(args.out).resolve()
    arms = [parse_arm(a) for a in args.arm]
    cases_dir, catalog_path = SUITES[args.suite]
    catalog = json.loads(catalog_path.read_text())["cases"]
    cases = [yaml.safe_load((cases_dir / f"{cid}.yaml").read_text()) for cid in catalog]
    if args.only:
        cases = [c for c in cases if c["id"] in set(args.only)]
    root.mkdir(parents=True, exist_ok=True)
    (root / "arms.json").write_text(json.dumps(arms, indent=2))
    slot = 0
    for rep in range(args.runs):
        for case in cases:
            for name in arm_order([a["name"] for a in arms], slot):
                arm = next(a for a in arms if a["name"] == name)
                out = root / "runs" / arm["name"] / case["id"] / f"{rep}.json"
                if out.exists():
                    continue
                record = run_one(arm, case, catalog[case["id"]], rep, args.model, root)
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_text(json.dumps(record, indent=2))
                print(f"{now()} {arm['name']:9} {case['id']:24} rep={rep} expect={record.get('expect_pass')} "
                      f"wall={record['wall_s']}s children={len(record.get('children') or [])} "
                      f"exits={record['exits']}", flush=True)
            slot += 1
    return 0


def cmd_rescore(args) -> int:
    """Re-read every recorded run under the current metrics and oracles.

    Task text is unchanged, so a run stays a valid observation; only how it
    is read changes. The expect verdict the run was first scored with is kept
    beside the new one.
    """
    root = Path(args.out).resolve()
    for path in sorted((root / "runs").rglob("*.json")):
        record = json.loads(path.read_text())
        base = root / "work" / record["arm"] / record["case"] / str(record["rep"])
        cases_dir, _ = SUITES[args.suite]
        case = yaml.safe_load((cases_dir / f"{record['case']}.yaml").read_text())
        if "expect_pass_at_run" not in record:
            record["expect_pass_at_run"] = record.get("expect_pass")
        record.pop("error", None)
        record["expect_pass"], output = check([case["expect"]["program"], *case["expect"]["args"]], base / "ws")
        record["unit_results"] = unit_results(output)
        (base / "expect.rescored.out").write_text(output)
        observe(record, base / "home")
        record["rescored_at"] = now()
        path.write_text(json.dumps(record, indent=2))
        print(f"{record['arm']:9} {record['case']:24} rep={record['rep']} "
              f"expect {record['expect_pass_at_run']} -> {record['expect_pass']}", flush=True)
    return 0


def cmd_report(args) -> int:
    root = Path(args.out).resolve()
    runs = [judge_run(json.loads(p.read_text())) for p in sorted((root / "runs").rglob("*.json"))]
    invalid = sorted(str(p.relative_to(root)) for p in (root / "work").rglob("*.invalid-*"))
    for r in runs:
        # A RECOVERY run the driver never killed did not test recovery.
        if r["category"] == "RECOVERY":
            r["category"] = f"RECOVERY:{r.get('kill_point') or 'not_killed'}"
    arms = sorted({r["arm"] for r in runs})
    categories = sorted({r["category"] for r in runs})
    summary = {
        "arms": json.loads((root / "arms.json").read_text()),
        "by_arm": {a: aggregate([r for r in runs if r["arm"] == a]) for a in arms},
        "by_arm_bucket": {a: {b: aggregate([r for r in runs if r["arm"] == a and r.get("bucket") == b])
                              for b in sorted({r.get("bucket") for r in runs if r.get("bucket")})} for a in arms},
        "by_arm_category": {a: {c: aggregate([r for r in runs if r["arm"] == a and r["category"] == c])
                                for c in categories} for a in arms},
        "invalid_attempts": invalid,
        "errors": [{"arm": r["arm"], "case": r["case"], "rep": r["rep"], "error": r["error"]}
                   for r in runs if r.get("error")],
        "runs": [{k: r.get(k) for k in ("arm", "case", "category", "rep", "expect_pass", "visible_checks_pass",
                                        "task_outcome", "verification_status", "wall_s", "exits", "kill_point",
                                        "delegated", "useful_children", "unnecessary_delegation",
                                        "false_verified", "incorrect_and_verified")}
                 | {"children": len(r.get("children") or []),
                    "stops": r.get("lifecycle", {}).get("by_stop"),
                    "resumed": r.get("lifecycle", {}).get("resumed"),
                    "requests": (r.get("usage") or {}).get("requests"),
                    "cost_usd_micros": (r.get("usage") or {}).get("cost_usd_micros")}
                 for r in runs],
    }
    (root / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary["by_arm"], indent=2))
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--out", required=True)
    r.add_argument("--suite", choices=sorted(SUITES), default="product_closure")
    r.add_argument("--model", required=True)
    r.add_argument("--arm", action="append", required=True)
    r.add_argument("--runs", type=int, default=1)
    r.add_argument("--only", nargs="*")
    rep = sub.add_parser("report")
    rep.add_argument("--out", required=True)
    rescore = sub.add_parser("rescore")
    rescore.add_argument("--out", required=True)
    rescore.add_argument("--suite", choices=sorted(SUITES), default="product_closure")
    args = p.parse_args()
    return {"run": cmd_run, "report": cmd_report, "rescore": cmd_rescore}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
