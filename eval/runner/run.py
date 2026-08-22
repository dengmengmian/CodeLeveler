#!/usr/bin/env python3
"""Unified eval runner. Observer only.

    python3 eval/runner/run.py --suite adoption --experiment m3-baseline
    python3 eval/runner/run.py --suite adoption --experiment m3-baseline --runs 3 --output eval/reports/tmp

CLI overrides beat the YAML. Nothing here changes product runtime.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

EVAL_ROOT = Path(__file__).resolve().parents[1]
LIB = EVAL_ROOT / "lib"
sys.path.insert(0, str(LIB))

from catalog import load_catalog  # noqa: E402
from experiment import apply_overrides, load_experiment, model_ref, resolve_experiment  # noqa: E402
from report import experiment_report  # noqa: E402
from runner import (  # noqa: E402
    binary_version,
    default_user_config,
    git_sha,
    materialize_task_yamls,
    new_batch_id,
    prepare_home,
    run_leveler_eval,
    score_home,
    utc_now,
)
from schema import compact_record  # noqa: E402

REPO = EVAL_ROOT.parent
ADOPTION_TASKS = EVAL_ROOT / "micro" / "adoption" / "tasks"
ADOPTION_CATALOG = EVAL_ROOT / "micro" / "adoption" / "catalog.json"


def write_outputs(out_dir: Path, batch: dict, experiment: dict) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    compact = [compact_record(r) for r in batch.get("runs") or []]
    payload = {**batch, "experiment": experiment, "compact": compact}
    (out_dir / "batch.json").write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    (out_dir / "report.md").write_text(
        experiment_report(payload, experiment=experiment), encoding="utf-8"
    )


def run_adoption(cfg: dict) -> int:
    catalog = load_catalog(ADOPTION_CATALOG)
    task_ids = [str(t) for t in (cfg.get("tasks") or [])]
    cases_dir = materialize_task_yamls(
        ADOPTION_TASKS,
        catalog,
        task_ids=task_ids or None,
        shape=cfg.get("shape"),
    )
    batch_id = new_batch_id(str(cfg["experiment"]))
    home = EVAL_ROOT / "runs" / batch_id / "home"
    raw_out = EVAL_ROOT / "runs" / batch_id
    raw_out.mkdir(parents=True, exist_ok=True)
    user_cfg = default_user_config()
    prepare_home(home, str(cfg.get("arm") or "control"), user_cfg if user_cfg.is_file() else None)
    model = model_ref(cfg)
    binary = str(cfg.get("binary") or "leveler")
    rc = run_leveler_eval(
        binary=binary,
        config_dir=REPO / "configs",
        cases_dir=cases_dir,
        model=model,
        home=home,
        json_out=raw_out / "eval_result.json",
        repetitions=int(cfg["runs"]),
        timeout_seconds=int(cfg.get("timeout_seconds") or 0) or None,
    )
    batch = score_home(
        home,
        catalog=catalog,
        arm=str(cfg.get("arm") or "control"),
        model=model,
        git=git_sha(REPO),
        binary=binary_version(binary),
        batch_id=batch_id,
        started_at=utc_now(),
        suite="adoption",
        max_rounds=20,
    )
    batch["notes"] = cfg.get("description") or cfg.get("notes")
    out_dir = REPO / str(cfg["output"])
    write_outputs(out_dir, batch, cfg)
    print(f"eval_exit={rc} report={out_dir / 'report.md'} batch={out_dir / 'batch.json'}")
    print("leveler eval exit is completion/expect, not the adoption verdict.")
    return 0 if batch.get("runs") else rc


def run_capability(cfg: dict) -> int:
    cases = cfg.get("cases") or "evals/smoke"
    print(
        "capability suite uses the existing harness:\n"
        f"  {cfg.get('binary') or 'leveler'} eval run --cases {cases} "
        f"--model {model_ref(cfg)} --repetitions {cfg['runs']}"
    )
    out_dir = REPO / str(cfg["output"])
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "HOW_TO_RUN.txt").write_text(
        f"leveler eval run --cases {cases} --model {model_ref(cfg)} --repetitions {cfg['runs']}\n",
        encoding="utf-8",
    )
    return 0


def run_safety(cfg: dict) -> int:
    out_dir = REPO / str(cfg["output"])
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "HOW_TO_RUN.txt").write_text(
        "Safety probes stay in the control plane and MUST NOT enter the adoption denominator.\n"
        "See docs/eval-methodology.md and eval/safety/README.md.\n",
        encoding="utf-8",
    )
    print(f"safety experiment `{cfg['experiment']}` is documented at {out_dir / 'HOW_TO_RUN.txt'}")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--suite", required=True)
    p.add_argument("--experiment", required=True)
    p.add_argument("--model")
    p.add_argument("--provider")
    p.add_argument("--runs", type=int)
    p.add_argument("--output")
    p.add_argument("--binary")
    p.add_argument("--task")
    p.add_argument("--shape")
    args = p.parse_args()
    path = resolve_experiment(EVAL_ROOT, args.suite, args.experiment)
    cfg = apply_overrides(
        load_experiment(path),
        model=args.model,
        provider=args.provider,
        runs=args.runs,
        output=args.output,
        binary=args.binary,
        task=args.task,
        shape=args.shape,
    )
    suite = cfg["suite"]
    if suite == "adoption":
        return run_adoption(cfg)
    if suite == "capability":
        return run_capability(cfg)
    if suite == "safety":
        return run_safety(cfg)
    raise SystemExit(f"unknown suite {suite!r}")


if __name__ == "__main__":
    sys.exit(main())
