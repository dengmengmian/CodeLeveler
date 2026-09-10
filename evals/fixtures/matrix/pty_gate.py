#!/usr/bin/env python3
"""Decide the release gates from the evidence, so the verdict is not an opinion.

Each gate names the round that proves it and the round it reads. A gate whose
evidence is missing is UNPROVEN, which is not a pass — the whole point of
recording `precondition-unmet` in the drivers is that an untested boundary must
not look like a tested one.

    python3 evals/fixtures/matrix/pty_gate.py --labs <lab> [<lab> ...] \\
        --replay <corpus result json> [--out gates.json]
"""

import argparse
import glob
import json
import os


def load(path):
    with open(path) as fh:
        return json.load(fh)


def all_runs(labs):
    runs = []
    for lab in labs:
        for path in sorted(glob.glob(os.path.join(lab, "out", "stage-*.json"))):
            stage = load(path)
            for r in stage.get("results", []):
                runs.append(dict(r, stage=stage["stage"], lab=lab))
    return runs


def rounds(run, prefix):
    return [x for x in run.get("rounds", []) if x["label"].startswith(prefix)]


def gate(name, proven, detail):
    return {"gate": name, "verdict": "PASS" if proven else "UNPROVEN", "detail": detail}


def pty_gates(runs):
    """§72 — the terminal product gates."""
    out = []
    hard = [f for r in runs for f in r["findings"]
            if f["kind"] in ("panic", "panic-in-stream", "startup-died", "reopen-died",
                             "composer-dead", "ctrl-c-killed")]
    launches = [x for r in runs for x in rounds(r, "startup")]
    painted = [x for x in launches if x.get("state") == "idle"]
    out.append(gate("PTY launch", launches and len(painted) == len(launches),
                    f"{len(painted)}/{len(launches)} launches reached an idle screen"))

    typed = [x for r in runs for x in rounds(r, "composer-alive")]
    usable = [x for x in typed if x.get("usable")]
    out.append(gate("PTY input", typed and len(usable) == len(typed),
                    f"{len(usable)}/{len(typed)} composer probes echoed"))

    resizes = [x for r in runs for x in rounds(r, "resize-")]
    lost = [x for x in resizes if x.get("missing_regions")]
    out.append(gate("Resize", resizes and not lost,
                    f"{len(resizes)} resizes, {len(lost)} lost a critical region"))

    scrolls = [x for r in runs for x in rounds(r, "scroll") if "pageup_changed" in x]
    moved = [x for x in scrolls if x.get("pageup_changed") and x.get("pagedown_changed")]
    out.append(gate("Scroll", scrolls and len(moved) == len(scrolls),
                    f"{len(moved)}/{len(scrolls)} scroll rounds moved the transcript"))

    appr = [x for r in runs for x in rounds(r, "approve-outcome")]
    ok = [x for x in appr if x.get("canary_deleted") and x.get("overlay_dismissed")]
    out.append(gate("Permission approve", appr and len(ok) == len(appr),
                    f"{len(ok)}/{len(appr)} approvals ran the command and closed"))

    deny = [x for r in runs for x in rounds(r, "deny-outcome")]
    ok = [x for x in deny if not x.get("canary_deleted") and x.get("overlay_dismissed")]
    out.append(gate("Permission deny", deny and len(ok) == len(deny),
                    f"{len(ok)}/{len(deny)} refusals blocked the command and closed"))

    cancels = [x for r in runs for x in rounds(r, "esc-cancel")]
    ok = [x for x in cancels if x.get("went_busy") and not x.get("still_busy")]
    out.append(gate("Cancel", cancels and len(ok) == len(cancels),
                    f"{len(ok)}/{len(cancels)} cancels ended a genuinely busy turn"))

    resumes = [x for r in runs for x in rounds(r, "reopen")]
    facts = [x for r in runs for x in rounds(r, "session-facts")]
    clean = [x for x in facts if x.get("running_turns", 1) == 0]
    out.append(gate("Resume", resumes and facts and len(clean) == len(facts),
                    f"{len(resumes)} reopens; {len(clean)}/{len(facts)} left no running turn"))

    out.append(gate("Terminal restore / no crash", not hard,
                    f"{len(hard)} panic / dead-process / dead-composer findings"))
    return out


def replay_gates(replay):
    """§73 — the projection gates."""
    t = replay.get("totals", {})
    # What is IN a log was written by the binary that recorded it. Sessions from
    # before the argument-bounding fix still carry the truncation they were
    # written with, and no later change can alter that — so the log-content gate
    # is counted over the sessions this binary recorded, and the legacy count is
    # reported beside it rather than folded into the verdict.
    sessions = replay.get("sessions", [])
    live = [s for s in sessions if not s["label"].startswith("legacy:")]
    live_bad = [b for s in live for b in s.get("args_unparseable", [])]
    legacy_bad = len(t.get("args_unparseable", [])) - len(live_bad)
    return [
        gate("Args parseable", not live_bad,
             f'{len(live_bad)} unparseable blobs over {len(live)} sessions recorded by '
             f'this binary ({legacy_bad} in pre-fix legacy sessions, exempt)'),
        gate("Edit filename retained", not t.get("patch_identity_lost"),
             f'{t.get("patch_calls", 0)} patches, {len(t.get("patch_identity_lost", []))} lost their file'),
        gate("Diff truth", not t.get("failed_edit_looks_applied") and not t.get("diff_without_file"),
             f'{t.get("applied_diffs", 0)} applied diffs; '
             f'{len(t.get("failed_edit_looks_applied", []))} failed edits carrying a diff, '
             f'{len(t.get("diff_without_file", []))} without a file header'),
        gate("Outcome glyph truth", not t.get("wrong_success_glyph"),
             f'{len(t.get("wrong_success_glyph", []))} success marks on work that did not succeed, '
             f'over {t.get("frames", 0)} frames'),
        gate("No internal identifiers on screen", not t.get("internal_leak"),
             f'{len(t.get("internal_leak", []))} leaks'),
        gate("Cross-session isolation", not t.get("cross_session_leaks"),
             f'{t.get("cross_session_leaks", 0)} carried-over screens across '
             f'{t.get("sessions", 0)} sessions in one AppState'),
        gate("Event decoding", t.get("decode_failures", 1) == 0,
             f'{t.get("decode_failures")} of {t.get("events", 0)} events failed to decode'),
    ]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--labs", nargs="+", required=True)
    ap.add_argument("--replay", required=True)
    ap.add_argument("--out")
    args = ap.parse_args()

    runs = all_runs(args.labs)
    gates = pty_gates(runs) + replay_gates(load(args.replay))
    width = max(len(g["gate"]) for g in gates)
    for g in gates:
        print(f'{g["gate"]:<{width}}  {g["verdict"]:<9} {g["detail"]}')
    failed = [g for g in gates if g["verdict"] != "PASS"]
    print(f"\nGATES: {len(gates) - len(failed)}/{len(gates)} PASS")
    if args.out:
        with open(args.out, "w") as fh:
            json.dump(gates, fh, ensure_ascii=False, indent=2)
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
