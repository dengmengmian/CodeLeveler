"""Observer runner: isolated LEVELER_HOME + `leveler eval run`.

Does not inject prompts, does not set eval-only product behaviour. The only
arm-specific product input is the already-shipped `agents.offer_timing`
config key, written into the isolated home (never into ~/.leveler).
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from catalog import expected_disposition, load_catalog, task_shape
from eventlog import extract_path, find_session_dbs
from schema import make_batch, make_run

ARM_CONTROL = "control"
ARM_TIMING = "timing.after_first_edit"
ARM_SINGLE = "single"
ARM_MULTI = "multi"
ARM_SELF = "self"
ARM_REVIEWER = "reviewer"

FACTORS = {
    ARM_CONTROL: ("baseline", "product_default"),
    ARM_TIMING: ("timing", "after_first_edit"),
    ARM_SINGLE: ("mode", "single_agent"),
    ARM_MULTI: ("mode", "multi_agent"),
    "single_agent": ("mode", "single_agent"),
    "multi_agent": ("mode", "multi_agent"),
    ARM_SELF: ("independent_review", "off"),
    ARM_REVIEWER: ("independent_review", "always"),
}


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def git_sha(repo: Path) -> str | None:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            cwd=repo,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        return out.strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def binary_version(binary: str) -> str | None:
    try:
        out = subprocess.check_output([binary, "--version"], text=True, stderr=subprocess.STDOUT)
        return out.strip().splitlines()[0]
    except (OSError, subprocess.CalledProcessError):
        return None


def _is_delegation_assignment(line: str) -> bool:
    stripped = line.strip()
    if stripped.startswith("#"):
        return False
    return stripped.startswith("delegation") and "=" in stripped


def _is_independent_review_assignment(line: str) -> bool:
    stripped = line.strip()
    if stripped.startswith("#"):
        return False
    return stripped.startswith("independent_review") and "=" in stripped


def prepare_home(home: Path, arm: str, user_config: Path | None) -> None:
    home.mkdir(parents=True, exist_ok=True)
    text = ""
    if user_config and user_config.is_file():
        text = user_config.read_text(encoding="utf-8")
    if arm == ARM_TIMING:
        # Isolated home only. Product default stays plan_registration.
        text = "\n".join(ln for ln in text.splitlines() if "offer_timing" not in ln)
        if "[agents]" in text:
            text = text.replace("[agents]", '[agents]\noffer_timing = "after_first_edit"', 1)
        else:
            text += '\n[agents]\noffer_timing = "after_first_edit"\n'
    elif arm in (ARM_SINGLE, "single_agent"):
        # Shipped `agents.delegation` key, isolated home only. Not eval_mode.
        lines = [ln for ln in text.splitlines() if not _is_delegation_assignment(ln)]
        text = "\n".join(lines)
        if "[agents]" in text:
            text = text.replace("[agents]", "[agents]\ndelegation = false", 1)
        else:
            if text and not text.endswith("\n"):
                text += "\n"
            text += "[agents]\ndelegation = false\n"
    elif arm in (ARM_MULTI, "multi_agent"):
        # Product default is on. Strip a copied-in disable so this arm is
        # not contaminated by the user's global preference.
        lines = [ln for ln in text.splitlines() if not _is_delegation_assignment(ln)]
        text = "\n".join(lines)
        if text and not text.endswith("\n"):
            text += "\n"
    elif arm in (ARM_SELF, "self_verify"):
        # Isolated home only. Product default stays `auto` (shape-triggered).
        lines = [ln for ln in text.splitlines() if not _is_independent_review_assignment(ln)]
        text = "\n".join(lines)
        if "[agents]" in text:
            text = text.replace("[agents]", '[agents]\nindependent_review = "off"', 1)
        else:
            if text and not text.endswith("\n"):
                text += "\n"
            text += '[agents]\nindependent_review = "off"\n'
    elif arm in (ARM_REVIEWER, "independent_review"):
        lines = [ln for ln in text.splitlines() if not _is_independent_review_assignment(ln)]
        text = "\n".join(lines)
        if "[agents]" in text:
            text = text.replace("[agents]", '[agents]\nindependent_review = "always"', 1)
        else:
            if text and not text.endswith("\n"):
                text += "\n"
            text += '[agents]\nindependent_review = "always"\n'
    (home / "config.toml").write_text(text, encoding="utf-8")


def infer_task_id(db_path: Path, known_ids: list[str]) -> str | None:
    blob = str(db_path)
    for tid in sorted(known_ids, key=len, reverse=True):
        if tid in blob:
            return tid
    return None



def load_expect_verdicts(eval_result: Path) -> dict[str, dict[str, Any]]:
    """Join each case's independent `expect` verdict, keyed by case id.

    `leveler eval run --json-out` already executed `expect`; without this the
    observer scores `task_success = None` for every run and the experiment
    cannot answer its primary question.

    A verifier that did not run stays `passed=None` (unscored), never False —
    an unscored run must not be counted as a failure. With repetitions, every
    repetition must pass.
    """
    try:
        doc = json.loads(Path(eval_result).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    out: dict[str, dict[str, Any]] = {}
    for case in ((doc.get("report") or {}).get("cases") or []):
        cid = case.get("id")
        if not cid:
            continue
        ran = bool(case.get("verification_ran"))
        passed = case.get("expect_passed") if ran else None
        ev = case.get("verification_evidence") or {}
        argv = [str(ev.get("program") or "")] + [str(a) for a in (ev.get("args") or [])]
        command = " ".join(x for x in argv if x) or None
        prev = out.get(cid)
        if prev is None:
            out[cid] = {"ran": ran, "passed": passed, "command": command}
            continue
        # Repetitions: all must pass; any repetition that ran makes it scored.
        prev["ran"] = prev["ran"] or ran
        if passed is False:
            prev["passed"] = False
        elif prev["passed"] is None and passed is True:
            prev["passed"] = True
        prev["command"] = prev["command"] or command
    return out


def score_home(
    home: Path,
    *,
    catalog: dict[str, Any],
    arm: str,
    model: str | None,
    git: str | None,
    binary: str | None,
    batch_id: str,
    started_at: str | None,
    suite: str = "adoption",
    max_rounds: int | None = 20,
    experiment: str | None = None,
    verdicts: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    factor, value = FACTORS.get(arm, ("unknown", arm))
    known_ids = list((catalog.get("tasks") or {}).keys())
    runs = []
    for db in find_session_dbs(home):
        timeline = extract_path(db)
        task_id = infer_task_id(db, known_ids) or "unknown"
        verdict = (verdicts or {}).get(task_id) or {}
        run = make_run(
            run_id=f"{batch_id}:{task_id}:{db.parent.name[-12:]}",
            started_at=started_at,
            git_sha=git,
            binary=binary,
            leveler_home=str(home),
            session_db=str(db),
            task_id=task_id,
            suite=suite,
            max_rounds=max_rounds,
            expected_disposition=expected_disposition(catalog, task_id),
            shape=task_shape(catalog, task_id),
            arm_name=arm,
            arm_factor=factor,
            arm_value=value,
            model_ref=model or timeline.get("model_from_event"),
            timeline=timeline,
            verifier_ran=bool(verdict.get("ran")),
            verifier_passed=verdict.get("passed"),
            verifier_command=verdict.get("command"),
            experiment=experiment,
            mode=arm
            if arm
            in (
                ARM_SINGLE,
                ARM_MULTI,
                "single_agent",
                "multi_agent",
                ARM_SELF,
                ARM_REVIEWER,
            )
            else None,
        )
        runs.append(run)
    return make_batch(
        batch_id=batch_id,
        runs=runs,
        arm={"name": arm, "factor": factor, "value": value},
        model=model,
    )


def materialize_task_yamls(
    tasks_dir: Path,
    catalog: dict[str, Any],
    *,
    task_ids: list[str] | None = None,
    shape: str | None = None,
) -> Path:
    wanted_ids = {t for t in (task_ids or []) if t}
    wanted = []
    for path in sorted(tasks_dir.glob("*.yaml")):
        text = path.read_text(encoding="utf-8")
        tid = None
        for line in text.splitlines():
            if line.startswith("id:"):
                tid = line.split(":", 1)[1].strip()
                break
        if tid is None:
            continue
        entry = (catalog.get("tasks") or {}).get(tid) or {}
        if wanted_ids and tid not in wanted_ids:
            continue
        if shape and entry.get("shape") != shape:
            continue
        wanted.append(path)
    if not wanted:
        raise FileNotFoundError(f"no tasks matched ids={task_ids!r} shape={shape!r} in {tasks_dir}")
    dest = Path(tempfile.mkdtemp(prefix="eval-cases-"))
    for path in wanted:
        shutil.copy(path, dest / path.name)
    return dest


def run_leveler_eval(
    *,
    binary: str,
    config_dir: Path,
    cases_dir: Path,
    model: str,
    home: Path,
    json_out: Path,
    repetitions: int,
    timeout_seconds: int | None = None,
) -> int:
    json_out.parent.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["LEVELER_HOME"] = str(home)
    env["LEVELER_EVAL_KEEP_WORKSPACE"] = "1"
    cmd = [
        binary,
        "--config-dir",
        str(config_dir),
        "eval",
        "run",
        "--model",
        model,
        "--cases",
        str(cases_dir),
        "--repetitions",
        str(repetitions),
        "--json-out",
        str(json_out),
    ]
    try:
        proc = subprocess.run(cmd, env=env, timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        return 124
    return proc.returncode


def default_user_config() -> Path:
    override = os.environ.get("LEVELER_HOME")
    if override:
        return Path(override) / "config.toml"
    return Path.home() / ".leveler" / "config.toml"


def new_batch_id(prefix: str) -> str:
    return f"{prefix}-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}-{uuid.uuid4().hex[:6]}"


