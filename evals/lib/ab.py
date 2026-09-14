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


UNKNOWN = "unknown"


def _useful_child(child: dict[str, Any], terminal: dict[str, Any], run: dict[str, Any]) -> str | None:
    """A usefulness label, None for not useful, or UNKNOWN when the terminal
    carries no typed outcome (a log older than typed terminals)."""
    if terminal.get("outcome") is None:
        return UNKNOWN
    if terminal.get("stop") != "completed" or not str(terminal.get("outcome")).startswith("completed"):
        return None
    lc = run["lifecycle"]
    if child.get("role") == "reviewer":
        return "independent_review"
    if child.get("read_only") is False:
        changed = set(run.get("changed_files") or [])
        kept = [p for p in (lc.get("child_mutations") or {}).get(child["id"], []) if p in changed]
        return "independent_subtask" if kept and run.get("expect_pass") else None
    read = (lc.get("child_reads") or {}).get(child["id"]) or []
    reread = (lc.get("parent_rereads") or {}).get(child["id"]) or []
    if terminal.get("outcome") == "completed_with_findings" and len(reread) < len(read):
        return "evidence_not_redone"
    return None


def judge_run(run: dict[str, Any]) -> dict[str, Any]:
    out = dict(run)
    if run.get("error"):
        # No durable record: nothing about delegation or truth is known.
        for key in ("false_verified", "incorrect_and_verified", "completed_but_incorrect", "delegated",
                    "independent_review", "useful_delegation", "unnecessary_delegation"):
            out[key] = None
        out["useful_children"] = {}
        return out
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
    labels = [useful.get(c["id"]) for c in natural]
    if any(label not in (None, UNKNOWN) for label in labels):
        out["useful_delegation"] = True
    elif UNKNOWN in labels:
        out["useful_delegation"] = None
    else:
        out["useful_delegation"] = False
    if not natural:
        out["unnecessary_delegation"] = False
    elif run.get("category") == "SIMPLE":
        out["unnecessary_delegation"] = True
    elif out["useful_delegation"] is None:
        out["unnecessary_delegation"] = None
    else:
        out["unnecessary_delegation"] = not out["useful_delegation"]
    return out


def _rate(num: int, den: int) -> float | None:
    return round(num / den, 4) if den else None


def _mean(xs: list[float]) -> float | None:
    return round(mean(xs), 2) if xs else None


def _total(values: list[Any]) -> int | None:
    """Sum, or None when any value is unknown: a partial sum reads as less."""
    return sum(int(v) for v in values) if values and all(v is not None for v in values) else None


def aggregate(all_runs: list[dict[str, Any]]) -> dict[str, Any]:
    runs = [r for r in all_runs if not r.get("error")]
    n = len(runs)
    delegated = [r for r in runs if r["delegated"]]
    judged = [r for r in delegated if r["useful_delegation"] is not None]
    lc = [r.get("lifecycle") or {} for r in runs]
    usage = [r.get("usage") or {} for r in runs]
    walls = [float(r["wall_s"]) for r in runs if r.get("wall_s") is not None]

    task_terminals = []
    for r in runs:
        terminals = r.get("child_terminals") or {}
        for child in r.get("children") or []:
            if child.get("role") != "reviewer":
                task_terminals.append(terminals.get(child["id"]))
    children = len(task_terminals)
    untyped = sum(1 for t in task_terminals if t is not None and t.get("outcome") is None)
    typed_rates = children > 0 and untyped == 0

    def child_rate(match) -> float | None:
        return _rate(sum(1 for t in task_terminals if t and match(t)), children) if typed_rates else None

    requests = [u.get("requests") for u in usage]
    return {
        "n": n,
        "errored_runs": len(all_runs) - n,
        "task_success_rate": _rate(sum(1 for r in runs if r.get("expect_pass")), n),
        "false_verified_total": sum(1 for r in runs if r["false_verified"]),
        "incorrect_and_verified_total": sum(1 for r in runs if r["incorrect_and_verified"]),
        "completed_but_incorrect_total": sum(1 for r in runs if r["completed_but_incorrect"]),
        "verification_pass_rate": _rate(sum(1 for r in runs if r.get("verification_status") == "passed"), n),
        "delegation_adoption_rate": _rate(len(delegated), n),
        "useful_delegation_rate": _rate(sum(1 for r in judged if r["useful_delegation"]), len(judged)),
        "unnecessary_delegation_rate": _rate(sum(1 for r in judged if r["unnecessary_delegation"]), len(judged)),
        "delegation_usefulness_unknown": len(delegated) - len(judged),
        "independent_review_runs": sum(1 for r in runs if r["independent_review"]),
        "children_total": children,
        "child_success_rate": child_rate(
            lambda t: str(t.get("outcome")).startswith("completed") and t.get("stop") == "completed"),
        "child_partial_rate": child_rate(lambda t: t.get("outcome") == "incomplete_partial"),
        "untyped_terminals": untyped,
        "lost_accepted_child_total": sum(x.get("lost_accepted_child", 0) for x in lc),
        "open_orphan_total": sum(x.get("open_orphan", 0) for x in lc),
        "duplicate_settlement_total": sum(x.get("duplicate_settlement", 0) for x in lc),
        "ownership_violation_total": sum(x.get("ownership_violation", 0) for x in lc),
        "unattributed_child_mutations": sum(x.get("unattributed_child_mutations", 0) for x in lc),
        "interrupted_total": sum(x.get("interrupted", 0) for x in lc),
        "resumed_total": sum(x.get("resumed", 0) for x in lc),
        "rounds_mean": _mean([float(r["rounds"]) for r in runs if r.get("rounds") is not None]),
        "wall_s_mean": _mean(walls),
        "wall_s_median": round(median(walls), 2) if walls else None,
        "requests_mean": _mean([float(v) for v in requests]) if requests and all(v is not None for v in requests) else None,
        "input_tokens_total": _total([u.get("input_tokens") for u in usage]),
        "output_tokens_total": _total([u.get("output_tokens") for u in usage]),
        "cached_input_tokens_total": _total([u.get("cached_input_tokens") for u in usage]),
        "cost_usd_micros_total": _total([u.get("cost_usd_micros") for u in usage]),
        "unpriced_requests": _total([u.get("unpriced_requests") for u in usage]),
    }
