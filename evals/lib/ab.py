"""Multi-arm product comparison over one task set. Pure functions.

A run record is what `evals/scripts/multi_agent_ab.py` observed: the
independent oracle (`expect_pass`), the product's own checks re-run on the
final tree (`visible_checks_pass`), and facts read from the event log.
Nothing here counts a spawn as success.
"""

from __future__ import annotations

from statistics import mean, median
from typing import Any

COMPLETED = {"completed", "verified", "completed_unverified"}


def arm_order(arms: list[str], slot: int) -> list[str]:
    """Rotate arm order per slot so no arm always runs first (provider drift)."""
    k = slot % len(arms)
    return arms[k:] + arms[:k]


def _useful_child(child: dict[str, Any], terminal: dict[str, Any], run: dict[str, Any]) -> str | None:
    if terminal.get("stop") != "completed" or not str(terminal.get("outcome") or "").startswith("completed"):
        return None
    lc = run["lifecycle"]
    if child.get("role") == "reviewer":
        return "independent_review"
    if child.get("read_only") is False:
        changed = set(run.get("changed_files") or [])
        kept = [p for p in (lc.get("child_mutations") or {}).get(child["id"], []) if p in changed]
        return "independent_subtask" if kept and run.get("expect_pass") else None
    if terminal.get("outcome") == "completed_with_findings" and (
        lc.get("parent_rereads_after_child", 0) < lc.get("child_read_paths", 0)
    ):
        return "evidence_not_redone"
    return None


def judge_run(run: dict[str, Any]) -> dict[str, Any]:
    out = dict(run)
    verified = run.get("verification_status") == "passed"
    out["false_verified"] = verified and run.get("visible_checks_pass") is False
    out["incorrect_and_verified"] = verified and run.get("expect_pass") is False
    out["completed_but_incorrect"] = run.get("task_outcome") in COMPLETED and run.get("expect_pass") is False

    useful: dict[str, str] = {}
    natural = [c for c in run.get("children") or [] if c.get("role") != "reviewer"]
    for child in run.get("children") or []:
        label = _useful_child(child, (run.get("child_terminals") or {}).get(child["id"]) or {}, run)
        if label:
            useful[child["id"]] = label
    out["useful_children"] = useful
    out["delegated"] = bool(natural)
    out["independent_review"] = "independent_review" in useful.values()
    out["useful_delegation"] = any(useful.get(c["id"]) for c in natural)
    out["unnecessary_delegation"] = bool(natural) and (
        run.get("category") == "SIMPLE" or not out["useful_delegation"]
    )
    return out


def _rate(num: int, den: int) -> float | None:
    return round(num / den, 4) if den else None


def _mean(xs: list[float]) -> float | None:
    return round(mean(xs), 2) if xs else None


def aggregate(runs: list[dict[str, Any]]) -> dict[str, Any]:
    n = len(runs)
    delegated = [r for r in runs if r["delegated"]]
    lc = [r["lifecycle"] for r in runs]
    usage = [r.get("usage") or {} for r in runs]
    children = sum(x.get("children", 0) for x in lc)
    costs = [u.get("cost_usd_micros") for u in usage]
    walls = [float(r["wall_s"]) for r in runs if r.get("wall_s") is not None]

    def total(key: str) -> int:
        return sum(int(u.get(key) or 0) for u in usage)

    return {
        "n": n,
        "task_success_rate": _rate(sum(1 for r in runs if r.get("expect_pass")), n),
        "false_verified_total": sum(1 for r in runs if r["false_verified"]),
        "incorrect_and_verified_total": sum(1 for r in runs if r["incorrect_and_verified"]),
        "completed_but_incorrect_total": sum(1 for r in runs if r["completed_but_incorrect"]),
        "verification_pass_rate": _rate(sum(1 for r in runs if r.get("verification_status") == "passed"), n),
        "delegation_adoption_rate": _rate(len(delegated), n),
        "useful_delegation_rate": _rate(sum(1 for r in delegated if r["useful_delegation"]), len(delegated)),
        "unnecessary_delegation_rate": _rate(sum(1 for r in delegated if r["unnecessary_delegation"]), len(delegated)),
        "independent_review_runs": sum(1 for r in runs if r["independent_review"]),
        "children_total": children,
        "child_success_rate": _rate(sum(x.get("child_success", 0) for x in lc), children),
        "child_partial_rate": _rate(sum(x.get("child_partial", 0) for x in lc), children),
        "untyped_terminals": sum(x.get("untyped_terminals", 0) for x in lc),
        "lost_accepted_child_total": sum(x.get("lost_accepted_child", 0) for x in lc),
        "open_orphan_total": sum(x.get("open_orphan", 0) for x in lc),
        "duplicate_settlement_total": sum(x.get("duplicate_settlement", 0) for x in lc),
        "ownership_violation_total": sum(x.get("ownership_violation", 0) for x in lc),
        "interrupted_total": sum(x.get("interrupted", 0) for x in lc),
        "resumed_total": sum(x.get("resumed", 0) for x in lc),
        "rounds_mean": _mean([float(r["rounds"]) for r in runs if r.get("rounds") is not None]),
        "wall_s_mean": _mean(walls),
        "wall_s_median": round(median(walls), 2) if walls else None,
        "requests_mean": _mean([float(u.get("requests") or 0) for u in usage]),
        "input_tokens_total": total("input_tokens"),
        "output_tokens_total": total("output_tokens"),
        "cached_input_tokens_total": total("cached_input_tokens"),
        # One run with unknown cost makes the arm's total unknown.
        "cost_usd_micros_total": sum(costs) if costs and all(c is not None for c in costs) else None,
        "unpriced_requests": total("unpriced_requests"),
    }
