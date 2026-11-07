#!/usr/bin/env python3
"""Long-running soak runner for scheduled automation (Phase 6.4).

This runner is an observer and lifecycle coordinator. It never reimplements
the scheduler: definitions, occurrences, claims and runs are created and
settled by the real `leveler schedule` store, dispatcher and runtime.

Lifecycle:
    preflight  inspect the host, binary identity and isolation preconditions
    dry-run    run a short real acceptance and leave replayable evidence
    start      freeze a plan, create isolated schedules and detach the runner
    status     read persisted state and live store observations
    report     render the evidence bundle (JSON + Markdown)
    stop       controlled stop, OS registration removal and process cleanup
    finalize   stop if needed, clean up, compute the verdict and write reports

Isolation contract:
    every test home lives under a fresh short `/tmp/lvs-*` root;
    every OS label derives from that home;
    the installed executable and `~/.leveler` are never modified;
    cleanup only targets labels and processes whose identity names this root.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import datetime as dt
import glob
import hashlib
import json
import math
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import traceback
import uuid
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple

VERSION = 1
REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_EVIDENCE_ROOT = REPO_ROOT / ".dev" / "evidence" / "phase6.4-soak"
DEFAULT_BINARY = Path(
    os.environ.get(
        "LEVELER_SOAK_BIN",
        "/private/tmp/leveler-ac-release-a4d886f/release/leveler",
    )
)
DEFAULT_COMMIT = "a4d886ff277b0bf4049668429f3c5b1b71f286fd"
DEFAULT_SOURCE_TREE = Path("/private/tmp/leveler-clean-a4d886f")
HEARTBEAT_STALE_SECS = 180
DEFAULT_DRY_RUN_SECS = 420
DEFAULT_SOAK_SECS = 24 * 3600

THRESHOLDS = {
    "max_duplicate_side_effects": 0,
    "max_unexplained_stranded_occurrences": 0,
    "max_orphan_processes_after_cleanup": 0,
    "max_orphan_registrations_after_cleanup": 0,
    "max_unexplained_wake_gap_secs": 300,
    "max_claim_latency_p95_secs": 180.0,
    "max_completion_latency_p95_secs": 300.0,
    "max_rss_growth_bytes": 512 * 1024 * 1024,
    "max_disk_growth_bytes": 512 * 1024 * 1024,
    "max_agent_usage_missing": 0,
    "min_agent_success_ratio": 1.0,
}

REGISTRATION_PREFIX = "com.leveler.schedule."
# A run that observes the host this close to a full disk stops and finalizes
# while there is still room to write its own terminal evidence.
DISK_FLOOR_BYTES = 2 * 1024 * 1024 * 1024


class SoakError(RuntimeError):
    pass


def utcnow() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc)


def iso(value: Optional[dt.datetime] = None) -> str:
    return (value or utcnow()).astimezone(dt.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def parse_iso(value: str) -> dt.datetime:
    match = re.match(r"^(.*?)(?:Z|[+-]\d{2}:?\d{2})$", value)
    base = value
    if match:
        base = match.group(1)
    parsed = dt.datetime.fromisoformat(base)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.astimezone(dt.timezone.utc)


def seconds_between(start: Optional[str], end: Optional[str]) -> Optional[float]:
    if not start or not end:
        return None
    try:
        return (parse_iso(end) - parse_iso(start)).total_seconds()
    except (ValueError, TypeError):
        return None


def atomic_write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp-" + uuid.uuid4().hex[:8])
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def read_json(path: Path, default: Any = None) -> Any:
    try:
        with open(path, "r", encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return default


def append_jsonl(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def append_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(text)
        if not text.endswith("\n"):
            handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def redact(text: str) -> str:
    text = re.sub(r"(?i)(api[_-]?key|authorization|bearer|token)\s*[=:]\s*\S+", r"\1=<redacted>", text)
    text = re.sub(r"sk-[A-Za-z0-9_-]{12,}", "<redacted>", text)
    return text


def run_command(
    args: Sequence[str],
    env: Optional[Dict[str, str]] = None,
    cwd: Optional[Path] = None,
    timeout: float = 60.0,
) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            list(args),
            env=env,
            cwd=str(cwd) if cwd else None,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        return subprocess.CompletedProcess(list(args), 124, "", "timeout: %s" % error)


def pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except (ProcessLookupError, PermissionError):
        return False


def pid_cmdline(pid: int) -> str:
    result = run_command(["ps", "-p", str(pid), "-o", "command="], timeout=10)
    return result.stdout.strip() if result.returncode == 0 else ""


def descendants(pid: int) -> List[int]:
    found: List[int] = []
    frontier = [pid]
    while frontier:
        current = frontier.pop()
        result = run_command(["pgrep", "-P", str(current)], timeout=10)
        for token in result.stdout.split():
            try:
                child = int(token)
            except ValueError:
                continue
            if child not in found:
                found.append(child)
                frontier.append(child)
    return found


def all_processes() -> List[Dict[str, Any]]:
    result = run_command(["ps", "-axo", "pid=,ppid=,rss=,%cpu=,command="], timeout=20)
    rows: List[Dict[str, Any]] = []
    if result.returncode != 0:
        return rows
    for line in result.stdout.splitlines():
        parts = line.strip().split(None, 4)
        if len(parts) < 5:
            continue
        try:
            rows.append(
                {
                    "pid": int(parts[0]),
                    "ppid": int(parts[1]),
                    "rss_kb": int(parts[2]),
                    "cpu": float(parts[3]),
                    "command": parts[4],
                }
            )
        except ValueError:
            continue
    return rows


def matching_processes(root: Path) -> List[Dict[str, Any]]:
    needle = str(root)
    return [
        row
        for row in all_processes()
        if needle in row["command"] and "ps -axo" not in row["command"]
    ]


def launchd_labels() -> List[str]:
    result = run_command(["launchctl", "list"], timeout=20)
    labels: List[str] = []
    if result.returncode != 0:
        return labels
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[2].startswith(REGISTRATION_PREFIX):
            labels.append(parts[2])
    return sorted(set(labels))


def launchd_loaded(label: str) -> bool:
    result = run_command(["launchctl", "print", "gui/%d/%s" % (os.getuid(), label)], timeout=20)
    return result.returncode == 0


def label_for(home: Path) -> str:
    value = 0xCBF29CE484222325
    for byte in str(home).encode("utf-8"):
        value ^= byte
        value = (value * 0x00000100000001B3) & 0xFFFFFFFFFFFFFFFF
    return REGISTRATION_PREFIX + "%016x" % value


def plist_for(label: str) -> Path:
    return Path.home() / "Library" / "LaunchAgents" / (label + ".plist")


def disk_stop_reason(free_bytes: int, floor_bytes: int = DISK_FLOOR_BYTES) -> Optional[str]:
    return "disk-low" if free_bytes < floor_bytes else None


def plist_owned_by_run(content: str, live_root: Path) -> bool:
    return str(live_root) in content


def unregister_lane_fallback(
    lane: Dict[str, Any], live_root: Path, recorder: "Recorder"
) -> Dict[str, Any]:
    """Unregister a lane without its binary, only when the plist is this run's.

    The product CLI is the normal path. After a host restart the lane binary
    under the (wiped) live root can be gone while the LaunchAgent remains
    loaded; this fallback removes it only after the plist names the run's own
    live root.
    """
    label = lane.get("label") or label_for(Path(lane["home"]))
    plist = plist_for(label)
    row: Dict[str, Any] = {
        "lane": lane["name"],
        "label": label,
        "plist": str(plist),
        "action": "no-plist",
    }
    if plist.is_file():
        try:
            content = plist.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            row.update(action="unreadable", error=redact(str(error)))
        else:
            if not plist_owned_by_run(content, live_root):
                row["action"] = "refused-foreign-plist"
            else:
                result = run_command(
                    ["launchctl", "bootout", "gui/%d/%s" % (os.getuid(), label)], timeout=30
                )
                row.update(
                    action="booted-out",
                    returncode=result.returncode,
                    error=redact((result.stderr or "").strip()[:400]) or None,
                )
                if result.returncode == 0 or not launchd_loaded(label):
                    with contextlib.suppress(OSError):
                        plist.unlink()
                        row["plist_removed"] = True
    recorder.event("launchd_bootout_fallback", **row)
    return row


# How long an overlap probe may wait for a real terminal run. An empty run
# list is not terminal and must not pass. When the window ends with nothing
# to judge, the check is UNKNOWN.
OVERLAP_OBSERVE_SECS = 180
# Fault-lane unknown is an injected kill. Cancelled is how quiesce stops a
# live run. Neither is a product shell failure. A snapshot miss that the
# product settled as unknown on any other lane is.
PRODUCT_SHELL_FAILURE_STATES = frozenset({"failed", "unknown", "blocked", "waiting_approval"})
SILENT_LOSS_STATES = frozenset({"skipped", "interrupted", "blocked", "cancelled", "waiting_approval"})


def overlap_observation_status(terminal: bool, policy_ok: bool, elapsed_secs: float) -> str:
    """PASS, FAIL, OPEN, or UNKNOWN. OPEN means the caller should wait."""
    if terminal and policy_ok:
        return "PASS"
    if terminal and not policy_ok:
        return "FAIL"
    if elapsed_secs < OVERLAP_OBSERVE_SECS:
        return "OPEN"
    return "UNKNOWN"


def overlap_probe_status(rows: Sequence[Tuple[bool, bool]], elapsed_secs: float) -> str:
    """Combine per-slot observations. A confirmed violation is FAIL immediately.

    Rows are `(terminal, policy_ok)`. No rows, or a row that never became
    terminal, cannot PASS. After the window that case is UNKNOWN.
    """
    if not rows:
        return "OPEN" if elapsed_secs < OVERLAP_OBSERVE_SECS else "UNKNOWN"
    statuses = [overlap_observation_status(terminal, ok, elapsed_secs) for terminal, ok in rows]
    if any(status == "FAIL" for status in statuses):
        return "FAIL"
    if any(status == "OPEN" for status in statuses):
        return "OPEN"
    if all(status == "PASS" for status in statuses):
        return "PASS"
    return "UNKNOWN"


TERMINAL_RUN_STATES = frozenset(
    {"succeeded", "failed", "unknown", "blocked", "cancelled", "skipped"}
)


def definition_id_for(defs: Dict[str, Any], key: str) -> Optional[str]:
    """Resolve a spec key to the definition id the lane's setup created.

    `lane["defs"][key]` is `{"id", "name", "spec"}`. A bare id is accepted for
    state written by older runner versions; treating the whole mapping as an
    id silently counted zero successes and reported every marker line as a
    duplicate side effect.
    """
    row = (defs or {}).get(key)
    if isinstance(row, dict):
        return row.get("id")
    return row


def effect_success_count(
    succeeded_by_definition: Dict[str, int], defs: Dict[str, Any], key: str
) -> int:
    return int(succeeded_by_definition.get(definition_id_for(defs, key) or "") or 0)


def _runs_for(snapshot: Dict[str, Any], definition_id: Optional[str]) -> List[Dict[str, Any]]:
    if not definition_id:
        return []
    return [run for run in snapshot.get("runs", []) if run.get("definition_id") == definition_id]


def _occurrences_for(snapshot: Dict[str, Any], definition_id: Optional[str]) -> List[Dict[str, Any]]:
    if not definition_id:
        return []
    return [
        occurrence
        for occurrence in snapshot.get("occurrences", [])
        if occurrence.get("definition_id") == definition_id
    ]


def _covering_run(runs: Sequence[Dict[str, Any]], due: dt.datetime) -> Optional[Dict[str, Any]]:
    """A run whose active interval contains `due`."""
    for run in runs:
        if not run.get("started_at") or not run.get("finished_at"):
            continue
        if parse_iso(run["started_at"]) <= due < parse_iso(run["finished_at"]):
            return run
    return None


def _missing_observation(reason: str) -> Dict[str, Any]:
    return {
        "missing": True,
        "reason": reason,
        "occurrence": None,
        "occurrence_state": None,
        "runs": [],
        "terminal": False,
        "policy_ok": False,
        "execution_ok": False,
    }


def natural_overlap_observation(
    snapshot: Dict[str, Any],
    ids: Dict[str, str],
    after: dt.datetime,
) -> Dict[str, Dict[str, Any]]:
    """Find one naturally scheduled instance of each overlap policy.

    The probe must not manufacture extra slots: injecting run-now work into a
    lane whose three definitions already occupy the runtime's admission
    capacity turns a policy observation into a capacity failure. A natural
    instance proves the same contract without changing the product's load.
    """
    results: Dict[str, Dict[str, Any]] = {}

    skip_runs = _runs_for(snapshot, ids.get("skip"))
    found = _missing_observation("no skipped slot due inside an active run after the probe")
    for occurrence in sorted(
        _occurrences_for(snapshot, ids.get("skip")),
        key=lambda row: row.get("scheduled_for") or "",
    ):
        due_raw = occurrence.get("scheduled_for")
        if not due_raw or parse_iso(due_raw) < after:
            continue
        skipped = [run for run in occurrence.get("runs", []) if run.get("state") == "skipped"]
        if occurrence.get("state") != "skipped" or len(skipped) != 1:
            continue
        covering = _covering_run(skip_runs, parse_iso(due_raw))
        if covering is None:
            continue
        found = {
            "missing": False,
            "occurrence": occurrence.get("id"),
            "occurrence_state": occurrence.get("state"),
            "scheduled_for": due_raw,
            "covering_run": {"id": covering.get("id"), "finished_at": covering.get("finished_at")},
            "terminal": skipped[0].get("state") in TERMINAL_RUN_STATES,
            "policy_ok": True,
            "execution_ok": True,
            "runs": [
                {
                    "id": run.get("id"),
                    "state": run.get("state"),
                    "session_id": run.get("session_id"),
                    "started_at": run.get("started_at"),
                    "finished_at": run.get("finished_at"),
                    "error": redact(str(run.get("error"))) if run.get("error") else None,
                }
                for run in skipped
            ],
        }
        break
    results["skip"] = found

    queue_runs = _runs_for(snapshot, ids.get("queue"))
    found = _missing_observation("no queued run started after its blocking run finished")
    candidates = [
        (run, occurrence)
        for occurrence in _occurrences_for(snapshot, ids.get("queue"))
        for run in occurrence.get("runs", [])
        if run.get("started_at") and parse_iso(run["started_at"]) >= after
    ]
    for run, occurrence in sorted(candidates, key=lambda pair: pair[0].get("started_at") or ""):
        due_raw = occurrence.get("scheduled_for")
        if not due_raw or not run.get("finished_at") or run.get("state") not in TERMINAL_RUN_STATES:
            continue
        due = parse_iso(due_raw)
        covering = _covering_run([row for row in queue_runs if row.get("id") != run.get("id")], due)
        if covering is None:
            continue
        policy_ok = bool(
            run.get("state") == "succeeded"
            and parse_iso(run["started_at"]) >= parse_iso(covering["finished_at"])
        )
        found = {
            "missing": False,
            "occurrence": occurrence.get("id"),
            "occurrence_state": occurrence.get("state"),
            "scheduled_for": due_raw,
            "covering_run": {"id": covering.get("id"), "finished_at": covering.get("finished_at")},
            "terminal": True,
            "policy_ok": policy_ok,
            "execution_ok": run.get("state") == "succeeded",
            "runs": [
                {
                    "id": run.get("id"),
                    "state": run.get("state"),
                    "session_id": run.get("session_id"),
                    "started_at": run.get("started_at"),
                    "finished_at": run.get("finished_at"),
                    "error": redact(str(run.get("error"))) if run.get("error") else None,
                }
            ],
        }
        break
    results["queue"] = found

    allow_runs = [
        run
        for run in _runs_for(snapshot, ids.get("allow"))
        if run.get("started_at") and run.get("finished_at")
    ]
    allow_runs.sort(key=lambda run: run.get("started_at") or "")
    found = _missing_observation("no second allow-concurrent run started while the first was active")
    for second in allow_runs:
        if parse_iso(second["started_at"]) < after:
            continue
        first = next(
            (
                run
                for run in allow_runs
                if run.get("id") != second.get("id")
                and parse_iso(run["started_at"]) <= parse_iso(second["started_at"]) < parse_iso(run["finished_at"])
            ),
            None,
        )
        if first is None:
            continue
        found = {
            "missing": False,
            "occurrence": second.get("occurrence_id"),
            "occurrence_state": None,
            "scheduled_for": None,
            "covering_run": {"id": first.get("id"), "finished_at": first.get("finished_at")},
            "terminal": second.get("state") in TERMINAL_RUN_STATES,
            "policy_ok": second.get("state") == "succeeded",
            "execution_ok": second.get("state") == "succeeded",
            "runs": [
                {
                    "id": run.get("id"),
                    "state": run.get("state"),
                    "session_id": run.get("session_id"),
                    "started_at": run.get("started_at"),
                    "finished_at": run.get("finished_at"),
                    "error": redact(str(run.get("error"))) if run.get("error") else None,
                }
                for run in (first, second)
            ],
        }
        break
    results["allow"] = found
    return results


def fault_recovery_facts(
    snapshot: Dict[str, Any],
    fault_id: str,
    killed_at: dt.datetime,
    active_run_ids: Sequence[str] = (),
) -> Dict[str, Any]:
    """What happened to the run that was in flight when the dispatcher died.

    A restarted dispatcher observes the runtime's terminal record and settles
    the run as succeeded. That is the healthy recovery path and must not be
    required to produce an `unknown`. `unknown` is also terminal and accepted:
    it is the runtime's honest answer when the outcome was lost; the point is
    that the same occurrence is never re-delivered and a later occurrence
    starts fresh.
    """
    runs = _runs_for(snapshot, fault_id)
    by_id = {run.get("id"): run for run in runs}
    affected = [by_id[run_id] for run_id in active_run_ids if run_id in by_id]
    if not affected:
        affected = [
            run
            for run in runs
            if run.get("delivered_at")
            and run.get("started_at")
            and parse_iso(run["delivered_at"]) <= killed_at
            and parse_iso(run["started_at"]) <= killed_at + dt.timedelta(seconds=5)
        ]
    stuck = [run.get("id") for run in affected if run.get("state") not in TERMINAL_RUN_STATES]
    unknown = [run.get("id") for run in affected if run.get("state") == "unknown"]
    recovered = [run.get("id") for run in affected if run.get("state") == "succeeded"]
    later_success = [
        run
        for run in runs
        if run.get("state") == "succeeded"
        and run.get("started_at")
        and parse_iso(run["started_at"]) > killed_at
    ]
    by_occurrence: Dict[str, List[str]] = {}
    for run in runs:
        occurrence_id = run.get("occurrence_id")
        if occurrence_id:
            by_occurrence.setdefault(str(occurrence_id), []).append(str(run.get("id")))
    duplicate_occurrences = sorted(
        occurrence_id for occurrence_id, ids in by_occurrence.items() if len(ids) > 1
    )
    passed = bool(affected) and not stuck and bool(later_success) and not duplicate_occurrences
    return {
        "affected": [run.get("id") for run in affected],
        "recovered": recovered,
        "unknown": unknown,
        "stuck": stuck,
        "later_success": len(later_success),
        "duplicate_occurrences": duplicate_occurrences,
        "passed": passed,
    }


def misfire_facts(
    snapshot: Dict[str, Any],
    ids: Dict[str, str],
    off_at: dt.datetime,
    on_at: dt.datetime,
) -> Dict[str, Any]:
    """The offline window's own outcome, not the next natural slot.

    Only an occurrence that became due inside the offline window and started
    after the lane came back proves `run-once` recovery. The next natural slot
    belongs to normal steady state and may still be running when the check
    looks; counting it made a healthy recovery fail.
    """
    skip_occurrences = _occurrences_for(snapshot, ids.get("skip"))
    skip_missed = [
        occurrence
        for occurrence in skip_occurrences
        if occurrence.get("state") == "missed"
        and occurrence.get("scheduled_for")
        and parse_iso(occurrence["scheduled_for"]) <= on_at
    ]
    recovered: List[Dict[str, Any]] = []
    for occurrence in _occurrences_for(snapshot, ids.get("runonce")):
        due_raw = occurrence.get("scheduled_for")
        if not due_raw:
            continue
        due = parse_iso(due_raw)
        if not (off_at < due <= on_at):
            continue
        for run in occurrence.get("runs", []):
            if run.get("state") not in TERMINAL_RUN_STATES:
                continue
            if not run.get("started_at"):
                continue
            if parse_iso(run["started_at"]) >= on_at - dt.timedelta(seconds=5):
                recovered.append(run)
    duplicates = sorted(
        occurrence.get("id")
        for occurrence in skip_occurrences
        if int(occurrence.get("run_count") or len(occurrence.get("runs") or [])) > 1
    )
    passed = (
        bool(skip_missed)
        and bool(recovered)
        and not duplicates
        and all(run.get("state") in ("succeeded", "unknown", "failed") for run in recovered)
    )
    return {
        "skip_missed": len(skip_missed),
        "recovered": recovered,
        "duplicates": duplicates,
        "passed": passed,
    }


def product_shell_failures(failures: Sequence[Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Shell and native failures that must move the verdict.

    Agent failures stay on S2. Fault-lane rows are the injected kill. Runner
    exceptions carry `source: runner` and are reported apart from the product.
    """
    return [
        row
        for row in failures
        if row.get("lane") not in ("fault", "agent")
        and row.get("source") != "runner"
        and row.get("state") in PRODUCT_SHELL_FAILURE_STATES
    ]


def effect_mismatches(evidence: Dict[str, Any]) -> List[Dict[str, Any]]:
    """Shell specs whose recorded success count disagrees with the marker."""
    found: List[Dict[str, Any]] = []
    for lane_name, lane in (evidence.get("lanes") or {}).items():
        for key, effect in (lane.get("effects") or {}).items():
            if not isinstance(effect, dict) or "effect_matches_success" not in effect:
                continue
            if effect.get("effect_matches_success") is not True:
                found.append({"lane": lane_name, "key": key, "effect": effect})
    return found


def duplicate_side_effect_count(evidence: Dict[str, Any], occurrence_duplicates: Sequence[Dict[str, Any]]) -> int:
    """Occurrence fan-out plus marker lines beyond the succeeded-run allowance.

    `effect_matches_success` is the single authority for how many marker lines
    a lane may have: the fault lane may append one line for an unknown run
    whose outcome was lost. Counting those allowed lines again here made a
    healthy fault lane fail the frozen duplicate threshold.
    """
    extra = len(occurrence_duplicates)
    for lane in (evidence.get("lanes") or {}).values():
        for effect in (lane.get("effects") or {}).values():
            if not isinstance(effect, dict):
                continue
            lines = effect.get("lines")
            succeeded = effect.get("succeeded_runs")
            if lines is None or succeeded is None:
                continue
            if effect.get("effect_matches_success") is True:
                continue
            if int(lines) > int(succeeded):
                extra += int(lines) - int(succeeded)
    return extra


def silent_losses(evidence: Dict[str, Any]) -> List[Dict[str, Any]]:
    """Classified occurrences that never gained a run row.

    A missed slot is the misfire policy's occurrence-level record and has no
    run on purpose. A skip, interrupt, block, cancel, or approval wait without
    a run is a swallowed trigger.
    """
    return [
        row
        for row in (evidence.get("classified_without_run") or [])
        if row.get("state") in SILENT_LOSS_STATES
    ]


def max_observed_wake_gap(evidence: Dict[str, Any]) -> Optional[float]:
    gaps: List[float] = []
    for lane in (evidence.get("lanes") or {}).values():
        gap = (lane.get("wakeups") or {}).get("max_gap_secs")
        if gap is not None:
            gaps.append(float(gap))
    return max(gaps) if gaps else None


def agent_success_ratio(evidence: Dict[str, Any]) -> Optional[float]:
    """Succeeded agent runs divided by terminal agent runs. None if none finished."""
    states = ((evidence.get("lanes") or {}).get("agent") or {}).get("run_states") or {}
    terminal_names = (
        "succeeded",
        "failed",
        "unknown",
        "blocked",
        "waiting_approval",
        "cancelled",
        "skipped",
    )
    terminal = sum(int(states.get(name, 0) or 0) for name in terminal_names)
    if terminal <= 0:
        return None
    return int(states.get("succeeded", 0) or 0) / float(terminal)


def residue_from_cleanup(cleanup: Dict[str, Any]) -> Dict[str, Any]:
    """Accept the cleanup_run dict or a `{"residue": ...}` wrapper.

    finalize passes the dict cleanup_run returns. That dict has `processes`
    and `labels` at the top level. Reading only `cleanup["residue"]` made the
    orphan thresholds pass on an empty lookup.
    """
    if not isinstance(cleanup, dict):
        return {}
    nested = cleanup.get("residue")
    if isinstance(nested, dict):
        return nested
    return cleanup


def record_runner_error(state: Dict[str, Any], name: str, error: str) -> Dict[str, Any]:
    entry = {"name": name, "error": error, "source": "runner"}
    state.setdefault("runner_errors", []).append(entry)
    return entry


def percentile(values: Sequence[float], fraction: float) -> Optional[float]:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(math.ceil(fraction * len(ordered))) - 1))
    return ordered[index]


def latency_stats(values: Sequence[float]) -> Dict[str, Any]:
    if not values:
        return {"count": 0, "p50": None, "p95": None, "p99": None, "max": None}
    return {
        "count": len(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values),
    }


def assert_isolated_home(home: Path, live_root: Path) -> None:
    resolved_home = home.resolve()
    resolved_root = live_root.resolve()
    user_home = Path.home().resolve() / ".leveler"
    if resolved_home == user_home or user_home in resolved_home.parents:
        raise SoakError("refusing to use the user's real Leveler home: %s" % resolved_home)
    if resolved_root not in resolved_home.parents:
        raise SoakError("home %s is not under the soak root %s" % (resolved_home, resolved_root))
    if (
        not str(resolved_root).startswith("/tmp")
        and not str(resolved_root).startswith("/private/tmp")
        and os.environ.get("LEVELER_SOAK_ALLOW_ANY_ROOT") != "1"
    ):
        raise SoakError("soak root must be under /tmp (got %s)" % resolved_root)


def choose_live_root() -> Path:
    token = uuid.uuid4().hex[:10]
    root = Path("/tmp") / ("lvs-" + token)
    root.mkdir(parents=True, exist_ok=False)
    return root.resolve()


def binary_identity(binary: Path) -> Dict[str, Any]:
    if not binary.is_file():
        raise SoakError("binary does not exist: %s" % binary)
    if not os.access(binary, os.X_OK):
        raise SoakError("binary is not executable: %s" % binary)
    result = run_command([str(binary), "--version"], timeout=30)
    version = (result.stdout or result.stderr).strip()
    return {
        "path": str(binary.resolve()),
        "sha256": sha256_file(binary),
        "version": version,
        "clean": ("-dirty" not in version and "UNTRUSTED" not in version),
        "revision": version_revision(version),
    }


def version_revision(text: str) -> Optional[str]:
    match = re.search(r"\(([0-9a-f]{7,40})(?:-dirty)?\)", text)
    return match.group(1) if match else None


def config_info() -> Dict[str, Any]:
    path = Path.home() / ".leveler" / "config.toml"
    if not path.is_file():
        return {"path": str(path), "exists": False, "default_model": None, "inline_key_present": False}
    text = path.read_text(encoding="utf-8", errors="replace")
    model = None
    for line in text.splitlines():
        match = re.match(r'^default_model\s*=\s*"([^"]+)"', line.strip())
        if match:
            model = match.group(1)
            break
    # Presence only. The value is never logged or copied into evidence.
    inline = bool(re.search(r"(?m)^\s*api_key\s*=\s*\S+", text))
    env_names = sorted(set(re.findall(r'api_key_env\s*=\s*"([^"]+)"', text)))
    env_available = {name: bool(os.environ.get(name)) for name in env_names}
    return {
        "path": str(path),
        "exists": True,
        "default_model": model,
        "inline_key_present": inline,
        "env_key_names": env_names,
        "env_key_available": env_available,
    }


def copy_config(home: Path) -> None:
    source = Path.home() / ".leveler" / "config.toml"
    if not source.is_file():
        return
    home.mkdir(parents=True, exist_ok=True)
    target = home / "config.toml"
    shutil.copyfile(source, target)
    os.chmod(target, 0o600)


def cli_environment(home: Path, idle_secs: int = 120) -> Dict[str, str]:
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
        "HOME": str(Path.home()),
        "TMPDIR": tempfile.gettempdir(),
        "LANG": os.environ.get("LANG", "en_US.UTF-8"),
        "LEVELER_HOME": str(home),
        "LEVELER_CONFIG_DIR": str(home),
        "LEVELER_DAEMON_IDLE_TIMEOUT_SECS": str(int(idle_secs)),
    }
    # Provider credentials may live in the environment instead of config.toml.
    # Forward only key-shaped variables; never record or print them.
    for key, value in os.environ.items():
        if key.startswith("LEVELER_") and key.endswith("_API_KEY"):
            env[key] = value
        elif key in ("DEEPSEEK_API_KEY", "BIGMODEL_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"):
            env[key] = value
    return env


def make_fixture_repo(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    (repo / ".leveler").mkdir(parents=True, exist_ok=True)
    (repo / ".leveler" / "config.yaml").write_text("{}\n", encoding="utf-8")
    (repo / "facts.txt").write_text("VALUE=421\nSOURCE=isolated-soak-fixture\n", encoding="utf-8")
    (repo / "README.md").write_text(
        "# Isolated soak fixture\n\nThis repository exists only for Phase 6.4 scheduled automation.\n",
        encoding="utf-8",
    )
    (repo / "src").mkdir(exist_ok=True)
    (repo / "src" / "main.rs").write_text(
        'fn main() {\n    println!("soak fixture");\n}\n', encoding="utf-8"
    )
    (repo / "effects").mkdir(exist_ok=True)
    if not (repo / ".git").exists():
        run_command(["git", "init", "-q"], cwd=repo, timeout=30)
    run_command(
        ["git", "-c", "user.email=soak@example.invalid", "-c", "user.name=soak", "add", "."],
        cwd=repo,
        timeout=30,
    )
    run_command(
        [
            "git",
            "-c",
            "user.email=soak@example.invalid",
            "-c",
            "user.name=soak",
            "commit",
            "-q",
            "-m",
            "soak fixture seed",
        ],
        cwd=repo,
        timeout=30,
    )


@dataclasses.dataclass
class RunPaths:
    evidence: Path
    live_root: Optional[Path]
    plan: Path
    state: Path
    heartbeat: Path
    events: Path
    runs: Path
    samples: Path
    log: Path
    report_json: Path
    report_md: Path
    control: Path

    @classmethod
    def for_evidence(cls, evidence: Path, live_root: Optional[Path]) -> "RunPaths":
        return cls(
            evidence=evidence,
            live_root=live_root,
            plan=evidence / "plan.json",
            state=evidence / "state.json",
            heartbeat=evidence / "heartbeat.json",
            events=evidence / "events.jsonl",
            runs=evidence / "runs.jsonl",
            samples=evidence / "samples.jsonl",
            log=evidence / "runner.log",
            report_json=evidence / "report.json",
            report_md=evidence / "report.md",
            control=evidence / "control.json",
        )


class Recorder:
    def __init__(self, paths: RunPaths):
        self.paths = paths
        self.paths.evidence.mkdir(parents=True, exist_ok=True)

    def event(self, kind: str, **fields: Any) -> Dict[str, Any]:
        row = {"ts": iso(), "kind": kind}
        row.update(fields)
        append_jsonl(self.paths.events, row)
        return row

    def log(self, message: str) -> None:
        append_text(self.paths.log, "[%s] %s" % (iso(), redact(message)))

    def state(self, value: Dict[str, Any]) -> None:
        atomic_write_json(self.paths.state, value)

    def heartbeat(self, value: Dict[str, Any]) -> None:
        atomic_write_json(self.paths.heartbeat, value)


@dataclasses.dataclass
class DefSpec:
    key: str
    name: str
    kind: str
    trigger: List[str]
    command: Optional[str] = None
    goal: Optional[str] = None
    overlap: str = "skip-if-running"
    misfire: str = "run-once"
    grace: int = 180
    timeout: int = 120
    session_policy: str = "continue"
    model: Optional[str] = None
    permission: str = "assisted"
    marker: Optional[str] = None

    def plan_row(self) -> Dict[str, Any]:
        return {
            "key": self.key,
            "name": self.name,
            "kind": self.kind,
            "trigger": self.trigger,
            "command": self.command,
            "goal": self.goal,
            "overlap": self.overlap,
            "misfire": self.misfire,
            "grace_secs": self.grace,
            "timeout_secs": self.timeout,
            "session_policy": self.session_policy,
            "model": self.model,
            "permission": self.permission,
            "marker": self.marker,
        }


def lane_paths(paths: RunPaths, name: str) -> Dict[str, Path]:
    if paths.live_root is None:
        raise SoakError("this run has no live root")
    return {
        "home": paths.live_root / "homes" / name,
        "repo": paths.live_root / "repos" / name,
        "binary": paths.live_root / "bin" / name / "leveler",
        "lane_evidence": paths.evidence / "lanes" / name,
        "log": paths.evidence / "lanes" / name / "process.log",
    }


def lane_cli_env(lane: Dict[str, Any]) -> Dict[str, str]:
    return cli_environment(Path(lane["home"]), int(lane.get("idle_secs", 120)))


def lane_cli(
    lane: Dict[str, Any],
    args: Sequence[str],
    timeout: float = 120.0,
    check: bool = True,
    cwd: Optional[Path] = None,
) -> subprocess.CompletedProcess:
    home = Path(lane["home"])
    repo = Path(lane["repo"])
    command = [str(lane["binary"]), *args, "--home", str(home), "--config-dir", str(home)]
    if "--repo" in args:
        pass
    elif args and args[0] == "schedule" and (len(args) < 2 or args[1] not in ("serve", "dispatch")):
        command.extend(["--repo", str(repo)])
    result = run_command(command, env=lane_cli_env(lane), cwd=cwd, timeout=timeout)
    if check and result.returncode != 0:
        raise SoakError(
            "`%s` failed (%d): %s"
            % (" ".join(command), result.returncode, redact((result.stderr or result.stdout).strip()[:1200]))
        )
    return result


def lane_json(lane: Dict[str, Any], args: Sequence[str], timeout: float = 120.0) -> Any:
    result = lane_cli(lane, list(args) + ["--json"], timeout=timeout)
    text = (result.stdout or "").strip()
    try:
        return json.loads(text)
    except ValueError as error:
        raise SoakError(
            "`%s` returned non-JSON output %r" % (" ".join(args), text[:400])
        ) from error


def prepare_lane(paths: RunPaths, name: str, idle_secs: int = 120) -> Dict[str, Any]:
    locations = lane_paths(paths, name)
    home = locations["home"]
    repo = locations["repo"]
    binary = locations["binary"]
    assert_isolated_home(home, paths.live_root)  # type: ignore[arg-type]
    home.mkdir(parents=True, exist_ok=True)
    binary.parent.mkdir(parents=True, exist_ok=True)
    copy_config(home)
    make_fixture_repo(repo)
    return {
        "name": name,
        "home": str(home),
        "repo": str(repo),
        "binary": str(binary),
        "idle_secs": idle_secs,
        "defs": {},
        "kind": "launchd",
        "serve_pid": None,
        "label": label_for(home),
        "log": str(locations["log"]),
    }


def install_lane_binary(plan_binary: Path, lane: Dict[str, Any]) -> Dict[str, Any]:
    target = Path(lane["binary"])
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(plan_binary, target)
    os.chmod(target, 0o755)
    return binary_identity(target)


def ensure_definition(lane: Dict[str, Any], spec: DefSpec, recorder: Recorder) -> str:
    rows = lane_json(lane, ["schedule", "list"])
    existing = [row for row in rows if row.get("name") == spec.name]
    if len(existing) > 1:
        raise SoakError("duplicate definition name %s in %s" % (spec.name, lane["name"]))
    if existing:
        row = existing[0]
        action = row.get("action") or {}
        if action.get("kind") != spec.kind:
            raise SoakError(
                "definition %s exists with a different action kind (%s)"
                % (spec.name, action.get("kind"))
            )
        if spec.kind == "shell" and action.get("command") != spec.command:
            raise SoakError("definition %s exists with a different command" % spec.name)
        if spec.kind == "agent" and action.get("goal") != spec.goal:
            raise SoakError("definition %s exists with a different goal" % spec.name)
        lane["defs"][spec.key] = {"id": row["id"], "name": spec.name, "spec": spec.plan_row()}
        recorder.event("definition_reused", lane=lane["name"], key=spec.key, definition=row["id"])
        return str(row["id"])

    args: List[str] = [
        "schedule",
        "add",
        "--name",
        spec.name,
        "--workspace",
        str(lane["repo"]),
        "--overlap",
        spec.overlap,
        "--misfire",
        spec.misfire,
        "--misfire-grace",
        str(spec.grace),
        "--timeout",
        str(spec.timeout),
        "--permission",
        spec.permission,
        "--grant-unattended",
        "--allow-external-side-effects",
    ]
    args.extend(spec.trigger)
    if spec.kind == "shell":
        args.extend(["--command", spec.command or "", "--session-policy", spec.session_policy])
    elif spec.kind == "agent":
        args.extend(["--agent-goal", spec.goal or "", "--session-policy", spec.session_policy])
        if spec.model:
            args.extend(["--model", spec.model])
    else:
        raise SoakError("unknown action kind %s" % spec.kind)
    created = lane_json(lane, args, timeout=120)
    lane["defs"][spec.key] = {"id": created["id"], "name": spec.name, "spec": spec.plan_row()}
    recorder.event(
        "definition_created",
        lane=lane["name"],
        key=spec.key,
        definition=created["id"],
        next_occurrence=created.get("next_occurrence"),
        revision=created.get("revision"),
    )
    return str(created["id"])


def lane_specs(mode: str, model: Optional[str]) -> Dict[str, List[DefSpec]]:
    """The actual scenario matrix. Times are anchors relative to start."""
    if mode == "dry-run":
        os_specs = [
            DefSpec(
                "tick",
                "soak-os-tick",
                "shell",
                ["--every", "60"],
                command="mkdir -p effects && printf '%s\\n' \"$(date -u +%s)\" >> effects/tick.log",
                grace=180,
                timeout=120,
                marker="effects/tick.log",
            ),
            DefSpec(
                "once",
                "soak-os-once",
                "shell",
                ["--at", "@@START+45@@"],
                command="printf 'once\\n' >> effects/once.log",
                misfire="run-once",
                grace=600,
                timeout=120,
                marker="effects/once.log",
            ),
            DefSpec(
                "pause",
                "soak-os-pause",
                "shell",
                ["--every", "60"],
                command="printf '%s\\n' \"$(date -u +%s)\" >> effects/pause.log",
                grace=180,
                timeout=120,
                marker="effects/pause.log",
            ),
        ]
        overlap_specs = [
            DefSpec(
                "skip",
                "soak-overlap-skip",
                "shell",
                ["--every", "60"],
                command="mkdir -p effects && sleep 75 && printf '%s\\n' \"$(date -u +%s)\" >> effects/skip.log",
                overlap="skip-if-running",
                grace=180,
                timeout=180,
                marker="effects/skip.log",
            ),
            DefSpec(
                "queue",
                "soak-overlap-queue",
                "shell",
                ["--every", "60"],
                command="mkdir -p effects && sleep 70 && printf '%s\\n' \"$(date -u +%s)\" >> effects/queue.log",
                overlap="queue-one",
                grace=180,
                timeout=180,
                marker="effects/queue.log",
            ),
            DefSpec(
                "allow",
                "soak-overlap-allow",
                "shell",
                ["--every", "60"],
                command="mkdir -p effects && sleep 70 && printf '%s\\n' \"$(date -u +%s)\" >> effects/allow.log",
                overlap="allow-concurrent",
                # Overlapping runs need one execution context each; a shared
                # session admits one turn at a time.
                session_policy="fresh",
                grace=180,
                timeout=180,
                marker="effects/allow.log",
            ),
        ]
        agent_specs = [
            DefSpec(
                "agent_short",
                "soak-agent-short",
                "agent",
                ["--at", "@@START+60@@"],
                goal=(
                    "Read facts.txt in the repository root. Create agent-report.md whose first "
                    "line is exactly the VALUE line from facts.txt and whose second line is "
                    "SOAK=agent-short. Do not modify any other file."
                ),
                session_policy="fresh",
                timeout=300,
                model=model,
                permission="full-access",
                marker="agent-report.md",
            )
        ]
        fault_specs = [
            DefSpec(
                "fault",
                "soak-fault-run",
                "shell",
                ["--every", "60"],
                command="mkdir -p effects && sleep 30 && printf '%s\\n' \"$(date -u +%s)\" >> effects/fault.log",
                grace=180,
                timeout=120,
                marker="effects/fault.log",
            )
        ]
        misfire_specs = [
            DefSpec(
                "skip",
                "soak-misfire-skip",
                "shell",
                ["--every", "60"],
                command="printf '%s\\n' \"$(date -u +%s)\" >> effects/misfire-skip.log",
                misfire="skip",
                grace=60,
                timeout=120,
                marker="effects/misfire-skip.log",
            ),
            DefSpec(
                "runonce",
                "soak-misfire-runonce",
                "shell",
                ["--every", "60"],
                command="printf '%s\\n' \"$(date -u +%s)\" >> effects/misfire-runonce.log",
                misfire="run-once",
                grace=60,
                timeout=120,
                marker="effects/misfire-runonce.log",
            ),
        ]
        upgrade_specs = [
            DefSpec(
                "upgrade",
                "soak-upgrade-tick",
                "shell",
                ["--every", "60"],
                command="printf '%s\\n' \"$(date -u +%s)\" >> effects/upgrade.log",
                grace=180,
                timeout=120,
                marker="effects/upgrade.log",
            )
        ]
        s11_specs = [
            DefSpec(
                "once",
                "soak-s11-once",
                "shell",
                ["--at", "@@START+45@@"],
                command="printf 's11\\n' >> effects/s11.log",
                misfire="run-once",
                grace=600,
                timeout=120,
                marker="effects/s11.log",
            )
        ]
        return {
            "os": os_specs,
            "overlap": overlap_specs,
            "agent": agent_specs,
            "fault": fault_specs,
            "misfire": misfire_specs,
            "upgrade": upgrade_specs,
            "s11": s11_specs,
        }

    # 24h load plan. Agent frequency is deliberately much lower than shell.
    os_specs = [
        DefSpec(
            "tick",
            "soak-os-tick",
            "shell",
            ["--every", "60"],
            command="mkdir -p effects && printf '%s\\n' \"$(date -u +%s)\" >> effects/tick.log",
            grace=180,
            timeout=120,
            marker="effects/tick.log",
        ),
        DefSpec(
            "once",
            "soak-os-once",
            "shell",
            ["--at", "@@START+180@@"],
            command="printf 'once\\n' >> effects/once.log",
            misfire="run-once",
            grace=600,
            timeout=120,
            marker="effects/once.log",
        ),
        DefSpec(
            "pause",
            "soak-os-pause",
            "shell",
            ["--every", "60"],
            command="printf '%s\\n' \"$(date -u +%s)\" >> effects/pause.log",
            grace=180,
            timeout=120,
            marker="effects/pause.log",
        ),
    ]
    overlap_specs = [
        DefSpec(
            "skip",
            "soak-overlap-skip",
            "shell",
            ["--every", "60"],
            command="mkdir -p effects && sleep 75 && printf '%s\\n' \"$(date -u +%s)\" >> effects/skip.log",
            overlap="skip-if-running",
            grace=180,
            timeout=180,
            marker="effects/skip.log",
        ),
        DefSpec(
            "queue",
            "soak-overlap-queue",
            "shell",
            ["--every", "60"],
            command="mkdir -p effects && sleep 70 && printf '%s\\n' \"$(date -u +%s)\" >> effects/queue.log",
            overlap="queue-one",
            grace=180,
            timeout=180,
            marker="effects/queue.log",
        ),
        DefSpec(
            "allow",
            "soak-overlap-allow",
            "shell",
            ["--every", "60"],
            command="mkdir -p effects && sleep 70 && printf '%s\\n' \"$(date -u +%s)\" >> effects/allow.log",
            overlap="allow-concurrent",
            # Overlapping runs need one execution context each; a shared
            # session admits one turn at a time.
            session_policy="fresh",
            grace=180,
            timeout=180,
            marker="effects/allow.log",
        ),
    ]
    agent_specs = [
        DefSpec(
            "agent_continue",
            "soak-agent-continue",
            "agent",
            ["--every", "21600"],
            goal=(
                "Read facts.txt in the repository root. Append one line to agent-history.md: "
                "the VALUE line followed by RUN=<current UTC date and time>. Create the file if "
                "it does not exist. Do not modify any other file."
            ),
            session_policy="continue",
            timeout=300,
            model=model,
            permission="full-access",
            marker="agent-history.md",
        ),
        DefSpec(
            "agent_fresh",
            "soak-agent-fresh",
            "agent",
            ["--at", "@@START+5400@@"],
            goal=(
                "Read facts.txt in the repository root. Create agent-fresh.md containing the "
                "VALUE line and SOAK=agent-fresh. Do not modify any other file."
            ),
            session_policy="fresh",
            timeout=300,
            model=model,
            permission="full-access",
            marker="agent-fresh.md",
        ),
    ]
    fault_specs = [
        DefSpec(
            "fault",
            "soak-fault-run",
            "shell",
            ["--every", "60"],
            command="mkdir -p effects && sleep 30 && printf '%s\\n' \"$(date -u +%s)\" >> effects/fault.log",
            grace=180,
            timeout=120,
            marker="effects/fault.log",
        )
    ]
    misfire_specs = [
        DefSpec(
            "skip",
            "soak-misfire-skip",
            "shell",
            ["--every", "60"],
            command="printf '%s\\n' \"$(date -u +%s)\" >> effects/misfire-skip.log",
            misfire="skip",
            grace=60,
            timeout=120,
            marker="effects/misfire-skip.log",
        ),
        DefSpec(
            "runonce",
            "soak-misfire-runonce",
            "shell",
            ["--every", "60"],
            command="printf '%s\\n' \"$(date -u +%s)\" >> effects/misfire-runonce.log",
            misfire="run-once",
            grace=60,
            timeout=120,
            marker="effects/misfire-runonce.log",
        ),
    ]
    upgrade_specs = [
        DefSpec(
            "upgrade",
            "soak-upgrade-tick",
            "shell",
            ["--every", "60"],
            command="printf '%s\\n' \"$(date -u +%s)\" >> effects/upgrade.log",
            grace=180,
            timeout=120,
            marker="effects/upgrade.log",
        )
    ]
    s11_specs = [
        DefSpec(
            "once",
            "soak-s11-once",
            "shell",
            ["--at", "@@START+120@@"],
            command="printf 's11\\n' >> effects/s11.log",
            misfire="run-once",
            grace=600,
            timeout=120,
            marker="effects/s11.log",
        )
    ]
    return {
        "os": os_specs,
        "overlap": overlap_specs,
        "agent": agent_specs,
        "fault": fault_specs,
        "misfire": misfire_specs,
        "upgrade": upgrade_specs,
        "s11": s11_specs,
    }


def anchor_trigger(trigger: List[str], start_at: dt.datetime) -> List[str]:
    resolved: List[str] = []
    for item in trigger:
        if item.startswith("@@START+"):
            offset = int(item.split("+", 1)[1].rstrip("@"))
            resolved.append(iso(start_at + dt.timedelta(seconds=offset)))
        else:
            resolved.append(item)
    return resolved


def schedule_database(home: Path) -> Path:
    return home / "state" / "schedules.db"


def _sqlite_ro(path: Path) -> sqlite3.Connection:
    # A mode=ro connection cannot create the -shm file a WAL-mode database
    # needs after a clean checkpoint, so SQLite reports "unable to open
    # database file" even though the store is readable. Open normally and
    # enforce read-only access with query_only instead.
    connection = sqlite3.connect(str(path), timeout=10.0)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA busy_timeout = 10000")
    connection.execute("PRAGMA query_only = ON")
    return connection


def _table_rows(connection: sqlite3.Connection, table: str) -> List[Dict[str, Any]]:
    try:
        cursor = connection.execute("SELECT * FROM %s" % table)
    except sqlite3.OperationalError as error:
        if "no such table" in str(error):
            return []
        raise
    return [dict(row) for row in cursor.fetchall()]


def read_schedule(home: Path) -> Dict[str, Any]:
    path = schedule_database(home)
    empty = {"database": str(path), "exists": path.is_file(), "definitions": [], "occurrences": [], "runs": [], "wakeups": []}
    if not path.is_file():
        return empty
    last_error: Optional[Exception] = None
    for attempt in range(4):
        try:
            with contextlib.closing(_sqlite_ro(path)) as connection:
                definitions = _table_rows(connection, "schedule_definitions")
                occurrences = _table_rows(connection, "schedule_occurrences")
                runs = _table_rows(connection, "schedule_runs")
                wakeups = _table_rows(connection, "schedule_wakeups")
            break
        except sqlite3.Error as error:
            last_error = error
            time.sleep(0.5 * (attempt + 1))
    else:
        raise SoakError("could not read %s: %s" % (path, last_error))
    runs_by_occurrence: Dict[str, List[Dict[str, Any]]] = {}
    for run in runs:
        runs_by_occurrence.setdefault(run.get("occurrence_id"), []).append(run)
    for occurrence in occurrences:
        attached = runs_by_occurrence.get(occurrence.get("id"), [])
        occurrence["runs"] = attached
        occurrence["run_count"] = len(attached)
    for definition in definitions:
        definition["occurrence_count"] = sum(
            1 for occurrence in occurrences if occurrence.get("definition_id") == definition.get("id")
        )
    return {
        "database": str(path),
        "exists": True,
        "definitions": definitions,
        "occurrences": occurrences,
        "runs": runs,
        "wakeups": wakeups,
    }


def find_session_databases(home: Path) -> List[Path]:
    return sorted(Path(p) for p in glob.glob(str(home / "state" / "projects" / "*" / "sessions.db")))


def read_agent_sessions(home: Path, repo: Path) -> Dict[str, Any]:
    rows: List[Dict[str, Any]] = []
    for path in find_session_databases(home):
        try:
            with contextlib.closing(_sqlite_ro(path)) as connection:
                sessions = _table_rows(connection, "sessions")
                for session in sessions:
                    if session.get("repository") != str(repo):
                        continue
                    session_id = session.get("id")
                    events = [
                        row
                        for row in _table_rows(connection, "events")
                        if row.get("session_id") == session_id
                    ]
                    turns = [
                        row
                        for row in _table_rows(connection, "turns")
                        if row.get("session_id") == session_id
                    ]
                    types: Dict[str, int] = {}
                    for event in events:
                        types[str(event.get("type"))] = types.get(str(event.get("type")), 0) + 1
                    rows.append(
                        {
                            "database": str(path),
                            "session_id": session_id,
                            "status": session.get("status"),
                            "outcome": session.get("outcome"),
                            "goal": session.get("goal"),
                            "model": session.get("model"),
                            "created_at": session.get("created_at"),
                            "updated_at": session.get("updated_at"),
                            "turns": len(turns),
                            "events": len(events),
                            "event_types": types,
                            "tool_calls_started": int(types.get("tool_call_started", 0)),
                            "tool_calls_completed": int(types.get("tool_call_completed", 0)),
                        }
                    )
        except sqlite3.Error as error:
            rows.append({"database": str(path), "error": str(error)})
    return {"sessions": rows}


def start_serve(lane: Dict[str, Any], recorder: Recorder) -> Optional[int]:
    home = Path(lane["home"])
    repo = Path(lane["repo"])
    binary = Path(lane["binary"])
    log_path = Path(lane["log"])
    log_path.parent.mkdir(parents=True, exist_ok=True)
    handle = open(log_path, "a", encoding="utf-8")
    command = [
        str(binary),
        "schedule",
        "serve",
        "--max",
        "2",
        "--poll-secs",
        "1",
        "--home",
        str(home),
        "--config-dir",
        str(home),
        "--repo",
        str(repo),
    ]
    process = subprocess.Popen(
        command,
        env=lane_cli_env(lane),
        cwd=str(repo),
        stdin=subprocess.DEVNULL,
        stdout=handle,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    handle.close()
    lane["serve_pid"] = process.pid
    lane["serve_started_at"] = iso()
    recorder.event("serve_started", lane=lane["name"], pid=process.pid, command=" ".join(command))
    return process.pid


def stop_serve(lane: Dict[str, Any], recorder: Recorder) -> Optional[Dict[str, Any]]:
    pid = lane.get("serve_pid")
    if not pid:
        return None
    command = pid_cmdline(int(pid))
    home = str(lane["home"])
    if not pid_alive(int(pid)):
        lane["serve_pid"] = None
        result = {"pid": pid, "already_dead": True}
        recorder.event("serve_stopped", lane=lane["name"], **result)
        return result
    if home not in command or "serve" not in command:
        recorder.event("serve_stop_refused", lane=lane["name"], pid=pid, command=command[:400])
        raise SoakError("refusing to stop PID %s: command line does not identify this lane" % pid)
    tree = descendants(int(pid))
    os.kill(int(pid), signal.SIGTERM)
    deadline = time.time() + 15
    while time.time() < deadline and pid_alive(int(pid)):
        time.sleep(0.2)
    forced = False
    if pid_alive(int(pid)):
        os.kill(int(pid), signal.SIGKILL)
        forced = True
    for child in reversed(tree):
        if pid_alive(child):
            with contextlib.suppress(ProcessLookupError):
                os.kill(child, signal.SIGTERM)
    lane["serve_pid"] = None
    result = {"pid": pid, "forced": forced, "descendants": tree, "command": command[:400]}
    recorder.event("serve_stopped", lane=lane["name"], **result)
    return result


def enable_lane(lane: Dict[str, Any], recorder: Recorder) -> Dict[str, Any]:
    status = lane_json(lane, ["schedule", "enable"])
    label = lane.get("label") or label_for(Path(lane["home"]))
    lane["label"] = label
    plist = plist_for(label)
    loaded = launchd_loaded(label)
    content = plist.read_text(encoding="utf-8", errors="replace") if plist.is_file() else ""
    row = {
        "lane": lane["name"],
        "label": label,
        "plist": str(plist),
        "plist_exists": plist.is_file(),
        "plist_names_home": str(lane["home"]) in content,
        "loaded": loaded,
        "registered": status.get("registered"),
        "executable": status.get("executable"),
        "arguments": status.get("arguments"),
        "last_exit_code": status.get("last_exit_code"),
    }
    recorder.event("launchd_enabled", **row)
    return row


def disable_lane(lane: Dict[str, Any], recorder: Recorder) -> Dict[str, Any]:
    status = lane_json(lane, ["schedule", "disable"])
    label = lane.get("label") or label_for(Path(lane["home"]))
    plist = plist_for(label)
    row = {
        "lane": lane["name"],
        "label": label,
        "plist": str(plist),
        "plist_exists": plist.is_file(),
        "loaded": launchd_loaded(label),
        "registered": status.get("registered"),
    }
    recorder.event("launchd_disabled", **row)
    return row


def registration_status(lane: Dict[str, Any]) -> Dict[str, Any]:
    label = lane.get("label") or label_for(Path(lane["home"]))
    plist = plist_for(label)
    return {
        "lane": lane["name"],
        "label": label,
        "plist": str(plist),
        "plist_exists": plist.is_file(),
        "loaded": launchd_loaded(label),
    }


def marker_counts(lane: Dict[str, Any], specs: Sequence[DefSpec]) -> Dict[str, Any]:
    repo = Path(lane["repo"])
    result: Dict[str, Any] = {}
    for spec in specs:
        if not spec.marker:
            continue
        path = repo / spec.marker
        if not path.is_file():
            result[spec.key] = {"path": spec.marker, "exists": False, "lines": 0, "bytes": 0}
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        result[spec.key] = {
            "path": spec.marker,
            "exists": True,
            "lines": len([line for line in text.splitlines() if line.strip()]),
            "bytes": path.stat().st_size,
            "value_ok": ("VALUE=421" in text) if spec.kind == "agent" else None,
            "tail": text[-400:],
        }
    return result


def load_seen_runs(paths: RunPaths) -> set:
    seen = set()
    if not paths.runs.is_file():
        return seen
    with open(paths.runs, "r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("run_id"):
                seen.add(str(row["run_id"]))
    return seen


def emit_run_records(
    paths: RunPaths,
    recorder: Recorder,
    lanes: Dict[str, Dict[str, Any]],
    specs_by_lane: Dict[str, List[DefSpec]],
    seen: set,
    plan: Dict[str, Any],
) -> int:
    emitted = 0
    for lane_name, lane in lanes.items():
        specs = {spec.key: spec for spec in specs_by_lane.get(lane_name, [])}
        snapshot = read_schedule(Path(lane["home"]))
        definitions = {d["id"]: d for d in snapshot["definitions"]}
        counts = marker_counts(lane, list(specs.values()))
        for occurrence in snapshot["occurrences"]:
            definition = definitions.get(occurrence.get("definition_id"), {})
            spec = None
            for candidate in specs.values():
                if candidate.name == definition.get("name"):
                    spec = candidate
                    break
            for run in occurrence.get("runs", []):
                run_id = str(run.get("id"))
                if run_id in seen:
                    continue
                seen.add(run_id)
                record = {
                    "ts": iso(),
                    "run_id": run_id,
                    "scenario": lane_name,
                    "schedule_id": occurrence.get("definition_id"),
                    "schedule_name": definition.get("name"),
                    "occurrence_id": occurrence.get("id"),
                    "run_now": bool(occurrence.get("run_now")),
                    "scheduled_for": occurrence.get("scheduled_for"),
                    "claimed_at": run.get("started_at"),
                    "delivered_at": run.get("delivered_at"),
                    "running_at": run.get("running_at"),
                    "handed_off_at": run.get("handed_off_at"),
                    "finished_at": run.get("finished_at"),
                    "terminal_state": run.get("state"),
                    "occurrence_state": occurrence.get("state"),
                    "claim_latency_ms": _latency_ms(occurrence.get("scheduled_for"), run.get("started_at")),
                    "completion_latency_ms": _latency_ms(run.get("started_at"), run.get("finished_at")),
                    "exit_code": run.get("exit_code"),
                    "error": redact(str(run.get("error"))) if run.get("error") else None,
                    "session_id": run.get("session_id"),
                    "task_outcome": run.get("task_outcome"),
                    "provider": run.get("provider"),
                    "usage_requests": run.get("usage_requests"),
                    "input_tokens": run.get("input_tokens"),
                    "cached_input_tokens": run.get("cached_input_tokens"),
                    "output_tokens": run.get("output_tokens"),
                    "reasoning_tokens": run.get("reasoning_tokens"),
                    "cost_usd_micros": run.get("cost_usd_micros"),
                    "usage_rows_without_cost": run.get("usage_rows_without_cost"),
                    "side_effects": counts,
                    "evidence_path": str(paths.runs),
                }
                append_jsonl(paths.runs, record)
                emitted += 1
    if emitted:
        recorder.event("runs_observed", count=emitted)
    return emitted


def _latency_ms(start: Optional[str], end: Optional[str]) -> Optional[int]:
    seconds = seconds_between(start, end)
    return int(round(seconds * 1000)) if seconds is not None else None


def sample_processes(paths: RunPaths, lanes: Dict[str, Dict[str, Any]], recorder: Recorder) -> Dict[str, Any]:
    rows = matching_processes(paths.live_root)  # type: ignore[arg-type]
    runtime_count = 0
    dispatcher_count = 0
    for row in rows:
        command = row["command"]
        if " schedule " in command and " serve" in command:
            dispatcher_count += 1
        elif re.search(r"\bserve\b", command):
            runtime_count += 1
    rss_kb = sum(row["rss_kb"] for row in rows)
    cpu = sum(row["cpu"] for row in rows)
    db_bytes = 0
    for lane in lanes.values():
        database = schedule_database(Path(lane["home"]))
        if database.is_file():
            db_bytes += database.stat().st_size
            with contextlib.suppress(OSError):
                db_bytes += database.with_name(database.name + "-wal").stat().st_size
    root_bytes = 0
    if paths.live_root and paths.live_root.is_dir():
        result = run_command(["du", "-sk", str(paths.live_root)], timeout=60)
        if result.returncode == 0:
            with contextlib.suppress(ValueError):
                root_bytes = int(result.stdout.split()[0]) * 1024
    sample = {
        "ts": iso(),
        "pids": [row["pid"] for row in rows],
        "processes": len(rows),
        "runtime_processes": runtime_count,
        "dispatcher_processes": dispatcher_count,
        "rss_kb": rss_kb,
        "cpu_pct": round(cpu, 3),
        "db_bytes": db_bytes,
        "live_root_bytes": root_bytes,
    }
    append_jsonl(paths.samples, sample)
    return sample


def defspec_from_row(row: Dict[str, Any]) -> DefSpec:
    return DefSpec(
        key=row["key"],
        name=row["name"],
        kind=row["kind"],
        trigger=list(row.get("trigger") or []),
        command=row.get("command"),
        goal=row.get("goal"),
        overlap=row.get("overlap", "skip-if-running"),
        misfire=row.get("misfire", "run-once"),
        grace=int(row.get("grace_secs", 180)),
        timeout=int(row.get("timeout_secs", 120)),
        session_policy=row.get("session_policy", "continue"),
        model=row.get("model"),
        permission=row.get("permission", "assisted"),
        marker=row.get("marker"),
    )


def specs_from_plan(plan: Dict[str, Any]) -> Dict[str, List[DefSpec]]:
    return {
        lane: [defspec_from_row(row) for row in rows]
        for lane, rows in (plan.get("lanes") or {}).items()
    }


def make_plan(
    mode: str,
    binary: Dict[str, Any],
    commit: str,
    start_at: dt.datetime,
    duration_secs: int,
    model: Optional[str],
    budget: Dict[str, Any],
    live_root: Path,
) -> Dict[str, Any]:
    raw_specs = lane_specs(mode, model)
    resolved: Dict[str, List[Dict[str, Any]]] = {}
    for lane, specs in raw_specs.items():
        rows = []
        for spec in specs:
            row = spec.plan_row()
            row["trigger"] = anchor_trigger(spec.trigger, start_at)
            rows.append(row)
        resolved[lane] = rows
    deadline = start_at + dt.timedelta(seconds=duration_secs)
    return {
        "version": VERSION,
        "mode": mode,
        "run_id": "%s-%s-%s"
        % (mode.replace("-", ""), start_at.strftime("%Y%m%dT%H%M%SZ"), uuid.uuid4().hex[:6]),
        "created_at": iso(),
        "start_at": iso(start_at),
        "deadline_at": iso(deadline),
        "duration_secs": duration_secs,
        "commit": commit,
        "binary": binary,
        "model": model,
        "config": {k: v for k, v in config_info().items() if k not in ("env_key_available",)},
        "live_root": str(live_root),
        "evidence_root": str(DEFAULT_EVIDENCE_ROOT),
        "budget": budget,
        "thresholds": dict(THRESHOLDS),
        "lanes": resolved,
    }


def initial_actions(mode: str, start_epoch: float) -> List[Dict[str, Any]]:
    def action(name: str, offset: float) -> Dict[str, Any]:
        return {"name": name, "due_at": start_epoch + offset, "status": "pending", "attempts": 0, "detail": None}

    if mode == "dry-run":
        return [
            action("rules_check", 5),
            action("pause_start", 15),
            action("pause_resume", 75),
            action("pause_verify", 140),
            action("s4_probe", 90),
            action("registrations_verify", 30),
            action("s9_cycle", 180),
            action("fault_kill_1", 5),
            action("fault_restart_1", 155),
            action("fault_recovery_verify_1", 180),
            action("fault_runtime_kill_1", 220),
            action("fault_runtime_verify_1", 245),
            action("misfire_off", 60),
            action("misfire_on", 240),
            action("misfire_verify", 305),
            action("upgrade_swap", 120),
            action("upgrade_verify", 240),
            action("s11_verify", 180),
            action("s8_cli", 120),
            action("agent_verify", 300),
        ]
    return [
        action("rules_check", 15),
        action("registrations_verify", 180),
        action("s4_probe_1", 1800),
        action("s4_probe_2", 28800),
        action("s4_probe_3", 57600),
        action("misfire_off", 900),
        action("misfire_on", 1080),
        action("misfire_verify", 1200),
        action("pause_start", 3600),
        action("pause_resume", 3900),
        action("pause_verify", 4260),
        action("s9_cycle", 21600),
        action("fault_kill_1", 7200),
        action("fault_restart_1", 7350),
        action("fault_recovery_verify_1", 7380),
        action("fault_runtime_kill_1", 14400),
        action("fault_runtime_verify_1", 14430),
        action("fault_kill_2", 43200),
        action("fault_restart_2", 43350),
        action("fault_recovery_verify_2", 43380),
        action("fault_runtime_kill_2", 50400),
        action("fault_runtime_verify_2", 50430),
        action("upgrade_swap", 3600),
        action("upgrade_verify", 3900),
        action("s8_cli_1", 7200),
        action("s8_cli_2", 82800),
        action("s11_verify", 600),
        action("agent_verify_1", 3600),
        action("agent_verify_2", 14400),
        action("agent_verify_3", 28800),
        action("agent_verify_4", 43200),
        action("agent_verify_5", 75600),
    ]


def add_action(state: Dict[str, Any], name: str, due_at: float, recorder: Recorder) -> None:
    if any(action["name"] == name for action in state.get("actions", [])):
        return
    action = {"name": name, "due_at": due_at, "status": "pending", "attempts": 0, "detail": None}
    state.setdefault("actions", []).append(action)
    recorder.event("action_added", name=name, due_at=iso(dt.datetime.fromtimestamp(due_at, dt.timezone.utc)))


def lane_by_name(lanes: Dict[str, Dict[str, Any]], name: str) -> Dict[str, Any]:
    lane = lanes.get(name)
    if not lane:
        raise SoakError("lane %s is not set up" % name)
    return lane


def active_runs(lane: Dict[str, Any], spec_key: Optional[str] = None) -> List[Dict[str, Any]]:
    snapshot = read_schedule(Path(lane["home"]))
    definitions = {row["id"]: row for row in snapshot["definitions"]}
    rows = []
    for occurrence in snapshot["occurrences"]:
        definition = definitions.get(occurrence.get("definition_id"), {})
        if spec_key is not None:
            expected = lane.get("defs", {}).get(spec_key, {}).get("name")
            if definition.get("name") != expected:
                continue
        for run in occurrence.get("runs", []):
            if run.get("state") in ("dispatching", "running"):
                row = dict(run)
                row["occurrence"] = occurrence
                row["definition"] = definition
                rows.append(row)
    return rows


def setup_run(
    paths: RunPaths,
    plan: Dict[str, Any],
    state: Dict[str, Any],
    recorder: Recorder,
) -> Tuple[Dict[str, Dict[str, Any]], Dict[str, List[DefSpec]]]:
    specs_by_lane = specs_from_plan(plan)
    lanes: Dict[str, Dict[str, Any]] = state.get("lanes") or {}
    source_binary = Path(plan["binary"]["path"])
    idle = {"s11": 20, "agent": 180, "fault": 120, "os": 120, "overlap": 120, "misfire": 120, "upgrade": 120}
    for name in ("os", "overlap", "agent", "fault", "misfire", "upgrade", "s11"):
        lane = lanes.get(name)
        if lane is None:
            lane = prepare_lane(paths, name, idle_secs=idle.get(name, 120))
            lane["kind"] = "serve" if name == "fault" else "launchd"
            lanes[name] = lane
            state["lanes"] = lanes
            recorder.state(state)
        assert_isolated_home(Path(lane["home"]), paths.live_root)  # type: ignore[arg-type]
        binary_path = Path(lane["binary"])
        if not binary_path.is_file() or (
            not lane.get("binary_swapped") and sha256_file(binary_path) != plan["binary"]["sha256"]
        ):
            identity = install_lane_binary(source_binary, lane)
            lane["binary_identity"] = identity
            recorder.event("lane_binary_installed", lane=name, **identity)
            state["lanes"] = lanes
            recorder.state(state)
        elif "binary_identity" not in lane:
            lane["binary_identity"] = binary_identity(binary_path)
        for spec in specs_by_lane.get(name, []):
            ensure_definition(lane, spec, recorder)
        state["lanes"] = lanes
        recorder.state(state)

    for name in ("os", "overlap", "agent", "misfire", "upgrade", "s11"):
        lane = lanes[name]
        registration = enable_lane(lane, recorder)
        lane["registration"] = registration
        state["lanes"] = lanes
        recorder.state(state)
    fault = lanes["fault"]
    if not fault.get("serve_pid") or not pid_alive(int(fault["serve_pid"])):
        start_serve(fault, recorder)
        state["lanes"] = lanes
        recorder.state(state)
    return lanes, specs_by_lane


def run_rule_checks(paths: RunPaths, plan: Dict[str, Any], recorder: Recorder) -> Dict[str, Any]:
    lane = prepare_lane(paths, "rules", idle_secs=120)
    lane["kind"] = "rules"
    install_lane_binary(Path(plan["binary"]["path"]), lane)
    now = utcnow()
    specs = [
        DefSpec("once", "soak-rule-once", "shell", ["--at", iso(now + dt.timedelta(hours=1))], command="true", grace=600),
        DefSpec("every", "soak-rule-every", "shell", ["--every", "3600"], command="true", grace=600),
        DefSpec("daily", "soak-rule-daily", "shell", ["--daily", "09:15", "--timezone", "UTC"], command="true", grace=600),
        DefSpec(
            "weekly",
            "soak-rule-weekly",
            "shell",
            ["--weekly", "mon,wed", "--local-time", "09:15", "--timezone", "UTC"],
            command="true",
            grace=600,
        ),
        DefSpec("cron", "soak-rule-cron", "shell", ["--cron", "*/5 * * * *", "--timezone", "UTC"], command="true", grace=600),
    ]
    results: List[Dict[str, Any]] = []
    for spec in specs:
        created = lane_json(lane, _add_args(lane, spec), timeout=120)
        next_value = created.get("next_occurrence")
        parsed = parse_iso(next_value) if next_value else None
        checks: Dict[str, Any] = {"next_present": parsed is not None}
        if spec.key == "once":
            checks["exact_instant"] = bool(parsed and abs((parsed - now - dt.timedelta(hours=1)).total_seconds()) <= 2)
        elif spec.key == "every":
            checks["due_immediately"] = bool(parsed and abs((parsed - now).total_seconds()) <= 5)
        elif spec.key == "daily":
            checks["future"] = bool(parsed and parsed > now)
            checks["local_time"] = bool(parsed and parsed.hour == 9 and parsed.minute == 15)
        elif spec.key == "weekly":
            checks["future"] = bool(parsed and parsed > now)
            checks["weekday"] = bool(parsed and parsed.weekday() in (0, 2))
        elif spec.key == "cron":
            checks["future"] = bool(parsed and parsed > now)
            checks["minute_step"] = bool(parsed and parsed.minute % 5 == 0)
        results.append(
            {
                "key": spec.key,
                "id": created.get("id"),
                "next_occurrence": next_value,
                "checks": checks,
                "passed": all(checks.values()),
            }
        )
        lane["defs"][spec.key] = {"id": created["id"], "name": spec.name, "spec": spec.plan_row()}

    # Retime, then remove, without leaving a claimable slot or reviving history.
    once_id = results[0]["id"]
    replacement = now + dt.timedelta(hours=2)
    updated = lane_json(
        lane,
        [
            "schedule",
            "update",
            once_id,
            "--at",
            iso(replacement),
            "--expected-revision",
            _definition_revision(lane, once_id),
        ],
    )
    updated_next = updated.get("next_occurrence")
    retime_ok = bool(updated_next and abs((parse_iso(updated_next) - replacement).total_seconds()) <= 2)
    removed = lane_cli(lane, ["schedule", "remove", once_id], timeout=60, check=False)
    list_after = lane_json(lane, ["schedule", "list"])
    remove_ok = removed.returncode == 0 and not any(row.get("id") == once_id for row in list_after)
    definition_count = len(list_after)
    for row in results[1:]:
        lane_cli(lane, ["schedule", "remove", row["id"]], timeout=60, check=False)
    final_list = lane_json(lane, ["schedule", "list"])
    result = {
        "ts": iso(),
        "triggers": results,
        "retime": {"passed": retime_ok, "next_occurrence": updated_next},
        "remove": {"passed": remove_ok, "definitions_before": definition_count, "definitions_after": len(final_list)},
        "passed": all(row["passed"] for row in results) and retime_ok and remove_ok and len(final_list) == 0,
        "limit": "deterministic rule projection only; no multi-day real trigger is claimed",
    }
    recorder.event("rules_checked", **result)
    return result


def _add_args(lane: Dict[str, Any], spec: DefSpec) -> List[str]:
    args = [
        "schedule",
        "add",
        "--name",
        spec.name,
        "--workspace",
        str(lane["repo"]),
        "--overlap",
        spec.overlap,
        "--misfire",
        spec.misfire,
        "--misfire-grace",
        str(spec.grace),
        "--timeout",
        str(spec.timeout),
        "--permission",
        spec.permission,
        "--grant-unattended",
        "--allow-external-side-effects",
    ]
    if spec.kind == "shell":
        args.extend(["--command", spec.command or "", "--session-policy", spec.session_policy])
    elif spec.kind == "agent":
        args.extend(["--agent-goal", spec.goal or "", "--session-policy", spec.session_policy])
        if spec.model:
            args.extend(["--model", spec.model])
    args.extend(spec.trigger)
    return args


def _definition_revision(lane: Dict[str, Any], definition_id: str) -> str:
    row = lane_json(lane, ["schedule", "show", definition_id])
    revision = row.get("revision") if isinstance(row, dict) else None
    if not revision:
        raise SoakError("definition %s has no revision" % definition_id)
    return str(revision)


class ActionRetry(Exception):
    pass


class Supervisor:
    def __init__(self, paths: RunPaths, plan: Dict[str, Any], state: Dict[str, Any], recorder: Recorder):
        self.paths = paths
        self.plan = plan
        self.state = state
        self.recorder = recorder
        self.lanes: Dict[str, Dict[str, Any]] = state.get("lanes") or {}
        self.specs_by_lane = specs_from_plan(plan)
        self.checks: Dict[str, Any] = state.setdefault("checks", {})
        self.seen_runs = load_seen_runs(paths)
        self.stop_requested = False
        self.start_epoch = parse_iso(plan["start_at"]).timestamp()
        self.deadline_epoch = parse_iso(plan["deadline_at"]).timestamp()
        self.mode = plan["mode"]
        state.setdefault("actions", initial_actions(self.mode, self.start_epoch))

    def install_signals(self) -> None:
        def handler(signum: int, frame: Any) -> None:
            self.stop_requested = True
            self.recorder.event("signal", signal=signum)

        signal.signal(signal.SIGTERM, handler)
        signal.signal(signal.SIGINT, handler)

    def persist(self) -> None:
        self.state["lanes"] = self.lanes
        self.recorder.state(self.state)

    def heartbeat(self) -> None:
        now = time.time()
        self.recorder.heartbeat(
            {
                "ts": iso(),
                "pid": os.getpid(),
                "status": self.state.get("status"),
                "phase": self.state.get("phase"),
                "now_epoch": now,
                "deadline_epoch": self.deadline_epoch,
                "remaining_secs": max(0, int(self.deadline_epoch - now)),
                "runs_observed": len(self.seen_runs),
                "pending_actions": [
                    action["name"] for action in self.state.get("actions", []) if action["status"] == "pending"
                ],
                "budget_stopped": self.state.get("budget_stopped", False),
                "disk_free_bytes": self.state.get("disk_free_bytes"),
            }
        )

    def mark(self, name: str, passed: bool, detail: Dict[str, Any]) -> None:
        self.checks[name] = {"passed": passed, "detail": detail, "ts": iso()}
        self.recorder.event("check", name=name, passed=passed, detail=detail)

    def run(self) -> int:
        self.state.update(
            {
                "status": "DRY_RUNNING" if self.mode == "dry-run" else "RUNNING",
                "phase": "setup",
                "runner_pid": os.getpid(),
                "runner_started_at": iso(),
                "error": None,
            }
        )
        self.persist()
        self.install_signals()
        self.recorder.event("runner_started", pid=os.getpid(), mode=self.mode)
        try:
            self.lanes, self.specs_by_lane = setup_run(self.paths, self.plan, self.state, self.recorder)
            self.state["phase"] = "running"
            self.state["lanes_ready_at"] = iso()
            self.persist()
            self.heartbeat()
            self.loop()
            reason = self.state.get("stop_reason") or "deadline"
        except Exception as error:  # noqa: BLE001 - the error must be recorded, not hidden
            self.recorder.log("supervisor failed: %s\n%s" % (error, traceback.format_exc()))
            self.lanes = self.state.get("lanes") or self.lanes
            self.state.update({"status": "FAILED", "phase": "failed", "error": redact(str(error))})
            self.persist()
            self.mark("runner", False, {"error": redact(str(error)), "traceback": traceback.format_exc()[-4000:]})
            with contextlib.suppress(Exception):
                self.finish(normal=False, reason="supervisor failure")
            return 1
        return self.finish(normal=(reason == "deadline"), reason=reason)

    def loop(self) -> None:
        next_poll = 0.0
        next_sample = 0.0
        next_budget = 0.0
        next_heartbeat = 0.0
        next_disk = 0.0
        while True:
            now = time.time()
            if self.stop_requested:
                self.state["stop_reason"] = "signal"
                self.recorder.event("stop_requested", source="signal")
                return
            if now >= self.deadline_epoch:
                self.state["stop_reason"] = "deadline"
                self.recorder.event("deadline_reached", deadline=self.plan["deadline_at"])
                return
            if now >= next_disk:
                usage = shutil.disk_usage(str(self.paths.live_root or "/tmp"))
                self.state["disk_free_bytes"] = usage.free
                reason = disk_stop_reason(usage.free)
                if reason:
                    record_runner_error(
                        self.state,
                        "disk",
                        "free space %d bytes is below the %d byte floor"
                        % (usage.free, DISK_FLOOR_BYTES),
                    )
                    self.recorder.event(
                        "disk_low", free_bytes=usage.free, floor_bytes=DISK_FLOOR_BYTES
                    )
                    self.state["stop_reason"] = reason
                    return
                next_disk = now + 10
            for action in list(self.state.get("actions", [])):
                if action["status"] != "pending" or now < action["due_at"]:
                    continue
                self.execute_action(action)
            if now >= next_poll:
                with contextlib.suppress(Exception):
                    emit_run_records(
                        self.paths,
                        self.recorder,
                        self.lanes,
                        self.specs_by_lane,
                        self.seen_runs,
                        self.plan,
                    )
                next_poll = now + 20
            if now >= next_sample:
                with contextlib.suppress(Exception):
                    sample = sample_processes(self.paths, self.lanes, self.recorder)
                    self.state["last_sample"] = sample
                next_sample = now + 60
            if now >= next_budget:
                self.check_budget()
                next_budget = now + 60
            if now >= next_heartbeat:
                self.heartbeat()
                self.persist()
                next_heartbeat = now + 10
            time.sleep(3)

    def execute_action(self, action: Dict[str, Any]) -> None:
        name = action["name"]
        handler = self.action_handler(name)
        if handler is None:
            action["status"] = "failed"
            action["detail"] = {"error": "no handler"}
            self.recorder.event("action_failed", name=name, error="no handler")
            self.persist()
            return
        try:
            detail = handler(name)
        except ActionRetry as error:
            action["attempts"] = int(action.get("attempts", 0)) + 1
            action["detail"] = {"retry": str(error)}
            if action["attempts"] >= 40:
                action["status"] = "failed"
                message = str(error)
                record_runner_error(self.state, name, message)
                self.recorder.event(
                    "action_failed",
                    name=name,
                    error=message,
                    attempts=action["attempts"],
                    source="runner",
                )
            else:
                self.recorder.event("action_retry", name=name, attempt=action["attempts"], detail=str(error))
            self.persist()
            return
        except Exception as error:  # noqa: BLE001 - record the real failure
            action["status"] = "failed"
            message = redact(str(error))
            action["detail"] = {"error": message, "source": "runner"}
            record_runner_error(self.state, name, message)
            self.recorder.event(
                "action_failed",
                name=name,
                error=message,
                traceback=traceback.format_exc()[-3000:],
                source="runner",
            )
            self.persist()
            return
        action["status"] = "done"
        action["detail"] = detail
        self.recorder.event("action_done", name=name, detail=detail)
        self.persist()

    def action_handler(self, name: str):
        exact = {
            "rules_check": self.do_rules_check,
            "pause_start": self.do_pause_start,
            "pause_resume": self.do_pause_resume,
            "pause_verify": self.do_pause_verify,
            "s4_probe": self.do_s4_probe,
            "misfire_off": self.do_misfire_off,
            "misfire_on": self.do_misfire_on,
            "misfire_verify": self.do_misfire_verify,
            "upgrade_swap": self.do_upgrade_swap,
            "upgrade_verify": self.do_upgrade_verify,
            "s11_verify": self.do_s11_verify,
            "s8_cli": self.do_s8_cli,
            "s9_cycle": self.do_s9_cycle,
            "registrations_verify": self.do_registrations_verify,
            "agent_verify": self.do_agent_verify,
        }
        if name in exact:
            return exact[name]
        if name.startswith("s4_probe_"):
            return self.do_s4_probe
        if name.startswith("s4_verify_"):
            return self.do_s4_verify
        if name.startswith("s8_cli_"):
            return self.do_s8_cli
        if name.startswith("agent_verify_"):
            return self.do_agent_verify
        if name.startswith("fault_kill_"):
            return self.do_fault_kill
        if name.startswith("fault_restart_"):
            return self.do_fault_restart
        if name.startswith("fault_recovery_verify_"):
            return self.do_fault_recovery_verify
        if name.startswith("fault_runtime_kill_"):
            return self.do_fault_runtime_kill
        if name.startswith("fault_runtime_verify_"):
            return self.do_fault_runtime_verify
        return None

    def do_rules_check(self, name: str) -> Dict[str, Any]:
        result = run_rule_checks(self.paths, self.plan, self.recorder)
        self.mark("S1_rules", bool(result["passed"]), result)
        return result

    def _definition_id(self, lane_name: str, key: str) -> str:
        lane = lane_by_name(self.lanes, lane_name)
        definition = lane.get("defs", {}).get(key)
        if not definition:
            raise SoakError("lane %s has no definition %s" % (lane_name, key))
        return str(definition["id"])

    def do_pause_start(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "os")
        definition_id = self._definition_id("os", "pause")
        lane_cli(lane, ["schedule", "pause", definition_id], timeout=60)
        paused_at = iso()
        self.state["pause_test"] = {"paused_at": paused_at, "definition": definition_id}
        self.recorder.event("pause_started", definition=definition_id, at=paused_at)
        return {"paused_at": paused_at, "definition": definition_id}

    def do_pause_resume(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "os")
        definition_id = self._definition_id("os", "pause")
        lane_cli(lane, ["schedule", "resume", definition_id], timeout=60)
        resumed_at = iso()
        self.state.setdefault("pause_test", {})["resumed_at"] = resumed_at
        self.recorder.event("pause_resumed", definition=definition_id, at=resumed_at)
        return {"resumed_at": resumed_at, "definition": definition_id}

    def do_pause_verify(self, name: str) -> Dict[str, Any]:
        test = self.state.get("pause_test") or {}
        definition_id = test.get("definition")
        if not definition_id:
            raise SoakError("pause test was not started")
        snapshot = read_schedule(Path(lane_by_name(self.lanes, "os")["home"]))
        runs = [run for run in snapshot["runs"] if run.get("definition_id") == definition_id]
        paused_at = parse_iso(test["paused_at"])
        resumed_at = parse_iso(test.get("resumed_at") or iso())
        during = [
            run
            for run in runs
            if run.get("started_at")
            and paused_at + dt.timedelta(seconds=5)
            <= parse_iso(run["started_at"])
            < resumed_at - dt.timedelta(seconds=2)
        ]
        after = [run for run in runs if run.get("started_at") and parse_iso(run["started_at"]) >= resumed_at]
        if not after:
            raise ActionRetry("no run after resume yet")
        result = {
            "runs_during_pause": len(during),
            "runs_after_resume": len(after),
            "passed": len(during) == 0 and len(after) >= 1,
        }
        self.mark("S1_pause_resume", result["passed"], result)
        return result

    def _wait_for_active_run(self, lane_name: str, exclude_run_now: bool = True) -> Dict[str, Any]:
        for run in active_runs(lane_by_name(self.lanes, lane_name)):
            occurrence = run.get("occurrence") or {}
            if exclude_run_now and occurrence.get("run_now"):
                continue
            return run
        raise ActionRetry("no active %s run yet" % lane_name)

    def do_s4_probe(self, name: str) -> Dict[str, Any]:
        """Start an observation window over the lane's natural overlap cycles.

        The three definitions already produce skip, queue-one, and
        allow-concurrent outcomes on their own grid. Firing additional
        `run-now` slots here stacked a fourth and fifth turn onto the
        runtime's four-turn admission capacity, so the policy observation
        failed for a reason that had nothing to do with the policy.
        """
        overlap = lane_by_name(self.lanes, "overlap")
        record = self.state.setdefault("s4_probes", {}).setdefault(name, {"started_at": iso()})
        record.setdefault("started_at", iso())
        record["active_at_probe"] = [run.get("id") for run in active_runs(overlap)]
        record["mode"] = "natural"
        suffix = "1" if self.mode == "dry-run" else name.rsplit("_", 1)[-1]
        add_action(self.state, "s4_verify_" + suffix, time.time() + 150, self.recorder)
        self.persist()
        return {
            "probe_started": record["started_at"],
            "mode": record["mode"],
            "active_at_probe": record["active_at_probe"],
        }

    def do_s4_verify(self, name: str) -> Dict[str, Any]:
        suffix = name.rsplit("_", 1)[-1]
        probe_name = "s4_probe" if self.mode == "dry-run" else "s4_probe_" + suffix
        record = (self.state.get("s4_probes") or {}).get(probe_name)
        if not record:
            raise SoakError("probe %s not found" % probe_name)
        probe_started = parse_iso(record["started_at"])
        overlap = lane_by_name(self.lanes, "overlap")
        snapshot = read_schedule(Path(overlap["home"]))
        definition_ids = {key: self._definition_id("overlap", key) for key in ("skip", "queue", "allow")}
        results = natural_overlap_observation(snapshot, definition_ids, probe_started)
        elapsed = time.time() - probe_started.timestamp()
        status = overlap_probe_status(
            [(bool(item.get("terminal")), bool(item.get("policy_ok"))) for item in results.values()],
            elapsed,
        )
        if status == "OPEN":
            raise ActionRetry("overlap runs are not terminal yet")
        allow = results.get("allow") or {}
        overlap_observed = bool(allow.get("policy_ok"))
        if status == "UNKNOWN":
            finding = (
                "overlap observation window of %ss ended before every slot had a terminal run"
                % OVERLAP_OBSERVE_SECS
            )
            execution_passed = False
            passed = False
        else:
            finding = None
            execution_passed = bool(allow.get("terminal") and allow.get("execution_ok"))
            if not execution_passed and status == "PASS":
                finding = (
                    "the allow-concurrent slot was claimed while an active run was still "
                    "running, but the second run did not reach a succeeded terminal"
                )
            passed = status == "PASS" and all(item.get("policy_ok") for item in results.values())
            if not passed:
                status = "FAIL"
                finding = finding or "an overlap slot finished outside its policy"
        self.mark(
            "S4_allow_execution_" + suffix,
            execution_passed,
            {
                "status": "PASS" if execution_passed else status,
                "run": (allow.get("runs") or [None])[0],
                "execution_state": (allow.get("runs") or [{}])[-1].get("state") if allow.get("runs") else None,
                "overlap_observed": overlap_observed,
                "finding": None if execution_passed else finding,
            },
        )
        payload = {
            "probe": suffix,
            "status": "PASS" if passed else status,
            "results": results,
            "passed": passed,
            "finding": None if passed else finding,
        }
        self.mark("S4_" + suffix, passed, payload)
        return payload

    def _fault_lane(self) -> Dict[str, Any]:
        return lane_by_name(self.lanes, "fault")

    def do_fault_kill(self, name: str) -> Dict[str, Any]:
        suffix = name.rsplit("_", 1)[-1]
        lane = self._fault_lane()
        active = self._wait_for_active_run("fault")
        pid = int(lane.get("serve_pid") or 0)
        command = pid_cmdline(pid)
        if not pid_alive(pid) or str(lane["home"]) not in command or "schedule" not in command:
            raise ActionRetry("fault dispatcher is not the recorded process")
        before = marker_counts(lane, self.specs_by_lane["fault"])
        runtimes = [
            row
            for row in matching_processes(self.paths.live_root)  # type: ignore[arg-type]
            if str(lane["repo"]) in row["command"] and " serve" in row["command"] and row["pid"] != pid
        ]
        os.kill(pid, signal.SIGKILL)
        killed_at = iso()
        self.state.setdefault("fault_kills", []).append(
            {
                "suffix": suffix,
                "pid": pid,
                "command": command[:400],
                "killed_at": killed_at,
                "effect_before": before,
                "runtime_pids": [row["pid"] for row in runtimes],
                "active_runs": [active.get("id")],
            }
        )
        lane["serve_pid"] = None
        self.persist()
        self.recorder.event(
            "fault_dispatcher_killed",
            suffix=suffix,
            pid=pid,
            at=killed_at,
            ownership_ok=(str(lane["home"]) in command),
            runtime_pids=[row["pid"] for row in runtimes],
        )
        return {"pid": pid, "killed_at": killed_at, "runtime_pids": [row["pid"] for row in runtimes]}

    def do_fault_restart(self, name: str) -> Dict[str, Any]:
        lane = self._fault_lane()
        if lane.get("serve_pid") and pid_alive(int(lane["serve_pid"])):
            return {"already_running": lane["serve_pid"]}
        pid = start_serve(lane, self.recorder)
        self.persist()
        return {"pid": pid, "restarted_at": iso()}

    def do_fault_recovery_verify(self, name: str) -> Dict[str, Any]:
        suffix = name.rsplit("_", 1)[-1]
        lane = self._fault_lane()
        kill = next(
            (row for row in reversed(self.state.get("fault_kills", [])) if row.get("suffix") == suffix),
            None,
        )
        if not kill:
            raise ActionRetry("no dispatcher kill recorded for %s yet" % suffix)
        killed_at = parse_iso(kill["killed_at"])
        snapshot = read_schedule(Path(lane["home"]))
        fault_id = self._definition_id("fault", "fault")
        facts = fault_recovery_facts(
            snapshot,
            fault_id,
            killed_at,
            kill.get("active_runs") or [],
        )
        if not facts["affected"]:
            raise ActionRetry("the killed dispatcher's in-flight run is not visible yet")
        if facts["stuck"] or not facts["later_success"]:
            raise ActionRetry(
                "recovery facts are not visible yet (stuck=%d recovered=%d unknown=%d later_success=%d)"
                % (
                    len(facts["stuck"]),
                    len(facts["recovered"]),
                    len(facts["unknown"]),
                    facts["later_success"],
                )
            )
        payload = {
            "suffix": suffix,
            "affected_runs": facts["affected"],
            "recovered_succeeded": facts["recovered"],
            "unknown_runs": facts["unknown"],
            "duplicate_occurrences": facts["duplicate_occurrences"],
            "later_success": facts["later_success"],
            "passed": facts["passed"],
            "note": (
                "a restarted dispatcher observes the runtime's terminal record and settles the "
                "run it lost; a lost outcome is settled unknown and is never re-delivered"
            ),
        }
        self.mark("S5_dispatcher_" + suffix, payload["passed"], payload)
        return payload

    def _find_fault_runtime(self) -> Optional[Dict[str, Any]]:
        lane = self._fault_lane()
        for row in matching_processes(self.paths.live_root):  # type: ignore[arg-type]
            if row["pid"] == int(lane.get("serve_pid") or 0):
                continue
            command = row["command"]
            if str(lane["repo"]) in command and re.search(r"\bserve\b", command):
                return row
        return None

    def do_fault_runtime_kill(self, name: str) -> Dict[str, Any]:
        suffix = name.rsplit("_", 1)[-1]
        self._wait_for_active_run("fault")
        runtime = self._find_fault_runtime()
        if runtime is None:
            raise ActionRetry("fault runtime is not visible yet")
        os.kill(runtime["pid"], signal.SIGKILL)
        killed_at = iso()
        self.state.setdefault("runtime_kills", []).append(
            {"suffix": suffix, "pid": runtime["pid"], "command": runtime["command"][:400], "killed_at": killed_at}
        )
        self.recorder.event(
            "fault_runtime_killed",
            suffix=suffix,
            pid=runtime["pid"],
            at=killed_at,
            ownership_ok=(str(self._fault_lane()["repo"]) in runtime["command"]),
        )
        self.persist()
        return {"pid": runtime["pid"], "killed_at": killed_at}

    def do_fault_runtime_verify(self, name: str) -> Dict[str, Any]:
        suffix = name.rsplit("_", 1)[-1]
        kill = next(
            (row for row in reversed(self.state.get("runtime_kills", [])) if row.get("suffix") == suffix),
            None,
        )
        if not kill:
            raise ActionRetry("no runtime kill recorded for %s yet" % suffix)
        killed_at = parse_iso(kill["killed_at"])
        lane = self._fault_lane()
        snapshot = read_schedule(Path(lane["home"]))
        fault_id = self._definition_id("fault", "fault")
        affected = [
            run
            for run in snapshot["runs"]
            if run.get("definition_id") == fault_id
            and run.get("started_at")
            and parse_iso(run["started_at"]) <= killed_at + dt.timedelta(seconds=5)
            and run.get("finished_at")
        ]
        unknown = [run for run in affected if run.get("state") == "unknown"]
        later_success = [
            run
            for run in snapshot["runs"]
            if run.get("definition_id") == fault_id
            and run.get("state") == "succeeded"
            and run.get("started_at")
            and parse_iso(run["started_at"]) > killed_at
        ]
        runtime_count = len(
            [
                row
                for row in matching_processes(self.paths.live_root)  # type: ignore[arg-type]
                if row["pid"] != int(lane.get("serve_pid") or 0)
                and str(lane["repo"]) in row["command"]
                and re.search(r"\bserve\b", row["command"])
            ]
        )
        if not unknown or not later_success:
            raise ActionRetry(
                "runtime kill consequences are not visible yet (unknown=%d later_success=%d)"
                % (len(unknown), len(later_success))
            )
        payload = {
            "suffix": suffix,
            "unknown_runs": [
                {"id": run.get("id"), "error": redact(str(run.get("error"))) if run.get("error") else None}
                for run in unknown
            ],
            "later_success": len(later_success),
            "runtime_instances": runtime_count,
            "passed": bool(unknown) and bool(later_success) and runtime_count <= 1,
            "note": "the dispatcher reports the lost runtime as Unknown; the next run starts a fresh runtime",
        }
        self.mark("S5_runtime_" + suffix, payload["passed"], payload)
        return payload

    def do_misfire_off(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "misfire")
        result = disable_lane(lane, self.recorder)
        self.state["misfire_offline"] = {"off_at": iso(), "result": result}
        self.persist()
        return result

    def do_misfire_on(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "misfire")
        result = enable_lane(lane, self.recorder)
        self.state.setdefault("misfire_offline", {})["on_at"] = iso()
        self.persist()
        return result

    def do_misfire_verify(self, name: str) -> Dict[str, Any]:
        window = self.state.get("misfire_offline") or {}
        if not window.get("off_at") or not window.get("on_at"):
            raise ActionRetry("misfire offline window is incomplete")
        lane = lane_by_name(self.lanes, "misfire")
        snapshot = read_schedule(Path(lane["home"]))
        definition_ids = {key: self._definition_id("misfire", key) for key in ("skip", "runonce")}
        facts = misfire_facts(
            snapshot,
            definition_ids,
            parse_iso(window["off_at"]),
            parse_iso(window["on_at"]),
        )
        if not facts["skip_missed"] and not facts["recovered"]:
            raise ActionRetry("misfire policy outcome is not visible yet")
        payload = {
            "off_at": window["off_at"],
            "on_at": window["on_at"],
            "skip_missed": facts["skip_missed"],
            "runonce_recovered": len(facts["recovered"]),
            "runonce_states": [run.get("state") for run in facts["recovered"]],
            "duplicates": facts["duplicates"],
            "passed": facts["passed"],
            "note": "the store keeps at most one pending slot; intermediate offline grid points are not materialized",
        }
        self.mark("S6_misfire", payload["passed"], payload)
        return payload

    def do_upgrade_swap(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "upgrade")
        snapshot = read_schedule(Path(lane["home"]))
        succeeded_before = [
            run
            for run in snapshot["runs"]
            if run.get("state") == "succeeded" and run.get("finished_at")
        ]
        if not succeeded_before:
            raise ActionRetry("no successful pre-upgrade run yet")
        before = binary_identity(Path(lane["binary"]))
        source = Path(self.plan["binary"]["path"])
        temporary = Path(lane["binary"]).with_name("leveler.new-" + uuid.uuid4().hex[:8])
        shutil.copyfile(source, temporary)
        os.chmod(temporary, 0o755)
        with open(temporary, "rb") as handle:
            os.fsync(handle.fileno())
        os.replace(temporary, lane["binary"])
        after = binary_identity(Path(lane["binary"]))
        lane["binary_swapped"] = True
        lane["binary_identity"] = after
        payload = {
            "swapped_at": iso(),
            "before": before,
            "after": after,
            "succeeded_runs_before": len(succeeded_before),
            "same_artifact": before["sha256"] == after["sha256"],
            "cross_revision": False,
            "note": "same clean artifact replacement; a cross-revision build is NOT_VERIFIED in this run",
        }
        self.state["upgrade"] = payload
        self.persist()
        self.recorder.event("binary_swapped", **payload)
        self.mark("S7_replacement", True, payload)
        return payload

    def do_upgrade_verify(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "upgrade")
        snapshot = read_schedule(Path(lane["home"]))
        swap_at = parse_iso((self.state.get("upgrade") or {}).get("swapped_at") or iso())
        after_runs = [
            run
            for run in snapshot["runs"]
            if run.get("started_at") and parse_iso(run["started_at"]) > swap_at
        ]
        succeeded = [run for run in after_runs if run.get("state") == "succeeded"]
        if not succeeded:
            raise ActionRetry("no run after the binary swap yet")
        duplicates = [occurrence.get("id") for occurrence in snapshot["occurrences"] if int(occurrence.get("run_count") or 0) > 1]
        payload = {
            "runs_after_swap": len(after_runs),
            "succeeded_after_swap": len(succeeded),
            "history_runs": len(snapshot["runs"]),
            "duplicates": duplicates,
            "next_occurrence": next(
                (
                    occurrence.get("scheduled_for")
                    for occurrence in sorted(snapshot["occurrences"], key=lambda row: row.get("scheduled_for") or "")
                    if occurrence.get("state") == "pending"
                ),
                None,
            ),
            "passed": len(succeeded) >= 1 and not duplicates,
        }
        self.mark("S7_continuity", payload["passed"], payload)
        return payload

    def _runtime_processes(self, lane_name: str) -> List[Dict[str, Any]]:
        lane = lane_by_name(self.lanes, lane_name)
        rows = []
        for row in matching_processes(self.paths.live_root):  # type: ignore[arg-type]
            command = row["command"]
            if str(lane["repo"]) in command and re.search(r"\bserve\b", command) and "schedule" not in command:
                rows.append(row)
        return rows

    def do_s11_verify(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "s11")
        snapshot = read_schedule(Path(lane["home"]))
        definition_id = self._definition_id("s11", "once")
        runs = [run for run in snapshot["runs"] if run.get("definition_id") == definition_id]
        succeeded = [run for run in runs if run.get("state") == "succeeded"]
        if not succeeded:
            raise ActionRetry("the S11 once run has not succeeded yet")
        runtime_rows = self._runtime_processes("s11")
        if runtime_rows:
            raise ActionRetry("the on-demand runtime has not exited yet: %s" % [row["pid"] for row in runtime_rows])
        payload = {
            "runs": len(runs),
            "succeeded": len(succeeded),
            "runtime_pids_after_idle": [],
            "idle_timeout_secs": lane.get("idle_secs"),
            "passed": len(succeeded) == 1 and not runtime_rows,
            "note": "the runtime was started by the OS pass and no resident serve process owns the schedule",
        }
        self.mark("S11_on_demand", payload["passed"], payload)
        return payload

    def do_s8_cli(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "os")
        definitions = lane_json(lane, ["schedule", "list"])
        by_name = {row["name"]: row for row in definitions}
        target = by_name.get("soak-os-tick")
        if not target:
            raise SoakError("the OS tick definition is missing")
        shown = lane_json(lane, ["schedule", "show", target["id"]])
        runs = lane_json(lane, ["schedule", "runs", "--definition", target["id"]])
        snapshot = read_schedule(Path(lane["home"]))
        db_runs = len([run for run in snapshot["runs"] if run.get("definition_id") == target["id"]])
        payload = {
            "definition": target["id"],
            "next_occurrence": shown.get("next_occurrence"),
            "revision": shown.get("revision"),
            "cli_runs": len(runs),
            "db_runs": db_runs,
            "consistent": len(runs) == db_runs,
            "tui": "NOT_VERIFIED",
            "desktop": "NOT_VERIFIED",
            "agent_tools": "NOT_VERIFIED",
            "passed": len(runs) == db_runs,
        }
        self.mark("S8_" + name, payload["passed"], payload)
        return payload

    def do_s9_cycle(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "s11")
        definitions_before = lane_json(lane, ["schedule", "list"])
        runs_before = read_schedule(Path(lane["home"]))["runs"]
        removed = lane_json(lane, ["schedule", "uninstall"])
        label = lane.get("label") or label_for(Path(lane["home"]))
        plist = plist_for(label)
        after_remove = {
            "label": label,
            "plist_exists": plist.is_file(),
            "loaded": launchd_loaded(label),
            "registered": removed.get("registered"),
            "definitions": len(lane_json(lane, ["schedule", "list"])),
            "runs": len(read_schedule(Path(lane["home"]))["runs"]),
        }
        again = enable_lane(lane, self.recorder)
        after_enable = registration_status(lane)
        payload = {
            "definitions_before": len(definitions_before),
            "runs_before": len(runs_before),
            "uninstall": after_remove,
            "reenable": again,
            "after_enable": after_enable,
            "passed": (
                not after_remove["plist_exists"]
                and not after_remove["loaded"]
                and after_remove["definitions"] == len(definitions_before)
                and after_remove["runs"] == len(runs_before)
                and after_enable["plist_exists"]
                and after_enable["loaded"]
            ),
        }
        self.mark("S9_install_uninstall", payload["passed"], payload)
        return payload

    def do_registrations_verify(self, name: str) -> Dict[str, Any]:
        rows = {}
        for lane_name in ("os", "overlap", "agent", "misfire", "upgrade", "s11"):
            lane = lane_by_name(self.lanes, lane_name)
            status = registration_status(lane)
            status["definitions"] = len(lane_json(lane, ["schedule", "list"]))
            rows[lane_name] = status
        payload = {"lanes": rows, "passed": all(row["plist_exists"] and row["loaded"] for row in rows.values())}
        self.mark("S9_registration", payload["passed"], payload)
        return payload

    def do_agent_verify(self, name: str) -> Dict[str, Any]:
        lane = lane_by_name(self.lanes, "agent")
        snapshot = read_schedule(Path(lane["home"]))
        definitions = {row["id"]: row for row in snapshot["definitions"]}
        rows = []
        for key in ("agent_continue", "agent_fresh", "agent_short"):
            definition_id = self._definition_id("agent", key) if key in lane.get("defs", {}) else None
            if not definition_id:
                continue
            runs = [run for run in snapshot["runs"] if run.get("definition_id") == definition_id]
            terminal = [run for run in runs if run.get("state") not in ("dispatching", "running", None)]
            rows.append(
                {
                    "key": key,
                    "definition": definition_id,
                    "runs": len(runs),
                    "terminal": len(terminal),
                    "states": [run.get("state") for run in runs],
                    "sessions": sorted({str(run.get("session_id")) for run in runs if run.get("session_id")}),
                    "usage_missing": [
                        run.get("id")
                        for run in terminal
                        if run.get("usage_requests") is None or run.get("cost_usd_micros") is None
                    ],
                    "errors": [redact(str(run.get("error"))) for run in terminal if run.get("error")],
                }
            )
        if not any(row["terminal"] for row in rows):
            raise ActionRetry("no terminal agent run yet")
        agents = read_agent_sessions(Path(lane["home"]), Path(lane["repo"]))
        markers = marker_counts(lane, [spec for spec in self.specs_by_lane["agent"]])
        marker_ok = True
        for row in rows:
            spec = next((item for item in self.specs_by_lane["agent"] if item.key == row["key"]), None)
            if not spec or not spec.marker or "succeeded" not in row["states"]:
                continue
            marker = markers.get(row["key"]) or {}
            if not marker.get("exists"):
                marker_ok = False
            elif marker.get("value_ok") is False:
                marker_ok = False
        fresh = next((row for row in rows if row["key"] == "agent_fresh"), None)
        fresh_ok = True
        if fresh and fresh["terminal"]:
            fresh_ok = any(state == "succeeded" for state in fresh["states"])
        short = next((row for row in rows if row["key"] == "agent_short"), None)
        short_ok = True
        if short:
            short_ok = any(state == "succeeded" for state in short["states"])
        succeeded = [state for row in rows for state in row["states"] if state == "succeeded"]
        usage_missing = [run for row in rows for run in row["usage_missing"]]
        payload = {
            "definitions": rows,
            "sessions": agents["sessions"],
            "markers": markers,
            "succeeded": len(succeeded),
            "usage_missing": usage_missing,
            "passed": bool(succeeded) and fresh_ok and short_ok and marker_ok and not usage_missing,
        }
        self.mark("S2_" + name, payload["passed"], payload)
        return payload

    def check_budget(self) -> None:
        budget = self.plan.get("budget") or {}
        max_cost = float(budget.get("max_cost_usd", 0.0))
        max_tokens = int(budget.get("max_total_tokens", 0))
        cost_micros = 0
        tokens = 0
        missing_cost = 0
        for lane in self.lanes.values():
            snapshot = read_schedule(Path(lane["home"]))
            for run in snapshot["runs"]:
                cost_micros += int(run.get("cost_usd_micros") or 0)
                tokens += sum(
                    int(run.get(key) or 0)
                    for key in ("input_tokens", "cached_input_tokens", "output_tokens", "reasoning_tokens")
                )
                if run.get("usage_requests") is not None and run.get("cost_usd_micros") is None:
                    missing_cost += 1
        exceeded = (max_cost > 0 and cost_micros / 1_000_000.0 >= max_cost) or (
            max_tokens > 0 and tokens >= max_tokens
        )
        self.state["budget_usage"] = {
            "cost_usd": round(cost_micros / 1_000_000.0, 6),
            "cost_usd_micros": cost_micros,
            "tokens": tokens,
            "usage_rows_without_cost": missing_cost,
            "max_cost_usd": max_cost,
            "max_total_tokens": max_tokens,
            "exceeded": exceeded,
            "ts": iso(),
        }
        if exceeded and not self.state.get("budget_stopped"):
            self.state["budget_stopped"] = True
            self.state["budget_stopped_at"] = iso()
            healed = []
            for key in list(lane_by_name(self.lanes, "agent").get("defs", {})):
                definition_id = self._definition_id("agent", key)
                lane = lane_by_name(self.lanes, "agent")
                lane_cli(lane, ["schedule", "pause", definition_id], timeout=60, check=False)
                lane_cli(lane, ["schedule", "cancel", definition_id], timeout=60, check=False)
                healed.append(definition_id)
            self.recorder.event("budget_stopped", usage=self.state["budget_usage"], paused=healed)
            self.mark("budget", False, self.state["budget_usage"])
            self.persist()

    def finish(self, normal: bool, reason: str) -> int:
        self.state["status"] = "FINALIZING" if normal else self.state.get("status", "FAILED")
        self.state["phase"] = "finalize"
        self.state["stop_reason"] = reason
        self.state["quiesce_started_at"] = iso()
        self.persist()
        self.heartbeat()
        try:
            if self.lanes:
                self.state["quiesce"] = quiesce_run(
                    self.paths,
                    self.plan,
                    self.lanes,
                    self.recorder,
                    grace_secs=180 if self.mode == "dry-run" else 300,
                )
                self.persist()
            result = finalize_run(self.paths, self.plan, self.state, self.recorder, self.lanes, self.specs_by_lane, self.checks)
            return 0 if result.get("verdict") not in ("FAIL",) else 1
        except Exception as error:  # noqa: BLE001 - finalization errors are evidence
            self.recorder.log("finalize failed: %s\n%s" % (error, traceback.format_exc()))
            self.state["status"] = "FAILED"
            self.state["error"] = redact(str(error))
            self.persist()
            return 1


def _bump(counter: Dict[str, int], key: Any) -> None:
    name = str(key)
    counter[name] = counter.get(name, 0) + 1


def _expected_slots(spec: DefSpec, start_at: dt.datetime, deadline: dt.datetime) -> Optional[int]:
    trigger = list(spec.trigger)
    if "--at" in trigger:
        return 1
    if "--every" in trigger:
        try:
            interval = int(trigger[trigger.index("--every") + 1])
        except (ValueError, IndexError):
            return None
        if interval <= 0:
            return None
        remaining = (deadline - start_at).total_seconds()
        return 1 + int(remaining // interval)
    return None


def _max_wake_gap(wakeups: Sequence[Dict[str, Any]]) -> Optional[float]:
    times = sorted(parse_iso(row["started_at"]) for row in wakeups if row.get("started_at"))
    if len(times) < 2:
        return None
    return max((later - earlier).total_seconds() for earlier, later in zip(times, times[1:]))


def aggregate_run(
    paths: RunPaths,
    plan: Dict[str, Any],
    state: Dict[str, Any],
    lanes: Dict[str, Dict[str, Any]],
    specs_by_lane: Dict[str, List[DefSpec]],
) -> Dict[str, Any]:
    start_at = parse_iso(plan["start_at"])
    deadline = parse_iso(plan["deadline_at"])
    evidence = {
        "run_id": plan["run_id"],
        "mode": plan["mode"],
        "commit": plan["commit"],
        "binary": plan["binary"],
        "model": plan.get("model"),
        "start_at": plan["start_at"],
        "deadline_at": plan["deadline_at"],
        "observed_at": iso(),
        "budget": plan.get("budget"),
        "thresholds": plan.get("thresholds") or THRESHOLDS,
        "lanes": {},
        "totals": {},
    }
    total_states: Dict[str, int] = {}
    total_occurrence_states: Dict[str, int] = {}
    duplicates: List[Dict[str, Any]] = []
    stranded: List[Dict[str, Any]] = []
    unsettled: List[Dict[str, Any]] = []
    classified_without_run: List[Dict[str, Any]] = []
    expected_slots: Dict[str, Any] = {}
    claim_latencies: List[float] = []
    completion_latencies: List[float] = []
    handoff_latencies: List[float] = []
    session_gaps: List[float] = []
    usage = {
        "requests": 0,
        "input_tokens": 0,
        "cached_input_tokens": 0,
        "output_tokens": 0,
        "reasoning_tokens": 0,
        "cost_usd_micros": 0,
        "rows_without_cost": 0,
    }
    failures: List[Dict[str, Any]] = []
    now = time.time()
    quiesce_started = state.get("quiesce_started_at")
    quiesce_epoch = parse_iso(quiesce_started).timestamp() if quiesce_started else now
    for lane_name, lane in lanes.items():
        snapshot = read_schedule(Path(lane["home"]))
        definitions = {row["id"]: row for row in snapshot["definitions"]}
        lane_states: Dict[str, int] = {}
        lane_occurrence_states: Dict[str, int] = {}
        lane_effects = marker_counts(lane, specs_by_lane.get(lane_name, []))
        succeeded_by_definition: Dict[str, int] = {}
        for run in snapshot["runs"]:
            _bump(lane_states, run.get("state"))
            _bump(total_states, run.get("state"))
            definition = definitions.get(run.get("definition_id")) or {}
            if run.get("state") == "succeeded":
                _bump(succeeded_by_definition, run.get("definition_id"))
            if run.get("usage_requests") is not None:
                usage["requests"] += int(run.get("usage_requests") or 0)
                usage["input_tokens"] += int(run.get("input_tokens") or 0)
                usage["cached_input_tokens"] += int(run.get("cached_input_tokens") or 0)
                usage["output_tokens"] += int(run.get("output_tokens") or 0)
                usage["reasoning_tokens"] += int(run.get("reasoning_tokens") or 0)
                if run.get("cost_usd_micros") is None:
                    usage["rows_without_cost"] += 1
                else:
                    usage["cost_usd_micros"] += int(run.get("cost_usd_micros") or 0)
            if run.get("state") in ("dispatching", "running"):
                unsettled.append(
                    {
                        "lane": lane_name,
                        "run": run.get("id"),
                        "state": run.get("state"),
                        "started_at": run.get("started_at"),
                        "delivered_at": run.get("delivered_at"),
                        "definition": definition.get("name"),
                    }
                )
            if run.get("state") in ("failed", "unknown", "blocked", "waiting_approval", "cancelled"):
                failures.append(
                    {
                        "lane": lane_name,
                        "run": run.get("id"),
                        "definition": definition.get("name"),
                        "state": run.get("state"),
                        "error": redact(str(run.get("error"))) if run.get("error") else None,
                        "task_outcome": run.get("task_outcome"),
                        "native_outcome": run.get("native_outcome"),
                        "started_at": run.get("started_at"),
                        "finished_at": run.get("finished_at"),
                    }
                )
            if run.get("started_at"):
                started = parse_iso(run["started_at"])
                if run.get("scheduled_for"):
                    claim_latencies.append((started - parse_iso(run["scheduled_for"])).total_seconds())
                if run.get("finished_at"):
                    completion_latencies.append((parse_iso(run["finished_at"]) - started).total_seconds())
                if run.get("handed_off_at"):
                    handoff_latencies.append((parse_iso(run["handed_off_at"]) - started).total_seconds())
        for occurrence in snapshot["occurrences"]:
            _bump(lane_occurrence_states, occurrence.get("state"))
            _bump(total_occurrence_states, occurrence.get("state"))
            runs = occurrence.get("runs", [])
            if len(runs) > 1:
                duplicates.append(
                    {
                        "lane": lane_name,
                        "occurrence": occurrence.get("id"),
                        "definition": (definitions.get(occurrence.get("definition_id")) or {}).get("name"),
                        "run_count": len(runs),
                    }
                )
            if occurrence.get("state") in (
                "skipped",
                "missed",
                "interrupted",
                "blocked",
                "cancelled",
                "waiting_approval",
            ) and not runs:
                classified_without_run.append(
                    {
                        "lane": lane_name,
                        "occurrence": occurrence.get("id"),
                        "state": occurrence.get("state"),
                        "scheduled_for": occurrence.get("scheduled_for"),
                    }
                )
            if occurrence.get("state") == "completed" and not runs:
                stranded.append(
                    {
                        "lane": lane_name,
                        "occurrence": occurrence.get("id"),
                        "reason": "completed without run",
                    }
                )
            if occurrence.get("state") == "pending" and occurrence.get("scheduled_for"):
                scheduled = parse_iso(occurrence["scheduled_for"]).timestamp()
                overdue = now - scheduled
                if overdue > 300 and scheduled < quiesce_epoch - 300:
                    stranded.append(
                        {
                            "lane": lane_name,
                            "occurrence": occurrence.get("id"),
                            "reason": "pending overdue %.0fs" % overdue,
                            "scheduled_for": occurrence.get("scheduled_for"),
                        }
                    )
        for spec in specs_by_lane.get(lane_name, []):
            expected = _expected_slots(spec, start_at, deadline)
            effect = lane_effects.get(spec.key) or {}
            definition_id = definition_id_for(lane.get("defs"), spec.key)
            succeeded = effect_success_count(succeeded_by_definition, lane.get("defs"), spec.key)
            if expected is not None:
                expected_slots[lane_name + "." + spec.key] = {
                    "expected_grid_slots": expected,
                    "materialized_occurrences": sum(
                        1
                        for occurrence in snapshot["occurrences"]
                        if occurrence.get("definition_id") == definition_id
                    ),
                    "note": "materialized occurrences are authoritative; grid slots may be coalesced",
                }
            if spec.kind == "shell" and spec.marker:
                expected_effects = succeeded
                unknown = lane_states.get("unknown", 0)
                ok = effect.get("lines", 0) == expected_effects or (
                    lane_name == "fault" and expected_effects <= effect.get("lines", 0) <= expected_effects + unknown
                )
                effect.update({"succeeded_runs": succeeded, "effect_matches_success": ok})
        evidence["lanes"][lane_name] = {
            "home": lane["home"],
            "repo": lane["repo"],
            "binary": lane.get("binary_identity") or binary_identity(Path(lane["binary"])),
            "definitions": [
                {
                    "id": row.get("id"),
                    "name": row.get("name"),
                    "action": row.get("action"),
                    "trigger": row.get("trigger"),
                    "enabled": row.get("enabled"),
                    "overlap": row.get("overlap"),
                    "misfire": row.get("misfire"),
                    "occurrence_count": row.get("occurrence_count"),
                    "last_run": row.get("last_run"),
                }
                for row in snapshot["definitions"]
            ],
            "run_states": lane_states,
            "occurrence_states": lane_occurrence_states,
            "effects": lane_effects,
            "wakeups": {
                "count": len(snapshot["wakeups"]),
                "sources": {
                    source: sum(1 for row in snapshot["wakeups"] if row.get("source") == source)
                    for source in sorted({row.get("source") for row in snapshot["wakeups"]})
                },
                "max_gap_secs": _max_wake_gap(snapshot["wakeups"]),
            },
            "database_bytes": Path(snapshot["database"]).stat().st_size if Path(snapshot["database"]).is_file() else 0,
        }
        sessions = read_agent_sessions(Path(lane["home"]), Path(lane["repo"]))
        if sessions["sessions"]:
            evidence["lanes"][lane_name]["sessions"] = sessions["sessions"]
        for session in sessions["sessions"]:
            if session.get("created_at") and session.get("updated_at"):
                session_gaps.append(
                    (parse_iso(session["updated_at"]) - parse_iso(session["created_at"])).total_seconds()
                )
    evidence["totals"] = {
        "definitions": sum(len(row["definitions"]) for row in evidence["lanes"].values()),
        "occurrences": sum(len(read_schedule(Path(lane["home"]))["occurrences"]) for lane in lanes.values()),
        "runs": sum(total_states.values()),
        "run_states": total_states,
        "occurrence_states": total_occurrence_states,
    }
    evidence["duplicates"] = duplicates
    evidence["stranded"] = stranded
    evidence["unsettled"] = unsettled
    evidence["classified_without_run"] = classified_without_run
    evidence["expected_slots"] = expected_slots
    evidence["failures"] = failures
    evidence["latency_secs"] = {
        "claim": latency_stats(claim_latencies),
        "completion": latency_stats(completion_latencies),
        "handoff": latency_stats(handoff_latencies),
    }
    evidence["usage"] = usage
    evidence["usage"]["cost_usd"] = round(usage["cost_usd_micros"] / 1_000_000.0, 6)
    evidence["samples"] = read_jsonl(paths.samples)
    return evidence


def read_jsonl(path: Path) -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    try:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    value = json.loads(line)
                except ValueError:
                    continue
                if isinstance(value, dict):
                    rows.append(value)
    except OSError:
        pass
    return rows


def resource_summary(samples: Sequence[Dict[str, Any]]) -> Dict[str, Any]:
    rss = [float(row.get("rss_kb") or 0) for row in samples if row.get("rss_kb")]
    cpu = [float(row.get("cpu_pct") or 0) for row in samples if row.get("cpu_pct") is not None]
    db_bytes = [float(row.get("db_bytes") or 0) for row in samples if row.get("db_bytes") is not None]
    live_bytes = [float(row.get("live_root_bytes") or 0) for row in samples if row.get("live_root_bytes") is not None]
    processes = [int(row.get("processes") or 0) for row in samples]

    def growth(values: List[float]) -> float:
        if len(values) < 2:
            return 0.0
        return max(0.0, max(values) - min(values))

    return {
        "sample_count": len(samples),
        "rss_kb": latency_stats(rss),
        "rss_kb_growth": growth(rss),
        "cpu_pct": latency_stats(cpu),
        "db_bytes_growth": growth(db_bytes),
        "live_root_bytes_growth": growth(live_bytes),
        "process_count_max": max(processes) if processes else 0,
        "process_count_last": processes[-1] if processes else 0,
    }


def derive_checks(
    evidence: Dict[str, Any],
    checks: Dict[str, Any],
    plan: Dict[str, Any],
    cleanup: Dict[str, Any],
) -> Dict[str, Any]:
    totals = evidence["totals"]
    states = totals.get("run_states") or {}
    occurrence_states = totals.get("occurrence_states") or {}
    thresholds = plan.get("thresholds") or THRESHOLDS
    duplicates = evidence.get("duplicates") or []
    stranded = evidence.get("stranded") or []
    unsettled = evidence.get("unsettled") or []
    failures = evidence.get("failures") or []
    usage = evidence.get("usage") or {}
    latency = evidence.get("latency_secs") or {}
    resources = resource_summary(evidence.get("samples") or [])

    swallowed = silent_losses(evidence)
    duplicate_count = duplicate_side_effect_count(evidence, duplicates)
    occurrence_ok = (
        duplicate_count <= int(thresholds["max_duplicate_side_effects"])
        and len(stranded) <= int(thresholds["max_unexplained_stranded_occurrences"])
        and not unsettled
        and not swallowed
    )
    checks["S1_occurrences"] = {
        "passed": occurrence_ok,
        "detail": {
            "duplicates": duplicates,
            "duplicate_side_effects": duplicate_count,
            "stranded": stranded,
            "unsettled": unsettled,
            "silent_losses": swallowed,
            "run_states": states,
            "occurrence_states": occurrence_states,
        },
    }
    claim_p95 = (latency.get("claim") or {}).get("p95")
    completion_p95 = (latency.get("completion") or {}).get("p95")
    wake_gap = max_observed_wake_gap(evidence)
    rss_growth_bytes = float(resources.get("rss_kb_growth") or 0) * 1024
    performance_ok = (
        resources["sample_count"] >= 2
        and (claim_p95 is None or claim_p95 <= thresholds["max_claim_latency_p95_secs"])
        and (completion_p95 is None or completion_p95 <= thresholds["max_completion_latency_p95_secs"])
        and resources["db_bytes_growth"] <= thresholds["max_disk_growth_bytes"]
        and resources["live_root_bytes_growth"] <= thresholds["max_disk_growth_bytes"]
        and rss_growth_bytes <= thresholds["max_rss_growth_bytes"]
        and wake_gap is not None
        and wake_gap <= thresholds["max_unexplained_wake_gap_secs"]
    )
    checks["S10_performance"] = {
        "passed": performance_ok,
        "detail": {
            "latency": latency,
            "resources": resources,
            "rss_growth_bytes": rss_growth_bytes,
            "wake_gap_secs": wake_gap,
        },
    }
    agent_runs = [
        row
        for lane_name, lane in plan.get("lanes", {}).items()
        if lane_name == "agent"
        for row in lane
        if row.get("kind") == "agent"
    ]
    agent_failures = [row for row in failures if row.get("lane") == "agent"]
    success_ratio = agent_success_ratio(evidence)
    agent_ok = (
        states.get("succeeded", 0) >= 1
        and not agent_failures
        and usage.get("rows_without_cost", 0) <= thresholds["max_agent_usage_missing"]
        and len(agent_runs) >= 1
        and success_ratio is not None
        and success_ratio >= float(thresholds["min_agent_success_ratio"])
    )
    checks["S2_agent"] = {
        "passed": agent_ok,
        "detail": {
            "succeeded_total": states.get("succeeded", 0),
            "agent_failures": agent_failures,
            "agent_success_ratio": success_ratio,
            "usage": usage,
            "definitions": len(agent_runs),
        },
    }
    shell_failures = product_shell_failures(failures)
    checks["S_shell_outcomes"] = {
        "passed": not shell_failures,
        "detail": {"failures": shell_failures},
    }
    mismatches = effect_mismatches(evidence)
    checks["S_effect_match"] = {
        "passed": not mismatches,
        "detail": {"mismatches": mismatches},
    }
    os_succeeded = 0
    agent_succeeded = 0
    for lane_name, lane in evidence["lanes"].items():
        if lane_name == "os":
            os_succeeded = int((lane.get("run_states") or {}).get("succeeded", 0))
        if lane_name == "agent":
            agent_succeeded = int((lane.get("run_states") or {}).get("succeeded", 0))
    mixed_ok = os_succeeded >= 1 and agent_succeeded >= 1 and os_succeeded >= 10 * max(1, agent_succeeded)
    if plan["mode"] == "dry-run":
        mixed_ok = os_succeeded >= 1
    checks["S3_mixed"] = {
        "passed": mixed_ok,
        "detail": {"os_succeeded": os_succeeded, "agent_succeeded": agent_succeeded},
        "note": "agent frequency is deliberately far below shell frequency",
    }
    residue = residue_from_cleanup(cleanup)
    orphan_processes = len(residue.get("processes") or [])
    orphan_registrations = len(residue.get("labels") or []) + len(residue.get("plists") or [])
    residue_ok = (
        orphan_processes <= int(thresholds["max_orphan_processes_after_cleanup"])
        and orphan_registrations <= int(thresholds["max_orphan_registrations_after_cleanup"])
        and not residue.get("missing_user_labels")
    )
    checks["S9_residue"] = {
        "passed": residue_ok,
        "detail": {
            **residue,
            "orphan_processes": orphan_processes,
            "orphan_registrations": orphan_registrations,
        },
    }
    checks["S7_cross_revision"] = {
        "passed": None,
        "status": "NOT_VERIFIED",
        "detail": "no second clean revision binary was built; only same-artifact replacement is exercised",
    }
    checks["S8_tui_desktop"] = {
        "passed": None,
        "status": "NOT_VERIFIED",
        "detail": "CLI compared against the real store; TUI/Desktop/agent-tool surfaces were not driven in this run",
    }
    checks["external_acceptance"] = {
        "passed": None,
        "status": "NOT_VERIFIED",
        "detail": [
            "macOS real sleep/wake, lock/logout/login",
            "Apple Mail launchd TCC and controlled real email",
            "Windows GUI/CUA/Outlook COM and real LogonTrigger",
            "Linux OS wakeup (Unsupported)",
        ],
    }
    checks["budget"] = {
        "passed": not plan.get("budget_stopped", False),
        "detail": plan.get("budget_usage") or {},
    }
    return checks


def required_checks(mode: str) -> List[str]:
    if mode == "dry-run":
        return [
            "S1_rules",
            "S1_pause_resume",
            "S1_occurrences",
            "S_shell_outcomes",
            "S_effect_match",
            "S2_agent",
            "S2_agent_verify",
            "S3_mixed",
            "S4_1",
            "S4_allow_execution_1",
            "S5_dispatcher_1",
            "S5_runtime_1",
            "S6_misfire",
            "S7_replacement",
            "S7_continuity",
            "S8_s8_cli",
            "S9_install_uninstall",
            "S9_registration",
            "S9_residue",
            "S10_performance",
            "S11_on_demand",
        ]
    required = [
        "S1_rules",
        "S1_pause_resume",
        "S1_occurrences",
        "S_shell_outcomes",
        "S_effect_match",
        "S2_agent",
        "S3_mixed",
        "S4_1",
        "S4_2",
        "S4_3",
        "S4_allow_execution_1",
        "S4_allow_execution_2",
        "S4_allow_execution_3",
        "S5_dispatcher_1",
        "S5_dispatcher_2",
        "S5_runtime_1",
        "S5_runtime_2",
        "S6_misfire",
        "S7_replacement",
        "S7_continuity",
        "S8_s8_cli_1",
        "S8_s8_cli_2",
        "S9_install_uninstall",
        "S9_registration",
        "S9_residue",
        "S10_performance",
        "S11_on_demand",
        "S2_agent_verify_1",
        "S2_agent_verify_2",
        "S2_agent_verify_3",
        "S2_agent_verify_4",
        "S2_agent_verify_5",
    ]
    return required


def compute_verdict(plan: Dict[str, Any], state: Dict[str, Any], checks: Dict[str, Any]) -> Tuple[str, List[str]]:
    reasons: List[str] = []
    if state.get("budget_stopped"):
        return "BUDGET_STOPPED", ["agent budget reached; agent definitions were paused and active runs cancelled"]
    if state.get("stop_reason") == "signal":
        return "STOPPED", ["the runner received a stop signal before the planned deadline"]
    missing = []
    failed = []
    for name in required_checks(plan["mode"]):
        row = checks.get(name)
        if not row:
            missing.append(name)
            continue
        if row.get("passed") is not True:
            failed.append(name)
    if missing:
        reasons.append("missing checks: " + ", ".join(missing))
    if failed:
        reasons.append("failed checks: " + ", ".join(failed))
    runner_errors = state.get("runner_errors") or []
    if runner_errors:
        reasons.append(
            "runner errors: " + ", ".join(str(row.get("name") or "runner") for row in runner_errors)
        )
    if plan["mode"] != "dry-run":
        observed = state.get("duration_secs")
        planned = int(plan.get("duration_secs") or 0)
        if observed is not None and observed < planned - 120:
            reasons.append("soak ended %.0fs before the planned deadline" % (planned - float(observed)))
    if reasons:
        return "FAIL", reasons
    return ("PASS" if plan["mode"] == "dry-run" else "PASS"), ["all required checks passed"]


def archive_lane_data(paths: RunPaths, lane: Dict[str, Any]) -> Dict[str, Any]:
    target = Path(paths.evidence) / "archive" / lane["name"]
    target.mkdir(parents=True, exist_ok=True)
    copied: List[str] = []
    database = schedule_database(Path(lane["home"]))
    if database.is_file():
        destination = target / "schedules.db"
        with contextlib.closing(sqlite3.connect(str(database), timeout=10.0)) as source, contextlib.closing(
            sqlite3.connect(str(destination))
        ) as sink:
            source.backup(sink)
        copied.append(str(destination))
    for database_path in find_session_databases(Path(lane["home"])):
        destination = target / ("session-%s.db" % database_path.parent.name)
        try:
            with contextlib.closing(sqlite3.connect(str(database_path), timeout=10.0)) as source, contextlib.closing(
                sqlite3.connect(str(destination))
            ) as sink:
                source.backup(sink)
            copied.append(str(destination))
        except sqlite3.Error:
            continue
    logs = Path(lane["home"]) / "logs"
    if logs.is_dir():
        for source in logs.rglob("*.log"):
            destination = target / "logs" / source.relative_to(logs)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
            copied.append(str(destination))
    effects = Path(lane["repo"]) / "effects"
    if effects.is_dir():
        for source in effects.rglob("*"):
            if source.is_file():
                destination = target / "effects" / source.relative_to(effects)
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
                copied.append(str(destination))
    for name in ("agent-report.md", "agent-fresh.md", "agent-history.md"):
        source = Path(lane["repo"]) / name
        if source.is_file():
            destination = target / name
            shutil.copyfile(source, destination)
            copied.append(str(destination))
    return {"target": str(target), "files": copied}


def cleanup_run(
    paths: RunPaths,
    plan: Dict[str, Any],
    lanes: Dict[str, Dict[str, Any]],
    recorder: Recorder,
) -> Dict[str, Any]:
    baseline_labels = set(plan.get("baseline_labels") or [])
    own_labels = {lane.get("label") or label_for(Path(lane["home"])) for lane in lanes.values()}
    disabled: Dict[str, Any] = {}
    for lane in lanes.values():
        status = registration_status(lane)
        if status["plist_exists"] or status["loaded"]:
            try:
                disabled[lane["name"]] = disable_lane(lane, recorder)
            except Exception as error:  # noqa: BLE001 - the fallback must own this failure
                message = redact(str(error))
                recorder.event(
                    "launchd_disable_failed", lane=lane["name"], error=message
                )
                disabled[lane["name"]] = unregister_lane_fallback(
                    lane, Path(plan["live_root"]), recorder
                )
    fault = lanes.get("fault")
    if fault and fault.get("serve_pid"):
        with contextlib.suppress(Exception):
            stop_serve(fault, recorder)
    deadline = time.time() + 8
    while time.time() < deadline:
        rows = [
            row
            for row in matching_processes(Path(plan["live_root"]))
            if str(plan["live_root"]) in row["command"] and "schedule_soak.py" not in row["command"]
        ]
        if not rows:
            break
        for row in rows:
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.kill(row["pid"], signal.SIGTERM)
        time.sleep(1)
    rows = [
        row
        for row in matching_processes(Path(plan["live_root"]))
        if str(plan["live_root"]) in row["command"] and "schedule_soak.py" not in row["command"]
    ]
    for row in rows:
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.kill(row["pid"], signal.SIGKILL)
    time.sleep(0.5)
    processes = [
        {"pid": row["pid"], "command": row["command"][:300]}
        for row in matching_processes(Path(plan["live_root"]))
        if str(plan["live_root"]) in row["command"] and "schedule_soak.py" not in row["command"]
    ]
    labels_now = set(launchd_labels())
    leftover_labels = sorted(labels_now & own_labels)
    leftover_plists = [str(plist_for(label)) for label in own_labels if plist_for(label).is_file()]
    unexpected_labels = sorted(labels_now - baseline_labels - own_labels)
    missing_user_labels = sorted(baseline_labels - labels_now)
    archives = {name: archive_lane_data(paths, lane) for name, lane in lanes.items()}
    credentials_removed: List[str] = []
    for lane in lanes.values():
        config_file = Path(lane["home"]) / "config.toml"
        if config_file.is_file():
            with contextlib.suppress(OSError):
                config_file.unlink()
                credentials_removed.append(str(config_file))
    residue = {
        "processes": processes,
        "labels": leftover_labels,
        "plists": leftover_plists,
        "unexpected_user_labels": unexpected_labels,
        "missing_user_labels": missing_user_labels,
        "credentials_removed": credentials_removed,
        "disabled": disabled,
        "archives": archives,
        "live_root_retained": bool(processes or leftover_labels or leftover_plists),
    }
    if not residue["live_root_retained"]:
        shutil.rmtree(plan["live_root"], ignore_errors=True)
    recorder.event("cleanup", **residue)
    return residue


def render_report(report: Dict[str, Any]) -> str:
    lines: List[str] = []
    verdict = report.get("verdict", "INCOMPLETE")
    lines.append("# Phase 6.4 soak report — %s" % report.get("run_id"))
    lines.append("")
    lines.append("**Verdict: %s**" % verdict)
    if report.get("verdict_reasons"):
        for reason in report["verdict_reasons"]:
            lines.append("- %s" % reason)
    lines.append("")
    lines.append("## Identity")
    lines.append("")
    lines.append("| Field | Value |")
    lines.append("| --- | --- |")
    for key in (
        "run_id",
        "mode",
        "commit",
        "start_at",
        "deadline_at",
        "finished_at",
        "stop_reason",
        "duration_secs",
        "runner_pid",
    ):
        lines.append("| %s | %s |" % (key, report.get(key)))
    binary = report.get("binary") or {}
    lines.append("| binary | `%s` |" % binary.get("path"))
    lines.append("| binary sha256 | `%s` |" % binary.get("sha256"))
    lines.append("| binary version | `%s` |" % binary.get("version"))
    lines.append("| model | `%s` |" % report.get("model"))
    lines.append("")
    lines.append("## Matrix")
    lines.append("")
    lines.append("| Check | Result | Detail |")
    lines.append("| --- | --- | --- |")
    checks = report.get("checks") or {}
    for name in sorted(checks):
        row = checks[name]
        status = row.get("status") or ("PASS" if row.get("passed") is True else "FAIL" if row.get("passed") is False else "NOT_VERIFIED")
        detail = json.dumps(row.get("detail") or {}, ensure_ascii=False, sort_keys=True)
        if len(detail) > 500:
            detail = detail[:500] + "…"
        lines.append("| %s | %s | `%s` |" % (name, status, detail.replace("|", "\\|")))
    lines.append("")
    lines.append("## Product findings")
    lines.append("")
    findings = report.get("product_findings") or []
    if findings:
        for row in findings:
            lines.append("- **%s** — %s" % (row.get("check"), row.get("status")))
            lines.append("  `%s`" % json.dumps(row.get("detail") or {}, ensure_ascii=False, sort_keys=True)[:1200])
    else:
        lines.append("- none recorded")
    lines.append("")
    lines.append("## Coverage gaps")
    lines.append("")
    gaps = report.get("coverage_gaps") or []
    if gaps:
        for row in gaps:
            lines.append("- **%s** — %s" % (row.get("check"), json.dumps(row.get("detail"), ensure_ascii=False)[:800]))
    else:
        lines.append("- none recorded")
    lines.append("")
    totals = report.get("totals") or {}
    lines.append("## Counts")
    lines.append("")
    lines.append("| Metric | Value |")
    lines.append("| --- | --- |")
    lines.append("| definitions | %s |" % totals.get("definitions"))
    lines.append("| occurrences | %s |" % totals.get("occurrences"))
    lines.append("| runs | %s |" % totals.get("runs"))
    for key, value in sorted((totals.get("run_states") or {}).items()):
        lines.append("| run state `%s` | %s |" % (key, value))
    for key, value in sorted((totals.get("occurrence_states") or {}).items()):
        lines.append("| occurrence state `%s` | %s |" % (key, value))
    lines.append("| duplicate occurrences | %s |" % len(report.get("duplicates") or []))
    lines.append("| stranded occurrences | %s |" % len(report.get("stranded") or []))
    lines.append("| unsettled runs | %s |" % len(report.get("unsettled") or []))
    lines.append("")
    lines.append("## Latency")
    lines.append("")
    lines.append("| Phase | count | P50 | P95 | P99 | max |")
    lines.append("| --- | --- | --- | --- | --- | --- |")
    for key in ("claim", "completion", "handoff"):
        row = (report.get("latency_secs") or {}).get(key) or {}
        lines.append(
            "| %s | %s | %s | %s | %s | %s |"
            % (key, row.get("count"), row.get("p50"), row.get("p95"), row.get("p99"), row.get("max"))
        )
    lines.append("")
    resources = report.get("resources") or {}
    lines.append("## Resources")
    lines.append("")
    lines.append("| Metric | Value |")
    lines.append("| --- | --- |")
    lines.append("| samples | %s |" % resources.get("sample_count"))
    lines.append("| RSS max KB | %s |" % (resources.get("rss_kb") or {}).get("max"))
    lines.append("| CPU P95 %% | %s |" % (resources.get("cpu_pct") or {}).get("p95"))
    lines.append("| process max | %s |" % resources.get("process_count_max"))
    lines.append("| DB growth bytes | %s |" % resources.get("db_bytes_growth"))
    lines.append("| live root growth bytes | %s |" % resources.get("live_root_bytes_growth"))
    lines.append("")
    usage = report.get("usage") or {}
    lines.append("## Usage")
    lines.append("")
    lines.append("| Metric | Value |")
    lines.append("| --- | --- |")
    for key in ("requests", "input_tokens", "cached_input_tokens", "output_tokens", "reasoning_tokens", "cost_usd", "rows_without_cost"):
        lines.append("| %s | %s |" % (key, usage.get(key)))
    lines.append("")
    failures = report.get("failures") or []
    lines.append("## Failures and unknowns")
    lines.append("")
    if failures:
        lines.append("| Lane | Definition | State | Error |")
        lines.append("| --- | --- | --- | --- |")
        for row in failures:
            lines.append(
                "| %s | %s | %s | `%s` |"
                % (row.get("lane"), row.get("definition"), row.get("state"), (row.get("error") or "")[:300].replace("|", "\\|"))
            )
    else:
        lines.append("None recorded.")
    lines.append("")
    cleanup = report.get("cleanup") or {}
    residue = cleanup.get("residue") or cleanup
    lines.append("## Cleanup and residue")
    lines.append("")
    lines.append("| Item | Value |")
    lines.append("| --- | --- |")
    lines.append("| leftover labels | %s |" % (residue.get("labels") or []))
    lines.append("| leftover plists | %s |" % (residue.get("plists") or []))
    lines.append("| leftover processes | %s |" % (residue.get("processes") or []))
    lines.append("| unexpected user labels | %s |" % (residue.get("unexpected_user_labels") or []))
    lines.append("| live root retained | %s |" % residue.get("live_root_retained"))
    lines.append("")
    lines.append("## Limits")
    lines.append("")
    for item in (
        "This is a single-host macOS run. It does not claim GitHub Actions native 3-OS CI.",
        "macOS sleep/wake, lock/logout, Apple Mail TCC, controlled real email, Windows GUI/CUA/Outlook COM and Linux OS wakeup remain NOT_VERIFIED or Unsupported.",
        "Cross-revision binary upgrade is NOT_VERIFIED; only same-artifact replacement was exercised.",
        "TUI, Desktop and agent-tool surfaces were not driven against the same schedule ID; only CLI versus the store is checked.",
        "A 24 h pass does not imply a 72 h or 7-day pass.",
    ):
        lines.append("- %s" % item)
    lines.append("")
    return "\n".join(lines)


def active_run_rows(lanes: Dict[str, Dict[str, Any]]) -> List[Dict[str, Any]]:
    rows: List[Dict[str, Any]] = []
    for lane_name, lane in lanes.items():
        for run in active_runs(lane):
            rows.append(
                {
                    "lane": lane_name,
                    "run": run.get("id"),
                    "definition": (run.get("definition") or {}).get("id"),
                    "definition_name": (run.get("definition") or {}).get("name"),
                    "state": run.get("state"),
                    "started_at": run.get("started_at"),
                }
            )
    return rows


def lanes_with_active_runs(rows: Sequence[Dict[str, Any]]) -> List[str]:
    """Unique lanes owning an unsettled run, in first-seen order."""
    names: List[str] = []
    for row in rows:
        name = row.get("lane")
        if name and name not in names:
            names.append(name)
    return names


def recover_active_runs(
    lanes: Dict[str, Dict[str, Any]],
    recorder: Recorder,
) -> List[Dict[str, Any]]:
    """Let the product's dispatcher recover expired leases after quiesce.

    Definitions are paused and OS registrations disabled before this runs, so a
    bounded pass can only reconcile claims whose lease already expired. The
    store, not this runner, decides the terminal state.
    """
    names = lanes_with_active_runs(active_run_rows(lanes))
    results: List[Dict[str, Any]] = []
    for name in names:
        lane = lane_by_name(lanes, name)
        row: Dict[str, Any] = {"lane": name, "started_at": iso()}
        try:
            row["summary"] = lane_json(
                lane,
                ["schedule", "dispatch", "--source", "manual"],
                timeout=120,
            )
        except Exception as error:  # noqa: BLE001 - recorded, never hidden
            row["error"] = redact(str(error))
            recorder.event("recovery_pass_failed", lane=name, error=row["error"])
        results.append(row)
    if results:
        recorder.event("recovery_passes", results=results)
    return results


def quiesce_run(
    paths: RunPaths,
    plan: Dict[str, Any],
    lanes: Dict[str, Dict[str, Any]],
    recorder: Recorder,
    grace_secs: float,
) -> Dict[str, Any]:
    started = time.time()
    paused: List[str] = []
    for lane in lanes.values():
        for key, definition in (lane.get("defs") or {}).items():
            definition_id = definition.get("id")
            if not definition_id:
                continue
            with contextlib.suppress(Exception):
                lane_cli(lane, ["schedule", "pause", str(definition_id)], timeout=30, check=False)
                paused.append(str(definition_id))
    # In-flight runs are observed by the dispatch process that claimed them.
    # Disabling the OS job first would kill that observer while the shell
    # child keeps running, leaving the run row `running` forever.  Wait for
    # settlement while the registration is still live, and only disable it
    # once nothing is left or the grace period has expired.
    before = active_run_rows(lanes)
    deadline = time.time() + grace_secs
    while time.time() < deadline and active_run_rows(lanes):
        time.sleep(2)
    remaining = active_run_rows(lanes)
    cancelled: List[str] = []
    if remaining:
        seen = set()
        for row in remaining:
            definition_id = row.get("definition")
            if not definition_id or definition_id in seen:
                continue
            seen.add(definition_id)
            lane = lane_by_name(lanes, row["lane"])
            with contextlib.suppress(Exception):
                lane_cli(lane, ["schedule", "cancel", str(definition_id)], timeout=30, check=False)
                cancelled.append(str(definition_id))
        stop_at = time.time() + 60
        while time.time() < stop_at and active_run_rows(lanes):
            time.sleep(2)
    disabled: List[str] = []
    for lane in lanes.values():
        status = registration_status(lane)
        if status["plist_exists"] or status["loaded"]:
            with contextlib.suppress(Exception):
                disable_lane(lane, recorder)
                disabled.append(lane["name"])
    fault = lanes.get("fault")
    if fault and fault.get("serve_pid"):
        with contextlib.suppress(Exception):
            stop_serve(fault, recorder)
    # A dispatcher killed before its lease expired leaves the run row
    # `running` with nobody left to reconcile it.  The product's own dispatch
    # pass is the authority for that transition; definitions are already
    # paused, so it can only recover expired claims.
    recovery_passes = recover_active_runs(lanes, recorder)
    result = {
        "started_at": iso(dt.datetime.fromtimestamp(started, tz=dt.timezone.utc)),
        "paused": paused,
        "disabled": disabled,
        "active_before": before,
        "cancelled": cancelled,
        "recovery_passes": recovery_passes,
        "active_after": active_run_rows(lanes),
        "waited_secs": round(time.time() - started, 1),
    }
    recorder.event("quiesced", **result)
    return result


def finalize_run(
    paths: RunPaths,
    plan: Dict[str, Any],
    state: Dict[str, Any],
    recorder: Recorder,
    lanes: Dict[str, Dict[str, Any]],
    specs_by_lane: Dict[str, List[DefSpec]],
    checks: Dict[str, Any],
) -> Dict[str, Any]:
    started = time.time()
    state["phase"] = "collecting"
    recorder.state(state)
    with contextlib.suppress(Exception):
        emit_run_records(paths, recorder, lanes, specs_by_lane, load_seen_runs(paths), plan)
    evidence = aggregate_run(paths, plan, state, lanes, specs_by_lane)
    state["duration_secs"] = started - parse_iso(plan["start_at"]).timestamp()
    state["observed_duration_secs"] = state["duration_secs"]
    cleanup = cleanup_run(paths, plan, lanes, recorder)
    evidence["cleanup"] = {"residue": cleanup}
    plan["budget_stopped"] = bool(state.get("budget_stopped"))
    plan["budget_usage"] = state.get("budget_usage") or {}
    checks = derive_checks(evidence, checks, plan, cleanup)
    required = set(required_checks(plan.get("mode") or "soak"))
    findings: List[Dict[str, Any]] = []
    gaps = [
        {"check": name, "detail": (row.get("detail") if isinstance(row, dict) else row)}
        for name, row in checks.items()
        if name in ("S7_cross_revision", "S8_tui_desktop", "external_acceptance")
        and row.get("passed") is not True
    ]
    evidence["product_findings"] = findings
    evidence["coverage_gaps"] = gaps
    verdict, reasons = compute_verdict(plan, state, checks)
    # A finalized report must carry its own outcome, not the transient
    # FINALIZING status the supervisor held while collecting evidence.
    status = "COMPLETED" if verdict in ("PASS", "FAIL") else verdict
    evidence.update(
        {
            "finished_at": iso(),
            "runner_pid": state.get("runner_pid"),
            "status": status,
            "checks": checks,
            "verdict": verdict,
            "verdict_reasons": reasons,
            "stop_reason": state.get("stop_reason"),
            "duration_secs": state.get("duration_secs"),
            "resources": resource_summary(evidence.get("samples") or []),
        }
    )
    atomic_write_json(paths.report_json, evidence)
    paths.report_md.write_text(render_report(evidence), encoding="utf-8")
    state["status"] = status
    state["verdict"] = verdict
    state["finished_at"] = evidence["finished_at"]
    state["phase"] = "complete"
    state["report"] = str(paths.report_json)
    state["report_markdown"] = str(paths.report_md)
    recorder.state(state)
    recorder.event("finalized", verdict=verdict, reasons=reasons)
    return evidence


def active_evidence_runs(root: Optional[Path] = None) -> List[Dict[str, Any]]:
    base = root or DEFAULT_EVIDENCE_ROOT
    active: List[Dict[str, Any]] = []
    if not base.is_dir():
        return active
    for plan_path in sorted(base.glob("*/plan.json")):
        state_path = plan_path.parent / "state.json"
        state = read_json(state_path, {}) or {}
        if state.get("status") not in ("RUNNING", "DRY_RUNNING", "STARTING", "FINALIZING"):
            continue
        pid = int(state.get("runner_pid") or 0)
        active.append(
            {
                "evidence": str(plan_path.parent),
                "run_id": (read_json(plan_path, {}) or {}).get("run_id"),
                "status": state.get("status"),
                "runner_pid": pid,
                "runner_alive": pid_alive(pid),
                "runner_owned": runner_owned(pid, plan_path.parent),
                "heartbeat_age_secs": heartbeat_age(plan_path.parent),
            }
        )
    return active


def heartbeat_age(evidence: Path) -> Optional[float]:
    heartbeat = read_json(evidence / "heartbeat.json")
    if not heartbeat or not heartbeat.get("ts"):
        return None
    try:
        return max(0.0, time.time() - parse_iso(str(heartbeat["ts"])).timestamp())
    except (TypeError, ValueError):
        return None


def runner_owned(pid: int, evidence: Path) -> bool:
    if not pid_alive(pid):
        return False
    command = pid_cmdline(pid)
    return (
        "schedule_soak.py" in command
        and "supervise" in command
        and str(evidence.resolve()) in command
    )


def preflight(
    binary_path: Optional[Path] = None,
    commit: Optional[str] = None,
    source_tree: Optional[Path] = None,
) -> Dict[str, Any]:
    binary_path = Path(binary_path or DEFAULT_BINARY)
    commit = commit or DEFAULT_COMMIT
    source_tree = Path(source_tree or DEFAULT_SOURCE_TREE)
    blockers: List[str] = []
    warnings: List[str] = []
    host = {
        "platform": sys.platform,
        "release": os.uname().release if hasattr(os, "uname") else None,
        "machine": os.uname().machine if hasattr(os, "uname") else None,
        "python": sys.version.split()[0],
    }
    if sys.platform != "darwin":
        blockers.append("this plan expects macOS launchd; host is %s" % sys.platform)
    tools = {}
    for tool in ("launchctl", "git", "ps"):
        path = shutil.which(tool)
        tools[tool] = path
        if not path:
            blockers.append("missing tool: %s" % tool)
    binaries = {}
    for label, path in (("candidate", binary_path),):
        try:
            identity = binary_identity(path)
            identity["matches_commit"] = bool(
                identity.get("revision") and commit.startswith(str(identity["revision"]))
            )
            identity["supports_schedule"] = run_command([str(path), "schedule", "--help"], timeout=20).returncode == 0
            binaries[label] = identity
            if not identity["clean"]:
                blockers.append("%s binary is dirty or untrusted: %s" % (label, identity["version"]))
            if not identity["matches_commit"]:
                blockers.append(
                    "%s binary revision %s does not match expected commit %s"
                    % (label, identity.get("revision"), commit[:12])
                )
            if not identity["supports_schedule"]:
                blockers.append("%s binary has no schedule command" % label)
        except SoakError as error:
            blockers.append(str(error))
    source = {"path": str(source_tree), "exists": source_tree.is_dir(), "head": None, "clean": None}
    if source_tree.is_dir():
        head = run_command(["git", "-C", str(source_tree), "rev-parse", "HEAD"], timeout=20)
        status = run_command(["git", "-C", str(source_tree), "status", "--porcelain"], timeout=20)
        source["head"] = (head.stdout or "").strip() or None
        source["clean"] = status.returncode == 0 and not (status.stdout or "").strip()
        if source["head"] != commit:
            blockers.append("clean source HEAD %s is not %s" % (source["head"], commit))
        if not source["clean"]:
            blockers.append("clean source tree has local changes")
    else:
        warnings.append("clean source tree %s is unavailable; binary identity is still checked" % source_tree)
    config = config_info()
    if not config["exists"]:
        warnings.append("no ~/.leveler/config.toml; agent lanes may be unable to run")
    elif not config.get("inline_key_present") and not any((config.get("env_key_available") or {}).values()):
        warnings.append("no inline or environment provider key found; agent lanes will fail without credentials")
    if Path(os.environ.get("LEVELER_HOME", "")).resolve() == (Path.home() / ".leveler").resolve():
        blockers.append("LEVELER_HOME points at the user's real home")
    live_root = Path("/tmp")
    usage = shutil.disk_usage(live_root)
    if usage.free < DISK_FLOOR_BYTES:
        blockers.append("less than %d GiB free under %s" % (DISK_FLOOR_BYTES // (1024**3), live_root))
    active = [row for row in active_evidence_runs() if row.get("runner_alive")]
    for row in active:
        blockers.append(
            "another soak runner is active: %s (pid %s, %s)"
            % (row["evidence"], row["runner_pid"], row["status"])
        )
    labels = launchd_labels()
    user_definitions = None
    real_home = Path.home() / ".leveler"
    real_db = schedule_database(real_home)
    if real_db.is_file():
        with contextlib.suppress(Exception):
            user_definitions = len(read_schedule(real_home)["definitions"])
    return {
        "ready": not blockers,
        "blockers": blockers,
        "warnings": warnings,
        "host": host,
        "tools": tools,
        "binaries": binaries,
        "source": source,
        "config": config,
        "disk_free_bytes": usage.free,
        "launchd_labels": labels,
        "user_leveler_definitions": user_definitions,
        "active_runs": active,
        "checked_at": iso(),
    }


def make_evidence_path(name: Optional[str], mode: str) -> Path:
    if name:
        safe = re.sub(r"[^A-Za-z0-9._-]+", "-", name).strip("-")
        if not safe:
            raise SoakError("invalid run name")
        return DEFAULT_EVIDENCE_ROOT / safe
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_EVIDENCE_ROOT / ("%s-%s" % (mode, stamp))


def create_run(
    mode: str,
    duration_secs: int,
    name: Optional[str],
    model: Optional[str],
    budget: Dict[str, Any],
    binary_path: Path,
    commit: str,
    source_tree: Optional[Path] = None,
) -> Dict[str, Any]:
    report = preflight(binary_path, commit, source_tree)
    if not report["ready"]:
        raise SoakError("preflight blocked: " + "; ".join(report["blockers"]))
    evidence = make_evidence_path(name, mode)
    if evidence.exists() and any(evidence.iterdir()):
        raise SoakError("evidence directory already exists and is not empty: %s" % evidence)
    identity = binary_identity(binary_path)
    if not identity["clean"]:
        raise SoakError("refusing a dirty or untrusted binary: %s" % identity["version"])
    if identity.get("revision") and not commit.startswith(str(identity["revision"])) and not str(identity["revision"]).startswith(commit[:7]):
        raise SoakError("binary revision %s does not match commit %s" % (identity["revision"], commit))
    if not identity["sha256"]:
        raise SoakError("binary identity has no digest")
    if not model:
        model = config_info().get("default_model")
    if not model:
        raise SoakError("no --model given and no default_model in config.toml")
    live_root = choose_live_root()
    start_at = dt.datetime.now(dt.timezone.utc)
    plan = make_plan(mode, identity, commit, start_at, duration_secs, model, budget, live_root)
    plan["baseline_labels"] = report["launchd_labels"]
    plan["host"] = report["host"]
    plan["preflight"] = {
        "warnings": report["warnings"],
        "source": report["source"],
        "config": report["config"],
        "checked_at": report["checked_at"],
    }
    evidence.mkdir(parents=True, exist_ok=True)
    paths = RunPaths.for_evidence(evidence, live_root)
    atomic_write_json(paths.plan, plan)
    state = {
        "run_id": plan["run_id"],
        "mode": mode,
        "status": "STARTING",
        "phase": "starting",
        "created_at": iso(),
        "runner_pid": None,
        "plan": str(paths.plan),
        "evidence": str(evidence),
        "live_root": str(live_root),
        "actions": initial_actions(mode, start_at.timestamp()),
        "lanes": {},
        "checks": {},
    }
    atomic_write_json(paths.state, state)
    command = [sys.executable, str(Path(__file__).resolve()), "supervise", "--evidence", str(evidence)]
    environment = dict(os.environ)
    environment.pop("LEVELER_HOME", None)
    environment.pop("LEVELER_CONFIG_DIR", None)
    output = open(evidence / "runner.out", "a", encoding="utf-8")
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            env=environment,
        )
    finally:
        output.close()
    deadline = time.time() + 90
    last: Dict[str, Any] = state
    while time.time() < deadline:
        current = read_json(paths.state, {}) or {}
        last = current
        if current.get("status") in ("FAILED", "COMPLETED", "STOPPED", "BUDGET_STOPPED"):
            break
        if int(current.get("runner_pid") or 0) == process.pid and current.get("phase") == "running":
            break
        time.sleep(1)
    if int(last.get("runner_pid") or 0) != process.pid:
        raise SoakError(
            "supervisor did not claim the run within 90s (state status=%s); see %s"
            % (last.get("status"), evidence / "runner.out")
        )
    return {
        "run_id": plan["run_id"],
        "mode": mode,
        "evidence": str(evidence),
        "runner_pid": process.pid,
        "start_at": plan["start_at"],
        "deadline_at": plan["deadline_at"],
        "status": last.get("status"),
        "phase": last.get("phase"),
        "binary": identity,
        "budget": budget,
        "lanes": sorted(plan.get("lanes", {})),
        "report": str(paths.report_md),
    }


def resolve_evidence(value: Optional[str]) -> Path:
    if value:
        candidate = Path(value).expanduser()
        if candidate.is_dir():
            return candidate.resolve()
        candidate = DEFAULT_EVIDENCE_ROOT / value
        if candidate.is_dir():
            return candidate.resolve()
        raise SoakError("run not found: %s" % value)
    candidates = sorted(
        DEFAULT_EVIDENCE_ROOT.glob("*/plan.json"),
        key=lambda path: path.stat().st_mtime,
        reverse=True,
    )
    if not candidates:
        raise SoakError("no soak run exists under %s" % DEFAULT_EVIDENCE_ROOT)
    return candidates[0].parent


def lane_summary(lane: Dict[str, Any]) -> Dict[str, Any]:
    try:
        snapshot = read_schedule(Path(lane["home"]))
    except Exception as error:  # noqa: BLE001 - status must report the real error
        return {"error": redact(str(error))}
    runs = snapshot["runs"]
    last = sorted(runs, key=lambda row: row.get("started_at") or "", reverse=True)[:1]
    states: Dict[str, int] = {}
    for run in runs:
        _bump(states, run.get("state"))
    return {
        "definitions": len(snapshot["definitions"]),
        "occurrences": len(snapshot["occurrences"]),
        "runs": len(runs),
        "run_states": states,
        "wakeups": len(snapshot["wakeups"]),
        "last_run": (
            {
                "id": last[0].get("id"),
                "state": last[0].get("state"),
                "started_at": last[0].get("started_at"),
                "finished_at": last[0].get("finished_at"),
                "error": redact(str(last[0].get("error"))) if last[0].get("error") else None,
            }
            if last
            else None
        ),
        "database_bytes": Path(snapshot["database"]).stat().st_size if Path(snapshot["database"]).is_file() else 0,
    }


def status_run(evidence: Path) -> Dict[str, Any]:
    plan = read_json(evidence / "plan.json")
    state = read_json(evidence / "state.json", {}) or {}
    heartbeat = read_json(evidence / "heartbeat.json", {}) or {}
    pid = int(state.get("runner_pid") or 0)
    owned = runner_owned(pid, evidence)
    age = heartbeat_age(evidence)
    if state.get("status") in ("COMPLETED", "STOPPED", "BUDGET_STOPPED", "FAILED"):
        liveness = "finished"
    elif owned and age is not None and age <= HEARTBEAT_STALE_SECS:
        liveness = "running"
    elif owned:
        liveness = "stale-heartbeat"
    elif pid:
        liveness = "dead-runner"
    else:
        liveness = "not-started"
    lanes = {}
    if plan:
        for name in (plan.get("lanes") or {}):
            lane = (state.get("lanes") or {}).get(name)
            if lane and Path(lane.get("home", "")).is_dir():
                lanes[name] = lane_summary(lane)
            else:
                lanes[name] = {"error": "lane home unavailable"}
    report = read_json(evidence / "report.json")
    now = time.time()
    remaining = None
    if plan and plan.get("deadline_at"):
        remaining = max(0, int(parse_iso(plan["deadline_at"]).timestamp() - now))
    return {
        "run_id": state.get("run_id") or (plan or {}).get("run_id"),
        "mode": state.get("mode") or (plan or {}).get("mode"),
        "status": state.get("status"),
        "phase": state.get("phase"),
        "liveness": liveness,
        "runner_pid": pid,
        "runner_owned": owned,
        "heartbeat_age_secs": age,
        "heartbeat": heartbeat,
        "start_at": (plan or {}).get("start_at"),
        "deadline_at": (plan or {}).get("deadline_at"),
        "remaining_secs": remaining,
        "budget": (plan or {}).get("budget"),
        "budget_usage": state.get("budget_usage"),
        "budget_stopped": state.get("budget_stopped", False),
        "actions": [
            {"name": row.get("name"), "status": row.get("status"), "due_at": row.get("due_at"), "attempts": row.get("attempts")}
            for row in state.get("actions", [])
        ],
        "checks": {name: {"passed": row.get("passed"), "status": row.get("status")} for name, row in (state.get("checks") or {}).items()},
        "lanes": lanes,
        "verdict": (report or {}).get("verdict") or state.get("verdict"),
        "report": str(evidence / "report.md") if (evidence / "report.md").is_file() else None,
        "error": state.get("error"),
    }


def stop_run(evidence: Path, wait_secs: float = 150.0) -> Dict[str, Any]:
    state = read_json(evidence / "state.json", {}) or {}
    pid = int(state.get("runner_pid") or 0)
    if state.get("status") in ("COMPLETED", "STOPPED", "BUDGET_STOPPED", "FAILED") and (evidence / "report.json").is_file():
        return status_run(evidence)
    if pid and runner_owned(pid, evidence):
        os.kill(pid, signal.SIGTERM)
        deadline = time.time() + wait_secs
        while time.time() < deadline:
            current = read_json(evidence / "state.json", {}) or {}
            if current.get("status") in ("COMPLETED", "STOPPED", "BUDGET_STOPPED", "FAILED") and (
                evidence / "report.json"
            ).is_file():
                return status_run(evidence)
            time.sleep(2)
        raise SoakError("the runner did not finish within %.0fs after SIGTERM" % wait_secs)
    if pid and not runner_owned(pid, evidence):
        raise SoakError("refusing to signal pid %s: it does not own %s" % (pid, evidence))
    return recover_finalize(evidence)


def lanes_from_plan(plan: Dict[str, Any]) -> Dict[str, Dict[str, Any]]:
    live_root = Path(plan["live_root"])
    lanes: Dict[str, Dict[str, Any]] = {}
    for name in plan.get("lanes", {}):
        home = live_root / "homes" / name
        repo = live_root / "repos" / name
        binary = live_root / "bin" / name / "leveler"
        lanes[name] = {
            "name": name,
            "home": str(home),
            "repo": str(repo),
            "binary": str(binary),
            "idle_secs": 120,
            "defs": {},
            "kind": "launchd",
            "serve_pid": None,
            "label": label_for(home),
        }
    return lanes


def recover_finalize(evidence: Path) -> Dict[str, Any]:
    plan = read_json(evidence / "plan.json")
    if not plan:
        raise SoakError("cannot finalize without plan.json")
    state = read_json(evidence / "state.json", {}) or {}
    if (evidence / "report.json").is_file():
        return status_run(evidence)
    if not Path(plan["live_root"]).is_dir():
        raise SoakError("the runner is dead and the live root is gone; no finalization is possible")
    recorder = Recorder(RunPaths.for_evidence(evidence, Path(plan["live_root"])))
    lanes = state.get("lanes") or lanes_from_plan(plan)
    specs_by_lane = specs_from_plan(plan)
    checks = state.get("checks") or {}
    state.setdefault("stop_reason", "dead-runner")
    state["status"] = "FINALIZING"
    state["phase"] = "finalize"
    state["quiesce_started_at"] = iso()
    state["quiesce"] = quiesce_run(
        RunPaths.for_evidence(evidence, Path(plan["live_root"])),
        plan,
        lanes,
        recorder,
        grace_secs=120,
    )
    result = finalize_run(
        RunPaths.for_evidence(evidence, Path(plan["live_root"])),
        plan,
        state,
        recorder,
        lanes,
        specs_by_lane,
        checks,
    )
    return status_run(evidence)


def print_status_human(report: Dict[str, Any]) -> None:
    print("run: %s (%s)" % (report.get("run_id"), report.get("mode")))
    print("status: %s / %s (runner %s)" % (report.get("status"), report.get("liveness"), report.get("runner_pid")))
    print("start: %s" % report.get("start_at"))
    print("deadline: %s (remaining %ss)" % (report.get("deadline_at"), report.get("remaining_secs")))
    if report.get("verdict"):
        print("verdict: %s" % report["verdict"])
    if report.get("budget_usage"):
        print("budget: %s" % json.dumps(report["budget_usage"], ensure_ascii=False))
    pending = [row["name"] for row in report.get("actions", []) if row.get("status") == "pending"]
    if pending:
        print("pending actions: %s" % ", ".join(pending))
    for lane, summary in report.get("lanes", {}).items():
        print("lane %s: %s" % (lane, json.dumps(summary, ensure_ascii=False)))
    if report.get("report"):
        print("report: %s" % report["report"])


def cmd_preflight(args: argparse.Namespace) -> int:
    result = preflight(Path(args.binary), args.commit, Path(args.source_tree))
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0 if result["ready"] else 1


def cmd_create(args: argparse.Namespace) -> int:
    budget = {
        "max_cost_usd": float(args.max_cost_usd),
        "max_total_tokens": int(args.max_total_tokens),
    }
    result = create_run(
        "dry-run" if args.command == "dry-run" else "soak",
        int(args.duration),
        args.name,
        args.model,
        budget,
        Path(args.binary).expanduser(),
        args.commit,
        Path(args.source_tree),
    )
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    if args.wait:
        evidence = Path(result["evidence"])
        deadline = time.time() + int(args.duration) + 600
        while time.time() < deadline:
            current = read_json(evidence / "state.json", {}) or {}
            if current.get("status") in ("COMPLETED", "STOPPED", "BUDGET_STOPPED", "FAILED"):
                print(json.dumps(status_run(evidence), ensure_ascii=False, indent=2, sort_keys=True))
                return 0
            time.sleep(10)
        raise SoakError("run did not reach a terminal state before the wait deadline")
    return 0


def cmd_supervise(args: argparse.Namespace) -> int:
    evidence = Path(args.evidence).resolve()
    plan = read_json(evidence / "plan.json")
    if not plan:
        raise SoakError("missing plan.json in %s" % evidence)
    state = read_json(evidence / "state.json", {}) or {}
    recorder = Recorder(RunPaths.for_evidence(evidence, Path(plan["live_root"])))
    supervisor = Supervisor(RunPaths.for_evidence(evidence, Path(plan["live_root"])), plan, state, recorder)
    return supervisor.run()


def cmd_status(args: argparse.Namespace) -> int:
    report = status_run(resolve_evidence(args.run))
    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    else:
        print_status_human(report)
    return 0


def cmd_report(args: argparse.Namespace) -> int:
    evidence = resolve_evidence(args.run)
    report = read_json(evidence / "report.json")
    if not report:
        raise SoakError("no finalized report; run `status` first")
    if args.format == "json":
        print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    else:
        print(render_report(report))
    return 0


def cmd_stop(args: argparse.Namespace) -> int:
    evidence = resolve_evidence(args.run)
    result = stop_run(evidence)
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


def cmd_finalize(args: argparse.Namespace) -> int:
    evidence = resolve_evidence(args.run)
    report = read_json(evidence / "report.json")
    if report:
        print(json.dumps({"status": "already-finalized", "verdict": report.get("verdict"), "report": str(evidence / "report.md")}, ensure_ascii=False, indent=2, sort_keys=True))
        return 0
    state = read_json(evidence / "state.json", {}) or {}
    pid = int(state.get("runner_pid") or 0)
    if pid and runner_owned(pid, evidence):
        result = stop_run(evidence)
    else:
        result = recover_finalize(evidence)
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Phase 6.4 scheduled-automation soak runner")
    subparsers = parser.add_subparsers(dest="command", required=True)

    preflight = subparsers.add_parser("preflight", help="check host, binary and isolation preconditions")
    preflight.add_argument("--binary", default=str(DEFAULT_BINARY))
    preflight.add_argument("--commit", default=DEFAULT_COMMIT)
    preflight.add_argument("--source-tree", default=str(DEFAULT_SOURCE_TREE))
    preflight.set_defaults(func=cmd_preflight)

    def add_create(name: str, default_duration: int, default_cost: float, default_tokens: int) -> None:
        command = subparsers.add_parser(name)
        command.add_argument("--duration", type=int, default=default_duration, help="seconds of real execution")
        command.add_argument("--name", default=None, help="evidence directory name")
        command.add_argument("--model", default=None, help="agent model override")
        command.add_argument("--max-cost-usd", type=float, default=default_cost)
        command.add_argument("--max-total-tokens", type=int, default=default_tokens)
        command.add_argument("--binary", default=str(DEFAULT_BINARY))
        command.add_argument("--commit", default=DEFAULT_COMMIT)
        command.add_argument("--source-tree", default=str(DEFAULT_SOURCE_TREE))
        command.add_argument("--wait", action="store_true", help="wait for the run to finish")
        command.set_defaults(func=cmd_create)

    add_create("dry-run", DEFAULT_DRY_RUN_SECS, 2.0, 2_000_000)
    add_create("start", DEFAULT_SOAK_SECS, 15.0, 30_000_000)

    status = subparsers.add_parser("status")
    status.add_argument("--run", default=None)
    status.add_argument("--json", action="store_true")
    status.set_defaults(func=cmd_status)

    report = subparsers.add_parser("report")
    report.add_argument("--run", default=None)
    report.add_argument("--format", choices=("md", "json"), default="md")
    report.set_defaults(func=cmd_report)

    stop = subparsers.add_parser("stop")
    stop.add_argument("--run", default=None)
    stop.set_defaults(func=cmd_stop)

    finalize = subparsers.add_parser("finalize")
    finalize.add_argument("--run", default=None)
    finalize.set_defaults(func=cmd_finalize)

    supervise = subparsers.add_parser("supervise")
    supervise.add_argument("--evidence", required=True)
    supervise.set_defaults(func=cmd_supervise)

    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except SoakError as error:
        print("soak error: %s" % error, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
