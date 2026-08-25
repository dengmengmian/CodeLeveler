#!/usr/bin/env python3
"""Unified eval runner. Observer only.

    python3 eval/runner/run.py --suite adoption --experiment m3-baseline
    python3 eval/runner/run.py --suite adoption --experiment m3-baseline --runs 3 --output eval/reports/tmp

CLI overrides beat the YAML. Nothing here changes product runtime.
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
from pathlib import Path

EVAL_ROOT = Path(__file__).resolve().parents[1]
LIB = EVAL_ROOT / "lib"
sys.path.insert(0, str(LIB))

from catalog import load_catalog  # noqa: E402
from experiment import (  # noqa: E402
    apply_overrides,
    load_experiment,
    model_ref,
    normalize_mode,
    resolve_experiment,
)
from report import experiment_report  # noqa: E402
from reviewer import reviewer_eval_result  # noqa: E402
from value import value_eval_result  # noqa: E402
from runner import (  # noqa: E402
    binary_version,
    default_user_config,
    git_sha,
    load_expect_verdicts,
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
VALUE_ROOT = EVAL_ROOT / "suites" / "multi_agent" / "multi_agent_value"
VALUE_CATALOG = VALUE_ROOT / "cases" / "catalog.json"
REVIEWER_ROOT = EVAL_ROOT / "suites" / "multi_agent" / "reviewer_value"
REVIEWER_CATALOG = REVIEWER_ROOT / "cases" / "catalog.json"


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
    if experiment.get("suite") == "multi_agent":
        projector = (
            reviewer_eval_result
            if experiment.get("experiment") == "MA-VALUE-REVIEWER-PILOT"
            else value_eval_result
        )
        results = [projector(r) for r in batch.get("runs") or []]
        (out_dir / "eval_result.json").write_text(
            json.dumps(
                {
                    "experiment": experiment.get("experiment"),
                    "mode": experiment.get("mode"),
                    "runs": results,
                },
                indent=2,
                ensure_ascii=False,
            )
            + "\n",
            encoding="utf-8",
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


def _how_to_run_value(cfg: dict, mode: str) -> str:
    model = model_ref(cfg)
    binary = cfg.get("binary") or "leveler"
    return (
        "# MA-VALUE-001 — how to run\n\n"
        "Observer only. Does not change spawn, claim, ownership, settlement, "
        "prompts, or tool schema.\n\n"
        "Control (`--mode single`) writes the shipped `agents.delegation = false` "
        "key into an isolated LEVELER_HOME. Treatment (`--mode multi`) uses the "
        "product default. Same model, provider, repository, task, tools, budget.\n\n"
        "Cases are Real Usage R005–R010 pointers. Hidden verifiers and checkouts "
        "live in the dogfood-control repo; this tree does not vendor them.\n\n"
        "## Framework (no model calls)\n\n"
        f"```sh\n{binary} eval run --suite multi_agent --experiment MA-VALUE-001 "
        f"--mode {mode}\n```\n\n"
        "## One arm, real model (expensive)\n\n"
        "Requires CONTROL_ROOT, a published binary, and budget. 6 tasks × 3 runs "
        "per arm; both arms = 36 runs.\n\n"
        "```sh\n"
        "export CONTROL_ROOT=\"${HOME}/Develop/codeleveler-dogfood-control\"\n"
        f"python3 eval/runner/run.py --suite multi_agent --experiment MA-VALUE-001 "
        f"--mode {mode} --model {model} --execute\n"
        "```\n\n"
        "Spawn rate is a diagnostic, not a success metric. See "
        "docs/evaluations/MA-VALUE-001.md.\n"
    )


def _how_to_run_reviewer(cfg: dict, mode: str) -> str:
    model = model_ref(cfg)
    binary = cfg.get("binary") or "leveler"
    return (
        "# MA-VALUE-REVIEWER-PILOT — how to run\n\n"
        "Observer only. Does not change reviewer permissions, spawn runtime, "
        "or tool schema. Finding count is not a success metric.\n\n"
        "Control (`--mode self`) writes `agents.independent_review = \"off\"` "
        "into an isolated LEVELER_HOME. Treatment (`--mode reviewer`) writes "
        "`\"always\"` so a reviewer launches after any product mutation. "
        "Product default stays `auto`.\n\n"
        "Cases are vendored EvaluationCase YAML under `evals/` (bug fix, "
        "concurrency, secrets, cross-module). Independent `expect` is the "
        "correctness score.\n\n"
        "## Framework (no model calls)\n\n"
        f"```sh\n{binary} eval run --suite multi_agent "
        f"--experiment MA-VALUE-REVIEWER-PILOT --mode {mode}\n```\n\n"
        "## Pilot, real model (5 tasks × 1 run per arm)\n\n"
        f"```sh\npython3 eval/runner/run.py --suite multi_agent "
        f"--experiment MA-VALUE-REVIEWER-PILOT --mode {mode} "
        f"--model {model} --execute\n```\n\n"
        "n=5 pairs is a pipeline check. min_n=6 for a published verdict.\n"
        "See docs/evaluations/MA-VALUE-REVIEWER-PILOT.md.\n"
    )


def run_reviewer_value(cfg: dict) -> int:
    mode = normalize_mode(cfg.get("mode"))
    if mode not in ("self", "reviewer"):
        raise SystemExit(
            "MA-VALUE-REVIEWER-PILOT requires --mode self or --mode reviewer"
        )
    catalog = load_catalog(REVIEWER_CATALOG)
    batch_id = new_batch_id(f"MA-VALUE-REVIEWER-PILOT-{mode}")
    home = EVAL_ROOT / "runs" / batch_id / "home"
    raw_out = EVAL_ROOT / "runs" / batch_id
    raw_out.mkdir(parents=True, exist_ok=True)
    user_cfg = default_user_config()
    prepare_home(home, mode, user_cfg if user_cfg.is_file() else None)
    model = model_ref(cfg)
    binary = str(cfg.get("binary") or "leveler")
    out_dir = REPO / str(cfg["output"]) / mode
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "HOW_TO_RUN.md").write_text(_how_to_run_reviewer(cfg, mode), encoding="utf-8")
    (out_dir / "experiment.json").write_text(
        json.dumps({k: v for k, v in cfg.items() if k != "_path"}, indent=2, default=str) + "\n",
        encoding="utf-8",
    )
    execute = bool(cfg.get("execute"))
    rc = 0
    eval_result = raw_out / "eval_result.json"
    if execute:
        import tempfile

        dest = Path(tempfile.mkdtemp(prefix="eval-reviewer-"))
        wanted = {str(t) for t in (cfg.get("tasks") or [])}
        for tid, entry in (catalog.get("tasks") or {}).items():
            if wanted and tid not in wanted:
                continue
            src = REPO / str(entry.get("evals_path") or "")
            if not src.is_file():
                raise SystemExit(f"EvaluationCase not found: {src}")
            shutil.copy(src, dest / src.name)
        rc = run_leveler_eval(
            binary=binary,
            config_dir=REPO / "configs",
            cases_dir=dest,
            model=model,
            home=home,
            json_out=eval_result,
            repetitions=int(cfg["runs"]),
            timeout_seconds=int(cfg.get("timeout_seconds") or 0) or None,
        )
    batch = score_home(
        home,
        catalog=catalog,
        arm=mode,
        model=model,
        git=git_sha(REPO),
        binary=binary_version(binary),
        batch_id=batch_id,
        started_at=utc_now(),
        suite="multi_agent",
        max_rounds=None,
        experiment=str(cfg.get("experiment") or "MA-VALUE-REVIEWER-PILOT"),
        verdicts=load_expect_verdicts(eval_result),
    )
    batch["notes"] = cfg.get("description") or cfg.get("notes")
    batch["mode"] = mode
    write_outputs(out_dir, batch, cfg)
    print(
        f"eval_exit={rc} report={out_dir / 'report.md'} batch={out_dir / 'batch.json'} "
        f"home={home} execute={execute}"
    )
    if not execute:
        print("No model run. Isolated home is wired for the requested arm.")
    return 0 if batch.get("runs") or not execute else rc


def run_multi_agent(cfg: dict) -> int:
    if str(cfg.get("experiment") or "") == "MA-VALUE-REVIEWER-PILOT":
        return run_reviewer_value(cfg)
    mode = normalize_mode(cfg.get("mode"))
    if mode is None:
        raise SystemExit("multi_agent suite requires --mode single or --mode multi")
    catalog = load_catalog(VALUE_CATALOG)
    batch_id = new_batch_id(f"MA-VALUE-001-{mode}")
    home = EVAL_ROOT / "runs" / batch_id / "home"
    raw_out = EVAL_ROOT / "runs" / batch_id
    raw_out.mkdir(parents=True, exist_ok=True)
    user_cfg = default_user_config()
    prepare_home(home, mode, user_cfg if user_cfg.is_file() else None)
    model = model_ref(cfg)
    binary = str(cfg.get("binary") or "leveler")
    out_dir = REPO / str(cfg["output"]) / mode
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "HOW_TO_RUN.md").write_text(_how_to_run_value(cfg, mode), encoding="utf-8")
    (out_dir / "experiment.json").write_text(
        json.dumps({k: v for k, v in cfg.items() if k != "_path"}, indent=2, default=str) + "\n",
        encoding="utf-8",
    )
    execute = bool(cfg.get("execute"))
    if execute:
        raise SystemExit(
            "MA-VALUE-001 --execute is not wired to a vendored case set. "
            "R005–R010 live in the dogfood-control repo; this tree only scores "
            "EventLogs and records the experiment. Set CONTROL_ROOT and score "
            "an existing LEVELER_HOME rather than inventing tasks here."
        )
    batch = score_home(
        home,
        catalog=catalog,
        arm=mode,
        model=model,
        git=git_sha(REPO),
        binary=binary_version(binary),
        batch_id=batch_id,
        started_at=utc_now(),
        suite="multi_agent",
        max_rounds=None,
        experiment=str(cfg.get("experiment") or "MA-VALUE-001"),
    )
    batch["notes"] = cfg.get("description") or cfg.get("notes")
    batch["mode"] = mode
    write_outputs(out_dir, batch, cfg)
    print(
        f"eval_exit=0 report={out_dir / 'report.md'} batch={out_dir / 'batch.json'} "
        f"home={home} execute={execute}"
    )
    print("No model run. Isolated home is wired for the requested arm.")
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
    p.add_argument(
        "--mode",
        help="single|multi (MA-VALUE-001) or self|reviewer (MA-VALUE-REVIEWER-PILOT)",
    )
    p.add_argument(
        "--execute",
        action="store_true",
        help="Attempt a real-model run. MA-VALUE-001 refuses: cases are not vendored.",
    )
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
        mode=args.mode,
        execute=True if args.execute else None,
    )
    suite = cfg["suite"]
    if suite == "adoption":
        return run_adoption(cfg)
    if suite == "capability":
        return run_capability(cfg)
    if suite == "safety":
        return run_safety(cfg)
    if suite == "multi_agent":
        return run_multi_agent(cfg)
    raise SystemExit(f"unknown suite {suite!r}")


if __name__ == "__main__":
    sys.exit(main())
