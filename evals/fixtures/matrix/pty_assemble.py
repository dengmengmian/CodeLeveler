#!/usr/bin/env python3
"""Turn a lab directory into the committed evidence tree, and count the coverage.

Raw PTY streams and session stores stay in the lab (they are large and full of
one machine's paths). What lands in the repository is what a reader can check:
per-run metadata, the metrics, a few selected screens, the replay invariants,
and a coverage table nobody had to type.

    python3 evals/fixtures/matrix/pty_assemble.py --lab <lab> --out <evidence dir>
"""

import argparse
import glob
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_metrics import facts  # noqa: E402

# Which screens are worth committing per run: the ones a claim rests on.
KEEP_SCREENS = [
    "startup", "task", "diff", "final", "overlay", "after-answer",
    "post-resume", "pre-interrupt", "after-cancel", "scroll-pageup-1",
    "after-resize-soak", "long-task",
]


def load(path):
    with open(path) as fh:
        return json.load(fh)


def stage_files(labs):
    """Every stage result across every lab, newest lab last.

    Runs that a driver defect invalidated live under `out/invalidated/` and are
    deliberately not matched here: they are evidence for a finding, not for a
    verdict.
    """
    return [p for lab in labs
            for p in sorted(glob.glob(os.path.join(lab, "out", "stage-*.json")))]


def assemble_pty(labs, out):
    runs, findings = [], []
    for path in stage_files(labs):
        stage = load(path)
        for r in stage.get("results", []):
            run_id = r["run_id"]
            lab_tag = os.path.basename(os.path.dirname(os.path.dirname(path)))
            d = os.path.join(out, "pty", stage["stage"], f"{run_id}@{lab_tag}")
            os.makedirs(d, exist_ok=True)
            meta = {k: v for k, v in r.items()
                    if k not in ("screens", "samples", "rounds", "findings",
                                 "mid_turn")}
            meta["rounds"] = r["rounds"]
            with open(os.path.join(d, "metadata.json"), "w") as fh:
                json.dump(meta, fh, ensure_ascii=False, indent=2)
            with open(os.path.join(d, "metrics.json"), "w") as fh:
                json.dump({"summary": r.get("metrics", {}),
                           "samples": r.get("samples", []),
                           "mid_turn": r.get("mid_turn", [])}, fh, indent=2)
            # Named screens, plus one mid-turn sample: a long task's last frame
            # says nothing about whether the interface stayed alive during it.
            screens = r.get("screens") or {}
            kept = {k: v for k, v in screens.items() if k in KEEP_SCREENS}
            mid = sorted(k for k in screens if "-t" in k and k.endswith("s"))
            if mid:
                kept[mid[len(mid) // 2]] = screens[mid[len(mid) // 2]]
            if kept:
                with open(os.path.join(d, "screen-selected.txt"), "w") as fh:
                    for label, text in kept.items():
                        fh.write(f"===== {label} =====\n{text}\n\n")
            runs.append(dict(meta, stage=stage["stage"], lab=lab_tag))
            for f in r.get("findings", []):
                findings.append(dict(f, run=run_id, stage=stage["stage"]))
    return runs, findings


def coverage(labs, runs, findings, replay):
    """The table. Every number is counted, none is asserted."""
    def rounds_of(run, label_startswith):
        return [x for x in run.get("rounds", []) if x["label"].startswith(label_startswith)]

    stores = [p for lab in labs
              for p in sorted(glob.glob(os.path.join(lab, "sessions", "*", "sessions.db")))]
    metrics = [row for p in stores for row in facts(p)]
    projects = {r["project"] for r in runs}
    langs = set()
    for r in runs:
        t = (r.get("type") or "")
        if t.startswith("rust"):
            langs.add("Rust")
        elif t.startswith("go"):
            langs.add("Go")
        elif t.startswith("node"):
            langs.add("TypeScript/JavaScript")

    resize_cycles = sum(len(rounds_of(r, "resize-")) for r in runs)
    scroll_rounds = sum(len(rounds_of(r, "scroll")) for r in runs)
    plans = [s["plan_steps_max"] for s in replay.get("sessions", [])]
    return {
        "Projects": len(projects),
        "Languages": len(langs),
        "PTY sessions": len(runs),
        "Replay sessions": len(replay.get("sessions", [])),
        "Long sessions": sum(1 for m in metrics if m["wall_seconds"] >= 600),
        "Blocked / incomplete outcomes": sum(
            1 for m in metrics if m["outcome"] not in ("completed", None)
        ) + replay.get("totals", {}).get("terminals", {}).get("TurnIncomplete", 0),
        "Cancel": sum(len(rounds_of(r, "esc-cancel")) for r in runs),
        "Resume": sum(len(rounds_of(r, "reopen")) for r in runs),
        "Permission approve": sum(
            1 for r in runs for x in rounds_of(r, "approve-outcome")
        ),
        "Permission deny": sum(1 for r in runs for x in rounds_of(r, "deny-outcome")),
        "Resize cycles": resize_cycles,
        "Scroll interactions": scroll_rounds,
        "Large patches (>=1000 char args)": sum(
            1 for s in replay.get("sessions", [])
            for name, n in s.get("largest_args", []) if n >= 1000
        ),
        "New files created": replay.get("totals", {}).get("new_file_calls", 0),
        "Plans >= 7 steps": sum(1 for n in plans if n >= 7),
        "Longest plan seen": max(plans) if plans else 0,
        "Total engine events (replay)": replay.get("totals", {}).get("events", 0),
        "Total tool calls (replay)": replay.get("totals", {}).get("tool_calls", 0),
        "PTY findings": len(findings),
        "_projects": sorted(projects),
        "_languages": sorted(langs),
    }


def runtime_performance(labs):
    stores = [p for lab in labs
              for p in sorted(glob.glob(os.path.join(lab, "sessions", "*", "sessions.db")))]
    rows = [row for p in stores for row in facts(p)]
    if not rows:
        return {}

    def med(key):
        vals = sorted(r[key] for r in rows if r.get(key) is not None)
        return vals[len(vals) // 2] if vals else None

    worst = max(rows, key=lambda r: r["wall_seconds"])
    return {
        "sessions": len(rows),
        "median_wall_seconds": med("wall_seconds"),
        "median_rounds": med("rounds"),
        "median_model_wait_share": med("model_wait_share"),
        "median_tool_share": med("tool_share"),
        "median_verification_share": med("verification_share"),
        "median_clarification_share": med("clarification_share"),
        "median_harness_share": med("harness_share"),
        "worst_wall_seconds": worst["wall_seconds"],
        "worst_wall_run": worst["run"],
        "worst_rounds": max(r["rounds"] for r in rows),
        "worst_harness_seconds": max(r["harness_seconds"] for r in rows),
        "worst_harness_run": max(rows, key=lambda r: r["harness_seconds"])["run"],
        "sessions_with_zero_harness_residue": sum(
            1 for r in rows if r["harness_seconds"] == 0.0
        ),
        "model_errors": sum(r["model_errors"] for r in rows),
        "retries": sum(r["retries"] for r in rows),
        "input_tokens": sum(r["input_tokens"] for r in rows),
        "cached_input_tokens": sum(r["cached_input_tokens"] for r in rows),
        "output_tokens": sum(r["output_tokens"] for r in rows),
        "cost_usd": round(sum(r["cost_usd"] for r in rows), 4),
        "rows": rows,
    }


def tui_performance(runs):
    """CPU and RSS of the terminal process itself, by phase."""
    rss_peaks, growth, by_phase = [], [], {}
    for r in runs:
        m = r.get("metrics") or {}
        if not m:
            continue
        rss_peaks.append(m["rss_peak_kb"])
        # The first sample lands between fork and exec, so start-to-end is not
        # growth; the trend inside a long run is. Report peak minus steady.
        growth.append(m["rss_end_kb"] - m["rss_peak_kb"])
        for phase, value in (m.get("cpu_by_phase_max") or {}).items():
            by_phase.setdefault(phase, []).append(value)
    return {
        "runs_sampled": len(rss_peaks),
        "rss_peak_kb_max": max(rss_peaks) if rss_peaks else None,
        "rss_peak_kb_median": sorted(rss_peaks)[len(rss_peaks) // 2] if rss_peaks else None,
        "rss_end_minus_peak_kb_max": max(growth) if growth else None,
        "cpu_max_by_phase": {k: max(v) for k, v in by_phase.items()},
        "cpu_median_of_maxima_by_phase": {
            k: sorted(v)[len(v) // 2] for k, v in by_phase.items()
        },
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--labs", nargs="+", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    runs, findings = assemble_pty(args.labs, args.out)

    replay_path = os.path.join(args.labs[0], "out", "corpus-full-result.json")
    if not os.path.exists(replay_path):
        replay_path = os.path.join(args.labs[0], "out", "corpus-historical-result.json")
    replay = load(replay_path) if os.path.exists(replay_path) else {}

    cov = coverage(args.labs, runs, findings, replay)
    perf = runtime_performance(args.labs)
    tui = tui_performance(runs)

    os.makedirs(os.path.join(args.out, "replay"), exist_ok=True)
    with open(os.path.join(args.out, "replay", "invariants.json"), "w") as fh:
        json.dump(replay.get("totals", {}), fh, ensure_ascii=False, indent=2)
    with open(os.path.join(args.out, "replay", "corpus.json"), "w") as fh:
        json.dump(
            [
                {k: v for k, v in s.items() if k not in ("final_frame",)}
                for s in replay.get("sessions", [])
            ],
            fh, ensure_ascii=False, indent=2,
        )
    with open(os.path.join(args.out, "manifest.json"), "w") as fh:
        json.dump({"coverage": cov, "agent_runtime": perf, "pty_tui": tui,
                   "pty_findings": findings}, fh, ensure_ascii=False, indent=2)

    print("coverage")
    for k, v in cov.items():
        if not k.startswith("_"):
            print(f"  {k:<34} {v}")
    print(f"  projects: {', '.join(cov['_projects'])}")
    print(f"  languages: {', '.join(cov['_languages'])}")
    print("\nagent runtime")
    for k, v in perf.items():
        if k != "rows":
            print(f"  {k:<34} {v}")
    print("\npty tui")
    for k, v in tui.items():
        print(f"  {k:<34} {v}")
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
