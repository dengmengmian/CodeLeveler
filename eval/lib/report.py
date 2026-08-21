"""Markdown and CSV reports from unified eval batches."""

from __future__ import annotations

import csv
import io
from typing import Any

from metrics import (
    adoption_summary,
    by_task,
    compare_batches,
    decision_latency_mean,
    describe,
    over_delegation,
    shape_correlation,
    summarize_runs,
    value_by_disposition,
)


def _pct(v: float | None) -> str:
    if v is None:
        return "n/a"
    return f"{v * 100:.0f}%"


def _ci(pair: list[float] | tuple[float, float] | None) -> str:
    if not pair:
        return ""
    return f"[{pair[0] * 100:.0f}%, {pair[1] * 100:.0f}%]"


def markdown_report(
    *,
    title: str,
    batches: dict[str, list[dict[str, Any]]],
    comparison: dict[str, Any] | None = None,
) -> str:
    lines = [f"# {title}", ""]
    lines.append("Observer report. Spawn is `sub_agent_started` with role ≠ reviewer, counted once per child id.")
    lines.append("KEEP is a first-class outcome. Invalid (never engaged) runs are excluded from rates.")
    lines.append("")
    for name, runs in batches.items():
        s = summarize_runs(runs, spawn_likely_only=True)
        over = over_delegation(runs)
        lines.append(f"## Arm `{name}`")
        lines.append("")
        lines.append("| metric | value | 90% Wilson |")
        lines.append("| --- | ---: | --- |")
        lines.append(f"| n valid (spawn-likely) | {s['n_valid']} | |")
        lines.append(f"| spawn rate | {_pct(s['spawn_rate'])} ({s['spawn_n']}/{s['n_valid']}) | {_ci(s['spawn_wilson90'])} |")
        lines.append(f"| offered rate | {_pct(s['offered_rate'])} | |")
        lines.append(f"| kept rate | {_pct(s['kept_rate'])} | |")
        lines.append(f"| useful-child rate | {_pct(s['useful_rate'])} | |")
        lines.append(f"| P_over (KEEP controls) | {_pct(over['p_over'])} ({over['spawn_n']}/{over['n_valid']}) | {_ci(over['wilson90'])} |")
        if s["insufficient_n"]:
            lines.append("")
            lines.append("_n < 6 valid runs: no verdict from this arm alone._")
        lines.append("")
        lines.append("### Per task")
        lines.append("")
        lines.append("| task | n valid | spawn | rate |")
        lines.append("| --- | ---: | ---: | ---: |")
        for tid, stats in by_task(runs).items():
            lines.append(
                f"| `{tid}` | {stats['n_valid']} | {stats['spawn_n']} | {_pct(stats['spawn_rate'])} |"
            )
        lines.append("")
    if comparison:
        lines.append("## Comparison")
        lines.append("")
        lines.append(f"- Δ spawn rate: {comparison.get('delta_spawn_rate')}")
        lines.append(f"- Fisher exact two-sided p: {comparison.get('fisher_p')}")
        lines.append(f"- verdict: **{comparison.get('verdict')}**")
        table = comparison.get("table") or {}
        lines.append("")
        lines.append("|  | spawn | no spawn |")
        lines.append("| --- | ---: | ---: |")
        lines.append(f"| control | {table.get('control_spawn')} | {table.get('control_nospawn')} |")
        lines.append(f"| treatment | {table.get('treatment_spawn')} | {table.get('treatment_nospawn')} |")
        lines.append("")
    return "\n".join(lines) + "\n"


def csv_summary(runs: list[dict[str, Any]]) -> str:
    buf = io.StringIO()
    fields = [
        "run_id",
        "task_id",
        "arm",
        "model",
        "valid",
        "disposition",
        "spawn",
        "offered",
        "offer_trigger",
        "offer_round",
        "first_edit_round",
        "kept",
        "delegated",
        "natural_spawn_count",
        "useful_child_count",
        "rounds",
        "ownership_granted",
        "ownership_denied",
        "verifier_passed",
        "expected_disposition",
    ]
    writer = csv.DictWriter(buf, fieldnames=fields)
    writer.writeheader()
    for run in runs:
        d = run.get("delegation") or {}
        e = run.get("edits") or {}
        m = run.get("metrics") or {}
        t = run.get("task") or {}
        s = run.get("safety") or {}
        v = run.get("verifier") or {}
        writer.writerow(
            {
                "run_id": (run.get("run") or {}).get("id"),
                "task_id": t.get("id"),
                "arm": (run.get("arm") or {}).get("name"),
                "model": (run.get("model") or {}).get("ref"),
                "valid": m.get("valid"),
                "disposition": m.get("disposition"),
                "spawn": m.get("spawn"),
                "offered": d.get("offered"),
                "offer_trigger": d.get("offer_trigger"),
                "offer_round": d.get("offer_round"),
                "first_edit_round": e.get("first_edit_round"),
                "kept": d.get("kept"),
                "delegated": d.get("delegated"),
                "natural_spawn_count": d.get("natural_spawn_count"),
                "useful_child_count": d.get("useful_child_count"),
                "rounds": e.get("rounds"),
                "ownership_granted": s.get("ownership_granted"),
                "ownership_denied": s.get("ownership_denied"),
                "verifier_passed": v.get("passed"),
                "expected_disposition": t.get("expected_disposition"),
            }
        )
    return buf.getvalue()


def experiment_report(batch: dict[str, Any], *, experiment: dict[str, Any] | None = None) -> str:
    """Auto report. Headings are fixed so CC/M-3 does not hand-edit numbers."""
    runs = batch.get("runs") or []
    exp = experiment or batch.get("experiment") or {}
    adopt = adoption_summary(runs)
    spawn_rate = summarize_runs(runs, spawn_likely_only=False)
    latencies = []
    turns = []
    for run in runs:
        lat = (run.get("metrics") or {}).get("decision_latency_rounds")
        if isinstance(lat, (int, float)):
            latencies.append(float(lat))
        rnd = (run.get("edits") or {}).get("rounds")
        if isinstance(rnd, (int, float)):
            turns.append(float(rnd))
    lat_d = describe(latencies)
    turn_d = describe(turns)
    ver_ran = sum(1 for r in runs if (r.get("verifier") or {}).get("ran"))
    ver_pass = sum(1 for r in runs if (r.get("verifier") or {}).get("passed") is True)
    ver_fail = sum(1 for r in runs if (r.get("verifier") or {}).get("passed") is False)
    name = exp.get("experiment") or batch.get("batch_id") or "eval"
    suite = exp.get("suite") or "adoption"
    lines = [
        f"# {suite} / {name}",
        "",
        "Generated by the eval observer. KEEP is first-class. Do not mix safety probes into adoption rates.",
        "",
        "## Experiment",
        "",
        f"- suite: `{suite}`",
        f"- experiment: `{name}`",
        f"- model: `{exp.get('model') or batch.get('model')}`",
        f"- provider: `{exp.get('provider')}`",
        f"- binary: `{exp.get('binary')}`",
        f"- runs per task: {exp.get('runs')}",
        f"- timeout_seconds: {exp.get('timeout_seconds')}",
        f"- population: `{exp.get('population', 'model_initiated_only')}`",
        f"- exclude: {', '.join(str(x) for x in (exp.get('exclude') or [])) or '—'}",
        f"- changes_runtime: {exp.get('changes_runtime', False)}",
        f"- description: {exp.get('description') or '—'}",
        "",
        "## Dataset",
        "",
        f"- n records: {len(runs)}",
        f"- n valid + offer seen: {adopt['n_offer_seen']}",
        "",
        "| task | shape | n | spawn | keep |",
        "| --- | --- | ---: | ---: | ---: |",
    ]
    from collections import defaultdict

    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for run in runs:
        grouped[(run.get("task") or {}).get("id") or "?"].append(run)
    for tid, items in sorted(grouped.items()):
        shape = (items[0].get("task") or {}).get("shape")
        spawn_n = sum(1 for r in items if (r.get("metrics") or {}).get("spawn"))
        lines.append(f"| `{tid}` | {shape} | {len(items)} | {spawn_n} | {len(items) - spawn_n} |")
    ci = adopt.get("adoption_wilson90") or spawn_rate.get("spawn_wilson90")
    lines += [
        "",
        "## Spawn statistics",
        "",
        f"- sample size (offer seen): {adopt['n_offer_seen']}",
        f"- spawn given offer: {adopt['spawn_given_offer']}",
        f"- KEEP given offer: {adopt['keep_given_offer']}",
        f"- adoption rate: {_pct(adopt['adoption_rate'])}",
        f"- raw spawn rate (all valid): {_pct(spawn_rate['spawn_rate'])} ({spawn_rate['spawn_n']}/{spawn_rate['n_valid']})",
        f"- decision latency mean/median/var: {lat_d['mean']} / {lat_d['median']} / {lat_d['variance']} (n={lat_d['n']})",
        f"- turns mean/median/var: {turn_d['mean']} / {turn_d['median']} / {turn_d['variance']} (n={turn_d['n']})",
        "",
        "## Confidence interval",
        "",
        f"- Wilson 90% on adoption rate: {_ci(ci)}",
        f"- insufficient_n: {adopt.get('insufficient_n')}",
        "",
        "## Verifier results",
        "",
        f"- ran: {ver_ran}",
        f"- passed: {ver_pass}",
        f"- failed: {ver_fail}",
        f"- not scored: {len(runs) - ver_ran}",
        "",
        "Adoption micro tasks use `expect: true` so verifier-pass is not code quality.",
        "",
        "## Findings",
        "",
    ]
    if adopt["n_offer_seen"] == 0:
        lines.append("- No valid offer-seen runs. Score an EventLog before reading rates.")
    elif adopt.get("insufficient_n"):
        lines.append(f"- n={adopt['n_offer_seen']} < 6: record the rate, do not publish a verdict.")
    else:
        lines.append(
            f"- Adoption { _pct(adopt['adoption_rate']) } on {adopt['n_offer_seen']} offer-seen runs; KEEP is {adopt['keep_given_offer']} of those."
        )
    lines.append("- Safety probes are excluded from this denominator by experiment config.")
    lines.append("")
    return "\n".join(lines) + "\n"


def adoption_micro_report(batch: dict[str, Any], *, title: str = "Adoption Micro Eval Report") -> str:
    runs = batch.get("runs") or []
    adopt = adoption_summary(runs)
    shapes = shape_correlation(runs)
    model = batch.get("model") or ((runs[0].get("model") or {}).get("ref") if runs else None)
    arm = batch.get("arm") or {}
    lines = [
        f"# {title}",
        "",
        "KEEP is a first-class outcome. This report does not treat KEEP as a failure.",
        "Natural spawn = `sub_agent_started` with role ≠ reviewer, once per child id.",
        "",
        "## 1. Dataset",
        "",
        f"- batch: `{batch.get('batch_id')}`",
        f"- n runs: {len(runs)}",
        f"- n with offer seen (valid): {adopt['n_offer_seen']}",
        "",
        "| task | shape | expected | n | spawn | keep |",
        "| --- | --- | --- | ---: | ---: | ---: |",
    ]
    from collections import defaultdict

    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for run in runs:
        grouped[(run.get("task") or {}).get("id") or "?"].append(run)
    for tid, items in sorted(grouped.items()):
        t = (items[0].get("task") or {})
        spawn_n = sum(1 for r in items if (r.get("metrics") or {}).get("spawn"))
        lines.append(
            f"| `{tid}` | {t.get('shape')} | {t.get('expected_disposition')} | {len(items)} | {spawn_n} | {len(items) - spawn_n} |"
        )
    lines += [
        "",
        "## 2. Experiment setup",
        "",
        "- factor: **task shape** (parallel / boundary / single)",
        "- prompt arm: none (product default coordinator hint + MA-WA1 offer only)",
        f"- model: `{model}`",
        f"- arm: `{arm.get('name')}` ({arm.get('factor')}={arm.get('value')})",
        "- runtime: unchanged spawn/claim/ownership/settlement",
        f"- notes: {batch.get('notes') or '—'}",
        "",
        "## 3. Metrics",
        "",
        "Primary: **adoption rate** = P(spawn | offer seen, valid).",
        "Secondary: decision latency (rounds from offer to first KEEP or spawn),",
        "shape correlation, and value (mean parent turns/edits spawn vs KEEP).",
        "Micro `expect` is `true` on purpose — value is cost, not code-quality success.",
        "",
        "## 4. Results",
        "",
        f"- adoption rate: {_pct(adopt['adoption_rate'])} ({adopt['spawn_given_offer']}/{adopt['n_offer_seen']}) {_ci(adopt['adoption_wilson90'])}",
        f"- KEEP given offer: {adopt['keep_given_offer']}",
        f"- mean decision latency: {decision_latency_mean(runs)}",
        "",
        "### Task shape correlation",
        "",
        "| shape | offer seen | spawn | KEEP | adoption | mean latency |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for shape in ("parallel", "boundary", "single"):
        row = shapes.get(shape) or {}
        lines.append(
            f"| {shape} | {row.get('n_offer_seen')} | {row.get('spawn_given_offer')} | {row.get('keep_given_offer')} | {_pct(row.get('adoption_rate'))} | {row.get('mean_decision_latency')} |"
        )
    lines += ["", "### Value (parent cost, not success)", ""]
    for shape in ("parallel", "boundary", "single"):
        v = value_by_disposition(runs, shape=shape)
        lines.append(
            f"- **{shape}**: spawn n={v['spawn']['n']} mean turns={v['spawn']['mean_turns']} mean edits={v['spawn']['mean_edits']}; "
            f"KEEP n={v['keep']['n']} mean turns={v['keep']['mean_turns']} mean edits={v['keep']['mean_edits']}"
        )
    over = over_delegation(runs)
    lines += [
        "",
        f"- P_over (single/KEEP-labelled spawn): {_pct(over['p_over'])} ({over['spawn_n']}/{over['n_valid']})",
        "",
        "## 5. Findings",
        "",
    ]
    par = shapes.get("parallel") or {}
    sin = shapes.get("single") or {}
    if adopt["n_offer_seen"] == 0:
        lines.append("- No valid offer-seen runs yet. Score a real EventLog before reading this section.")
    else:
        lines.append(
            f"- Parallel adoption { _pct(par.get('adoption_rate')) } vs single { _pct(sin.get('adoption_rate')) }."
        )
        lines.append("- If parallel adoption is low, the model is choosing KEEP on independent work — that is the DELEGATION_ADOPTION question, not a runtime block.")
        lines.append("- If single adoption is high, that is over-delegation (P_over), a different defect.")
        if adopt.get("insufficient_n"):
            lines.append("- n < 6 offer-seen runs: insufficient for a verdict.")
    lines += [
        "",
        "## 6. Next hypothesis",
        "",
        "Do not change offer timing (H-C was inconclusive). Next levers, one at a time:",
        "",
        "1. Prompt wording (two git SHAs, same tasks, `--arm control`).",
        "2. Model/provider (`--model` / `--provider`).",
        "3. Tool schema of `spawn_agent` (product change, measured here after).",
        "4. Reconsideration / planner — only after 1–2 show a shape-specific gap.",
        "",
    ]
    return "\n".join(lines) + "\n"


def compare_markdown(control_runs: list[dict[str, Any]], treatment_runs: list[dict[str, Any]], title: str) -> str:
    cmp_ = compare_batches(control_runs, treatment_runs, spawn_likely_only=True)
    return markdown_report(
        title=title,
        batches={"control": control_runs, "treatment": treatment_runs},
        comparison=cmp_,
    )
