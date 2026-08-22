"""Observer runner: isolated LEVELER_HOME + `leveler eval run`.

Does not inject prompts, does not set eval-only product behaviour. The only
arm-specific product input is the already-shipped `agents.offer_timing`
config key, written into the isolated home (never into ~/.leveler).
"""

from __future__ import annotations

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

FACTORS = {
    ARM_CONTROL: ("baseline", "product_default"),
    ARM_TIMING: ("timing", "after_first_edit"),
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
    (home / "config.toml").write_text(text, encoding="utf-8")


def infer_task_id(db_path: Path, known_ids: list[str]) -> str | None:
    blob = str(db_path)
    for tid in sorted(known_ids, key=len, reverse=True):
        if tid in blob:
            return tid
    return None


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
) -> dict[str, Any]:
    factor, value = FACTORS.get(arm, ("unknown", arm))
    known_ids = list((catalog.get("tasks") or {}).keys())
    runs = []
    for db in find_session_dbs(home):
        timeline = extract_path(db)
        task_id = infer_task_id(db, known_ids) or "unknown"
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


