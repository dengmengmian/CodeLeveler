"""Adoption rates and two-arm statistical comparison.

Primary estimand: P(natural spawn | valid engaged run).
KEEP-control tasks are scored separately as over-delegation (P_over).

Methods match the frozen MA-WA1 timing protocol: 90% Wilson intervals and
two-sided Fisher exact on the 2×2 spawn table. n<6 per arm is reported as
insufficient, not as a verdict.
"""

from __future__ import annotations

import math
from typing import Any

Z90 = 1.6448536269514722
Z95 = 1.959963984540054
MIN_N_FOR_VERDICT = 6


def wilson(k: int, n: int, z: float = Z90) -> tuple[float, float]:
    if n <= 0:
        return (0.0, 1.0)
    p = k / n
    d = 1 + z * z / n
    c = p + z * z / (2 * n)
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n))
    return ((c - h) / d, (c + h) / d)


def fisher_two_sided(a: int, b: int, c: int, d: int) -> float:
    """Two-sided Fisher exact p-value for [[a, b], [c, d]]."""
    n = a + b + c + d
    if n == 0:
        return 1.0
    r1 = a + b
    c1 = a + c

    def pr(x: int) -> float:
        if x < 0 or x > r1 or c1 - x < 0 or c1 - x > c + d:
            return 0.0
        return (
            math.comb(r1, x) * math.comb(c + d, c1 - x) / math.comb(n, c1)
        )

    p0 = pr(a)
    return sum(pr(x) for x in range(0, min(r1, c1) + 1) if pr(x) <= p0 + 1e-12)


def rate(k: int, n: int) -> float | None:
    if n <= 0:
        return None
    return k / n


def mean(xs: list[float]) -> float | None:
    if not xs:
        return None
    return sum(xs) / len(xs)


def median(xs: list[float]) -> float | None:
    if not xs:
        return None
    ordered = sorted(xs)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2


def variance(xs: list[float]) -> float | None:
    """Sample variance (n-1). None when n < 2 so we never invent 0."""
    if len(xs) < 2:
        return None
    m = mean(xs)
    assert m is not None
    return sum((x - m) ** 2 for x in xs) / (len(xs) - 1)


def describe(xs: list[float]) -> dict[str, float | int | None]:
    return {
        "n": len(xs),
        "mean": mean(xs),
        "median": median(xs),
        "variance": variance(xs),
        "min": min(xs) if xs else None,
        "max": max(xs) if xs else None,
    }


def summarize_runs(runs: list[dict[str, Any]], *, spawn_likely_only: bool = False) -> dict[str, Any]:
    selected = []
    for run in runs:
        expected = (run.get("task") or {}).get("expected_disposition")
        if spawn_likely_only and expected == "keep":
            continue
        selected.append(run)

    valid = [r for r in selected if (r.get("metrics") or {}).get("valid")]
    n = len(valid)
    spawn_n = sum(1 for r in valid if (r.get("metrics") or {}).get("spawn"))
    offered_n = sum(1 for r in valid if (r.get("delegation") or {}).get("offered"))
    kept_n = sum(1 for r in valid if (r.get("delegation") or {}).get("kept"))
    delegated_n = sum(1 for r in valid if (r.get("delegation") or {}).get("delegated"))
    useful_n = sum(1 for r in valid if (r.get("delegation") or {}).get("useful_child_count", 0) > 0)
    engaged_n = sum(1 for r in valid if (r.get("metrics") or {}).get("engaged"))

    lo, hi = wilson(spawn_n, n)
    return {
        "n_total": len(selected),
        "n_valid": n,
        "n_engaged": engaged_n,
        "spawn_n": spawn_n,
        "offered_n": offered_n,
        "kept_n": kept_n,
        "delegated_n": delegated_n,
        "useful_n": useful_n,
        "spawn_rate": rate(spawn_n, n),
        "offered_rate": rate(offered_n, n),
        "kept_rate": rate(kept_n, n),
        "delegated_rate": rate(delegated_n, n),
        "useful_rate": rate(useful_n, n),
        "spawn_wilson90": [lo, hi],
        "insufficient_n": n < MIN_N_FOR_VERDICT,
    }


def over_delegation(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """P(spawn | KEEP-control task, valid)."""
    keep_runs = [
        r
        for r in runs
        if (r.get("task") or {}).get("expected_disposition") == "keep"
        and (r.get("metrics") or {}).get("valid")
    ]
    n = len(keep_runs)
    k = sum(1 for r in keep_runs if (r.get("metrics") or {}).get("spawn"))
    lo, hi = wilson(k, n)
    return {
        "n_valid": n,
        "spawn_n": k,
        "p_over": rate(k, n),
        "wilson90": [lo, hi],
        "insufficient_n": n < MIN_N_FOR_VERDICT,
    }


def compare_batches(
    control: list[dict[str, Any]],
    treatment: list[dict[str, Any]],
    *,
    spawn_likely_only: bool = True,
) -> dict[str, Any]:
    a = summarize_runs(control, spawn_likely_only=spawn_likely_only)
    b = summarize_runs(treatment, spawn_likely_only=spawn_likely_only)
    a_yes, a_no = a["spawn_n"], a["n_valid"] - a["spawn_n"]
    b_yes, b_no = b["spawn_n"], b["n_valid"] - b["spawn_n"]
    p = fisher_two_sided(a_yes, a_no, b_yes, b_no)
    insufficient = a["insufficient_n"] or b["insufficient_n"]
    delta = None
    if a["spawn_rate"] is not None and b["spawn_rate"] is not None:
        delta = b["spawn_rate"] - a["spawn_rate"]
    verdict = "insufficient_n"
    if not insufficient:
        if delta is not None and p < 0.05 and delta > 0:
            verdict = "treatment_higher"
        elif delta is not None and p < 0.05 and delta < 0:
            verdict = "treatment_lower"
        else:
            verdict = "no_significant_difference"
    return {
        "control": a,
        "treatment": b,
        "delta_spawn_rate": delta,
        "fisher_p": p,
        "table": {"control_spawn": a_yes, "control_nospawn": a_no, "treatment_spawn": b_yes, "treatment_nospawn": b_no},
        "verdict": verdict,
        "min_n": MIN_N_FOR_VERDICT,
    }


def by_task(runs: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    groups: dict[str, list[dict[str, Any]]] = {}
    for run in runs:
        tid = (run.get("task") or {}).get("id") or "unknown"
        groups.setdefault(tid, []).append(run)
    return {tid: summarize_runs(items) for tid, items in sorted(groups.items())}


def _offered_valid(runs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    out = []
    for run in runs:
        m = run.get("metrics") or {}
        d = run.get("delegation") or {}
        if not m.get("valid"):
            continue
        if m.get("offer_seen") or d.get("offered"):
            out.append(run)
    return out


def adoption_summary(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """P(spawn | offer seen, valid). KEEP after an offer is first-class, not a fail."""
    offered = _offered_valid(runs)
    n = len(offered)
    spawn_n = sum(1 for r in offered if (r.get("metrics") or {}).get("spawn"))
    keep_n = n - spawn_n
    lo, hi = wilson(spawn_n, n)
    return {
        "n_offer_seen": n,
        "spawn_given_offer": spawn_n,
        "keep_given_offer": keep_n,
        "adoption_rate": rate(spawn_n, n),
        "adoption_wilson90": [lo, hi],
        "keep_is_first_class": True,
        "insufficient_n": n < MIN_N_FOR_VERDICT,
    }


def decision_latency_mean(runs: list[dict[str, Any]]) -> float | None:
    vals = []
    for run in _offered_valid(runs):
        lat = (run.get("metrics") or {}).get("decision_latency_rounds")
        if isinstance(lat, (int, float)):
            vals.append(float(lat))
    if not vals:
        return None
    return sum(vals) / len(vals)


def shape_correlation(runs: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    groups: dict[str, list[dict[str, Any]]] = {"parallel": [], "boundary": [], "single": []}
    for run in runs:
        shape = (run.get("task") or {}).get("shape") or "unknown"
        groups.setdefault(shape, []).append(run)
    table = {}
    for shape, items in groups.items():
        s = adoption_summary(items)
        lat = decision_latency_mean(items)
        table[shape] = {
            **s,
            "n_valid": sum(1 for r in items if (r.get("metrics") or {}).get("valid")),
            "spawn_n": s["spawn_given_offer"],
            "mean_decision_latency": lat,
        }
    return table


def value_by_disposition(runs: list[dict[str, Any]], *, shape: str | None = None) -> dict[str, Any]:
    """Cost of spawn vs KEEP. Not a success/fail score. Micro `expect` is not code quality."""
    selected = []
    for run in _offered_valid(runs):
        if shape is not None and (run.get("task") or {}).get("shape") != shape:
            continue
        selected.append(run)

    def mean(items: list[dict[str, Any]], field: str) -> float | None:
        vals = []
        for run in items:
            v = (run.get("edits") or {}).get(field)
            if isinstance(v, (int, float)):
                vals.append(float(v))
        if not vals:
            return None
        return sum(vals) / len(vals)

    spawn = [r for r in selected if (r.get("metrics") or {}).get("spawn")]
    keep = [r for r in selected if not (r.get("metrics") or {}).get("spawn")]
    return {
        "shape": shape,
        "spawn": {
            "n": len(spawn),
            "mean_turns": mean(spawn, "rounds"),
            "mean_edits": mean(spawn, "parent_edit_count"),
        },
        "keep": {
            "n": len(keep),
            "mean_turns": mean(keep, "rounds"),
            "mean_edits": mean(keep, "parent_edit_count"),
        },
    }
