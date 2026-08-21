#!/usr/bin/env python3
"""Adoption micro observer runner.

    python3 eval/micro/adoption/runner/run.py run --model deepseek/deepseek-v4-flash
    python3 eval/micro/adoption/runner/run.py run --model flash --provider deepseek --shape parallel
    python3 eval/micro/adoption/runner/run.py run --task a01-independent-modules --model M
    python3 eval/micro/adoption/runner/run.py report --batch PATH --md PATH

Does not change product runtime. Isolated LEVELER_HOME only.
First experiment: vary task shape only (no prompt arm).
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
import tempfile
from pathlib import Path

ADOPTION = Path(__file__).resolve().parents[1]
EVAL_ROOT = ADOPTION.parents[1]
LIB = EVAL_ROOT / "lib"
sys.path.insert(0, str(LIB))

from catalog import load_catalog  # noqa: E402
from report import adoption_micro_report  # noqa: E402
from runner import (  # noqa: E402
    ARM_CONTROL,
    binary_version,
    default_user_config,
    git_sha,
    new_batch_id,
    prepare_home,
    run_leveler_eval,
    score_home,
    utc_now,
)
from schema import compact_record  # noqa: E402

REPO = EVAL_ROOT.parent
DEFAULT_CASES = ADOPTION / "tasks"
DEFAULT_CATALOG = ADOPTION / "catalog.json"
DEFAULT_CONFIG_DIR = REPO / "configs"
REPORTS = ADOPTION / "reports"


def resolve_model(model: str | None, provider: str | None) -> str:
    if not model:
        raise SystemExit("--model is required")
    if provider and "/" not in model:
        return f"{provider}/{model}"
    return model


def materialize_cases(catalog: dict, task: str | None, shape: str | None) -> Path:
    wanted = []
    for path in sorted(DEFAULT_CASES.glob("*.yaml")):
        text = path.read_text(encoding="utf-8")
        tid = None
        for line in text.splitlines():
            if line.startswith("id:"):
                tid = line.split(":", 1)[1].strip()
                break
        if tid is None:
            continue
        entry = (catalog.get("tasks") or {}).get(tid) or {}
        if task and tid != task:
            continue
        if shape and entry.get("shape") != shape:
            continue
        wanted.append(path)
    if not wanted:
        raise SystemExit(f"no tasks matched task={task!r} shape={shape!r}")
    dest = Path(tempfile.mkdtemp(prefix="adoption-micro-cases-"))
    for path in wanted:
        shutil.copy(path, dest / path.name)
    return dest


def cmd_run(args: argparse.Namespace) -> int:
    catalog = load_catalog(Path(args.catalog))
    model = resolve_model(args.model, args.provider)
    cases_dir = materialize_cases(catalog, args.task, args.shape)
    batch_id = args.batch_id or new_batch_id("adoption")
    home = Path(args.home) if args.home else EVAL_ROOT / "runs" / batch_id / "home"
    out_dir = Path(args.out_dir) if args.out_dir else EVAL_ROOT / "runs" / batch_id
    out_dir.mkdir(parents=True, exist_ok=True)
    user_cfg = Path(args.user_config) if args.user_config else default_user_config()
    prepare_home(home, args.arm, user_cfg if user_cfg.is_file() else None)
    started = utc_now()
    eval_json = out_dir / "eval_result.json"
    rc = run_leveler_eval(
        binary=args.binary,
        config_dir=Path(args.config_dir),
        cases_dir=cases_dir,
        model=model,
        home=home,
        json_out=eval_json,
        repetitions=args.repetitions,
    )
    batch = score_home(
        home,
        catalog=catalog,
        arm=args.arm,
        model=model,
        git=git_sha(REPO),
        binary=binary_version(args.binary),
        batch_id=batch_id,
        started_at=started,
        suite="adoption",
        max_rounds=20,
    )
    batch["notes"] = "task_shape experiment; KEEP is first-class; no prompt arm"
    out_path = Path(args.json_out) if args.json_out else out_dir / "batch.json"
    compact = [compact_record(r) for r in batch["runs"]]
    payload = {**batch, "compact": compact}
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    md_path = Path(args.md_out) if args.md_out else out_dir / "REPORT.md"
    md_path.write_text(
        adoption_micro_report(batch, title="Adoption Micro Eval Report"),
        encoding="utf-8",
    )
    print(f"batch {batch_id} runs={len(batch['runs'])} eval_exit={rc}")
    print(f"json {out_path}")
    print(f"md   {md_path}")
    print("leveler eval exit is completion/expect, not the adoption verdict.")
    return 0 if batch["runs"] else rc


def cmd_report(args: argparse.Namespace) -> int:
    batch = json.loads(Path(args.batch).read_text(encoding="utf-8"))
    md = adoption_micro_report(batch, title="Adoption Micro Eval Report")
    out = Path(args.md) if args.md else Path("-")
    if str(out) == "-":
        sys.stdout.write(md)
    else:
        out.write_text(md, encoding="utf-8")
        print(f"wrote {out}")
    if args.csv:
        from report import csv_summary

        Path(args.csv).write_text(csv_summary(batch.get("runs") or []), encoding="utf-8")
    return 0


def cmd_score(args: argparse.Namespace) -> int:
    catalog = load_catalog(Path(args.catalog))
    batch = score_home(
        Path(args.home),
        catalog=catalog,
        arm=args.arm,
        model=args.model,
        git=git_sha(REPO),
        binary=binary_version(args.binary) if args.binary else None,
        batch_id=args.batch_id or new_batch_id("score"),
        started_at=None,
        suite="adoption",
        max_rounds=20,
    )
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    batch["compact"] = [compact_record(r) for r in batch["runs"]]
    out.write_text(json.dumps(batch, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"scored {len(batch['runs'])} run(s) -> {out}")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    run = sub.add_parser("run", help="run the decision benchmark (observer)")
    run.add_argument("--model", help="model id, or name when --provider is set")
    run.add_argument("--provider", help="provider prefix; combined as provider/model")
    run.add_argument("--task", help="single task id")
    run.add_argument("--shape", choices=["parallel", "boundary", "single"])
    run.add_argument("--arm", default=ARM_CONTROL, choices=[ARM_CONTROL, "timing.after_first_edit"])
    run.add_argument("--repetitions", type=int, default=1)
    run.add_argument("--catalog", default=str(DEFAULT_CATALOG))
    run.add_argument("--config-dir", default=str(DEFAULT_CONFIG_DIR))
    run.add_argument("--binary", default="leveler")
    run.add_argument("--home")
    run.add_argument("--out-dir")
    run.add_argument("--json-out")
    run.add_argument("--md-out")
    run.add_argument("--batch-id")
    run.add_argument("--user-config")
    run.set_defaults(func=cmd_run)

    report = sub.add_parser("report", help="render Markdown from a batch.json")
    report.add_argument("--batch", required=True)
    report.add_argument("--md")
    report.add_argument("--csv")
    report.set_defaults(func=cmd_report)

    score = sub.add_parser("score", help="score an existing LEVELER_HOME")
    score.add_argument("--home", required=True)
    score.add_argument("--out", required=True)
    score.add_argument("--arm", default=ARM_CONTROL)
    score.add_argument("--model")
    score.add_argument("--catalog", default=str(DEFAULT_CATALOG))
    score.add_argument("--binary", default="leveler")
    score.add_argument("--batch-id")
    score.set_defaults(func=cmd_score)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
