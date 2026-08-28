#!/usr/bin/env python3
"""Comparative lane runner — one case through one harness, judged externally.

Thin by design (PRE_BETA_EVAL_PLAN §5): materialize the exact same workspace
the native eval runner would build, hand the case's `task` text verbatim to the
harness's own headless CLI, enforce one shared wall-clock timeout, then run the
case's `expect` command as the only judge. The adapter never edits prompts,
never retries, never post-processes answers.

Usage:
  python3 eval/comparative/runner.py --arm leveler:deepseek/deepseek-v4-flash \
      --cases n3-caller-propagation --rep-id 1 --out eval/comparative/results/cl.jsonl
  python3 eval/comparative/runner.py --hc001 --out eval/comparative/results/hc-001.jsonl
  python3 eval/comparative/runner.py --hc002 --prepare-only \
      --out eval/comparative/results/hc-002-prepare.jsonl
Arms: leveler | atomcode | dsh   (":<model>" suffix selects the model;
atomcode ignores the suffix so the real default provider stays in force)

HC-002 paid runs stay HOLD until CODELEVELER_EVAL_BASELINE is a SHA other
than the HC-001 freeze (3b400357). Completion Reconciliation must land first.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "eval/comparative/manifest.yaml"
DSH_ROOT = Path.home() / "Develop/app/other/deepseek-harness"
ATOMCODE_BIN = Path.home() / ".local/bin/atomcode"
LEVELER_BIN = Path(os.environ.get("CODELEVELER_BINARY") or Path.home() / ".cargo/bin/leveler")
ATOMCODE_CONFIG = Path.home() / ".atomcode/config.toml"
LEVELER_CONFIG = Path.home() / ".leveler/config.toml"
FROZEN_LEVELER_SHA = "7a263e931a4f3907c1a05d7407413d9e6a722924"
OBSOLETE_HC001_LEVELER_SHA = "3b400357342cef4caa760628531ead3bd9eff333"
FROZEN_ATOMCODE_VERSION = "5.0.9"
FROZEN_ATOMCODE_SHA = "52ca5e6"
FROZEN_DSH_SHA = "cd5ef8148"
FROZEN_DSH_VERSION = "0.1.2-alpha.1"
HC001_CASE = "n3-caller-propagation"
HC001_MODEL = "deepseek/deepseek-v4-flash"
HC002_CASE = "icg-5-long-task"
HC002_TIMEOUT_SECONDS = 1800
WIRE_MODEL = "deepseek-v4-flash"

PROXY_VARS = ["http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"]

# Specified rotation: reduce provider/time-window bias.
HC001_ORDER = [
    ("leveler", 1),
    ("atomcode", 1),
    ("dsh", 1),
    ("dsh", 2),
    ("atomcode", 2),
    ("leveler", 2),
]
HC002_ORDER = list(HC001_ORDER)

_SK_RE = re.compile(r"sk-[A-Za-z0-9._-]{10,}")
_BEARER_RE = re.compile(r"(Bearer\s+)[A-Za-z0-9._\-+=/]+", re.I)


def clean_env(extra=None):
    env = dict(os.environ)
    for k in PROXY_VARS:
        env.pop(k, None)
    if extra:
        env.update(extra)
    return env


def evidence_iso(out_dir: Path, arm: str, case_id: str, rep: int) -> Path:
    return out_dir / arm / f"{case_id}-r{rep}"


def hc002_paid_gate(baseline: str | None) -> str:
    """Refuse paid HC-002 on the HC-001 CodeLeveler SHA.

    CODELEVELER_EVAL_BASELINE must be the post-reconciliation freeze
    (currently 7a263e93, which contains f759ff4a).
    """
    sha = (baseline or "").strip()
    if not sha or sha.startswith(OBSOLETE_HC001_LEVELER_SHA[:12]):
        raise SystemExit(
            "HC002_PAID_RUNS=HOLD: waiting for post-Completion-Reconciliation "
            "CODELEVELER_EVAL_BASELINE (not 3b400357)"
        )
    if len(sha) < 12:
        raise SystemExit(f"HC002_PAID_RUNS=HOLD: CODELEVELER_EVAL_BASELINE too short: {sha!r}")
    return sha


def file_sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def text_sha256(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def redact_text(text: str, extra_secrets=None) -> str:
    """Strip credential material before any evidence is persisted."""
    out = text or ""
    secrets = [s for s in (extra_secrets or []) if s and len(s) >= 8]
    for key, value in os.environ.items():
        ku = key.upper()
        if (
            value
            and len(value) >= 8
            and any(tok in ku for tok in ("API_KEY", "APIKEY", "ACCESS_TOKEN", "SECRET"))
            and "SSH" not in ku
        ):
            secrets.append(value)
    # longest first so a prefix of another secret cannot leave a tail
    for secret in sorted(set(secrets), key=len, reverse=True):
        out = out.replace(secret, "<REDACTED>")
    out = _SK_RE.sub("<REDACTED>", out)
    out = _BEARER_RE.sub(r"\1<REDACTED>", out)
    return out


def parse_claimed_completion(arm: str, text: str) -> dict:
    """Observable completion claim. Not a correctness judgement."""
    blob = text or ""
    if arm == "leveler":
        if "Completed in" in blob and "but not independently verified" in blob:
            return {"claimed_done": True, "stop_class": "CompletedUnverified", "method": "structured"}
        if re.search(r"Completed in \d+ round", blob):
            return {"claimed_done": True, "stop_class": "Completed", "method": "structured"}
        if "reported the goal blocked" in blob:
            return {"claimed_done": False, "stop_class": "Blocked", "method": "structured"}
        if "completeness could not be established" in blob:
            return {"claimed_done": False, "stop_class": "Incomplete", "method": "structured"}
        if "Answer ended after" in blob:
            return {"claimed_done": False, "stop_class": "Answered", "method": "structured"}
        if "Hit absolute round ceiling" in blob:
            return {"claimed_done": False, "stop_class": "TurnLimitReached", "method": "structured"}
        if "Policy-blocked" in blob:
            return {"claimed_done": False, "stop_class": "PolicyBlocked", "method": "structured"}
        if "went quiet without resolving" in blob:
            return {"claimed_done": False, "stop_class": "Stalled", "method": "structured"}
        if "stopped redundant closeout" in blob:
            return {"claimed_done": True, "stop_class": "CloseoutForced", "method": "structured"}
        if "BudgetExhausted" in blob or "budget" in blob.lower() and "Resume with: leveler resume" in blob:
            return {"claimed_done": False, "stop_class": "BudgetExhausted", "method": "structured"}
        return {"claimed_done": None, "stop_class": "Unknown", "method": "structured"}

    tail = blob[-4000:]
    lowered = tail.lower()
    fail_markers = (
        "could not complete",
        "unable to finish",
        "i could not",
        "task failed",
        "giving up",
        "blocked",
        "not done",
        "incomplete",
    )
    done_markers = (
        "the task is complete",
        "successfully fixed",
        "all tests pass",
        "done.",
        "completed the task",
        "fix is complete",
        "changes are complete",
    )
    if any(m in lowered for m in fail_markers):
        return {"claimed_done": False, "stop_class": "HeuristicFailure", "method": "heuristic"}
    if any(m in lowered for m in done_markers):
        return {"claimed_done": True, "stop_class": "HeuristicSuccess", "method": "heuristic"}
    return {"claimed_done": None, "stop_class": "Unknown", "method": "heuristic"}


def load_cases(ids):
    manifest = yaml.safe_load(MANIFEST.read_text())
    rows = []
    for entry in manifest["cases"]:
        if ids and entry["id"] not in ids:
            continue
        case = yaml.safe_load((ROOT / entry["path"]).read_text())
        case["_meta"] = entry
        rows.append(case)
    return rows


def materialize(case, ws: Path):
    """Mirror eval_cmd.rs: local clone of the fixture (or bare files), overlay,
    drop origin, commit the baseline."""
    ws.mkdir(parents=True, exist_ok=False)

    def git(*args):
        subprocess.run(["git", *args], cwd=ws, check=False, capture_output=True)

    if case.get("repo"):
        src = ROOT / case["repo"]
        subprocess.run(
            ["git", "clone", "--local", "--quiet", str(src), str(ws)],
            check=True, capture_output=True,
        )
        if case.get("base_ref"):
            git("checkout", "--quiet", case["base_ref"])
        git("remote", "remove", "origin")
    else:
        git("init", "-q")
    for rel, content in (case.get("files") or {}).items():
        p = ws / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content)
    git("config", "user.email", "eval@leveler")
    git("config", "user.name", "comparative-eval")
    git("add", "-A")
    git("commit", "-qm", "eval baseline")


def write_dsh_patch(home: Path, model: str) -> Path:
    home.mkdir(parents=True, exist_ok=True)
    patch = home / "patch.yml"
    patch.write_text(f"""\
- id: llm-pi-ai
  config:
    providers:
      taotoken:
        displayName: Taotoken Gateway
        apiKeyEnv: DEEPSEEK_API_KEY
        api: openai-completions
        baseURL: https://taotoken.net/api/v1
        compat:
          thinkingFormat: deepseek
        models:
          - id: {model}
            name: {model}
            contextWindow: 1000000
            maxTokens: 8192
- id: agent-default-model
  config:
    provider: taotoken
    model: {model}
""")
    return patch


def launch(
    arm,
    model,
    ws: Path,
    task: str,
    iso: Path,
    *,
    leveler_bin=None,
    leveler_config=None,
    atomcode_bin=None,
    dsh_root=None,
    atomcode_model_override=False,
):
    """Return (argv, env, cwd) for the harness's own headless CLI."""
    # Popen cwd is the workspace. Relative --repo / -C would then be resolved
    # against the workspace itself and fail to canonicalize (HC-001 abort #1).
    ws = Path(ws).resolve()
    iso = Path(iso).resolve()
    if arm == "leveler":
        home = iso / "leveler-home"
        home.mkdir(parents=True, exist_ok=True)
        src = Path(leveler_config) if leveler_config else LEVELER_CONFIG
        shutil.copy(src, home / "config.toml")
        bin_path = Path(leveler_bin) if leveler_bin else LEVELER_BIN
        pinned = model or HC001_MODEL
        return (
            [str(bin_path), "run", task, "--repo", str(ws), "--model", pinned, "--auto-approve"],
            clean_env({"LEVELER_HOME": str(home), "RUSTC_WRAPPER": ""}),
            ws,
        )
    if arm == "atomcode":
        # Real user config (~/.atomcode) by the user's explicit direction.
        # Do NOT pass --model on the default comparative lane.
        bin_path = Path(atomcode_bin) if atomcode_bin else ATOMCODE_BIN
        argv = [str(bin_path), "-p", task, "-C", str(ws), "-y", "-v", "--dev", "--no-telemetry"]
        if atomcode_model_override and model:
            argv += ["--model", model]
        return (argv, clean_env(), ws)
    if arm == "dsh":
        root = Path(dsh_root).resolve() if dsh_root else DSH_ROOT.resolve()
        home = iso / "dsh-home"
        wire = (model or HC001_MODEL).split("/")[-1]
        patch = write_dsh_patch(home, wire)
        return (
            ["node", "--import",
             (root / "node_modules/tsx/dist/esm/index.mjs").as_uri(),
             str(root / "apps/cli/src/bin.ts"),
             "--profile", "headless", "--patch", str(patch), task],
            clean_env({
                "DSH_HOME": str(home),
                "DSH_PERMISSION_MODE": "danger-full-access",
                "TSX_TSCONFIG_PATH": str(root / "tsconfig.json"),
            }),
            ws,
        )
    raise SystemExit(f"unknown arm {arm}")


def classify_harness_exit(arm, rc, timed_out, output, expect_ok, wall_seconds):
    """Startup/adapter failures are INFRA_FAILURE, never a task FAIL."""
    blob = output or ""
    if timed_out:
        return "TIMEOUT_FAIL" if not expect_ok else "PASS"
    infra_markers = (
        "failed to canonicalize workspace root",
        "no such file or directory",
        "cannot find module",
        "err_module_not_found",
        "could not resolve",
        "unknown option",
        "unrecognized option",
        "invalid value for",
    )
    low = blob.lower()
    if rc not in (0, None) and any(m in low for m in infra_markers) and wall_seconds is not None and wall_seconds < 15:
        return "INFRA_FAILURE"
    if expect_ok:
        return "PASS"
    return "FAIL"


def run_expect(case, ws: Path):
    expect = case["expect"]
    started = time.time()
    try:
        proc = subprocess.run(
            [expect["program"], *expect.get("args", [])],
            cwd=ws, capture_output=True, text=True, timeout=900, env=clean_env(),
        )
        tail = redact_text((proc.stderr or "") + "\n" + (proc.stdout or ""))[-2000:]
        return proc.returncode == 0, proc.returncode, tail, time.time() - started
    except subprocess.TimeoutExpired:
        return False, -1, "expect timed out", time.time() - started


def _cmd_text(argv, cwd=None) -> str:
    try:
        proc = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=30)
        return (proc.stdout or proc.stderr or "").strip()
    except Exception as exc:  # noqa: BLE001 — preflight must not crash the batch
        return f"ERROR {exc}"


def preflight() -> dict:
    atomcode_ver = _cmd_text([str(ATOMCODE_BIN), "--version"])
    leveler_ver = _cmd_text([str(LEVELER_BIN), "--version"])
    dsh_sha = _cmd_text(["git", "rev-parse", "HEAD"], cwd=DSH_ROOT)
    dsh_pkg = ""
    pkg = DSH_ROOT / "apps/cli/package.json"
    if pkg.exists():
        dsh_pkg = json.loads(pkg.read_text()).get("version", "")
    atomcode_cfg_hash = file_sha256(ATOMCODE_CONFIG) if ATOMCODE_CONFIG.exists() else None
    result = {
        "leveler_bin": str(LEVELER_BIN),
        "leveler_version": leveler_ver,
        "leveler_frozen_sha": FROZEN_LEVELER_SHA,
        "atomcode_bin": str(ATOMCODE_BIN),
        "atomcode_version": atomcode_ver,
        "atomcode_frozen": f"{FROZEN_ATOMCODE_VERSION} ({FROZEN_ATOMCODE_SHA})",
        "atomcode_config_sha256": atomcode_cfg_hash,
        "dsh_root": str(DSH_ROOT),
        "dsh_sha": dsh_sha,
        "dsh_version": dsh_pkg,
        "dsh_frozen_sha": FROZEN_DSH_SHA,
        "dsh_frozen_version": FROZEN_DSH_VERSION,
        "atomcode_version_drift": "NO" if FROZEN_ATOMCODE_VERSION in atomcode_ver and FROZEN_ATOMCODE_SHA in atomcode_ver else "YES",
        "dsh_version_drift": "NO" if dsh_sha.startswith(FROZEN_DSH_SHA) and dsh_pkg == FROZEN_DSH_VERSION else "YES",
        "codeleveler_version_drift": "NO" if FROZEN_LEVELER_SHA[:12] in leveler_ver and "dirty" not in leveler_ver.lower() else "YES",
    }
    return result


def collect_leveler_metrics(home: Path) -> dict:
    lib = ROOT / "eval" / "lib"
    if str(lib) not in sys.path:
        sys.path.insert(0, str(lib))
    try:
        from eventlog import extract_path, find_session_dbs  # type: ignore
    except Exception as exc:  # noqa: BLE001
        return {"error": str(exc)}
    dbs = find_session_dbs(home)
    if not dbs:
        return {"sessions_db": None}
    scored = extract_path(dbs[0])
    return {
        "sessions_db": str(dbs[0]),
        "input_tokens": scored.get("input_tokens"),
        "output_tokens": scored.get("output_tokens"),
        "total_tokens": scored.get("total_tokens"),
        "rounds": scored.get("rounds"),
        "parent_tool_names": scored.get("parent_tool_names"),
        "parent_mutations": scored.get("parent_mutations"),
        "first_edit_round": scored.get("first_edit_round"),
        "spawn": scored.get("spawn"),
        "delegated": scored.get("delegated"),
        "task_outcome": scored.get("task_outcome"),
        "verification_passed": scored.get("verification_passed"),
        "child_result_used": scored.get("child_result_used"),
    }


def parse_token_heuristic(arm: str, text: str) -> dict:
    """Best-effort token scrape from verbose logs. Missing → None, never 0."""
    blob = text or ""
    patterns = [
        re.compile(r"input[_\s-]?tokens?\s*[:=]\s*(\d+).*?output[_\s-]?tokens?\s*[:=]\s*(\d+)", re.I | re.S),
        re.compile(r"prompt[_\s-]?tokens?\s*[:=]\s*(\d+).*?completion[_\s-]?tokens?\s*[:=]\s*(\d+)", re.I | re.S),
        re.compile(r"tokens?\s*[:=]\s*in\s*=\s*(\d+)\s*out\s*=\s*(\d+)", re.I),
        re.compile(r"in:\s*(\d+).*?out:\s*(\d+)", re.I | re.S),
    ]
    inp = out = None
    for pat in patterns:
        matches = pat.findall(blob)
        if not matches:
            continue
        # sum all captured pairs if the log prints per-turn usage
        total_in = total_out = 0
        for a, b in matches:
            total_in += int(a)
            total_out += int(b)
        inp, out = total_in, total_out
        break
    return {
        "input_tokens": inp,
        "output_tokens": out,
        "total_tokens": None if inp is None or out is None else inp + out,
        "token_source": f"{arm}-log-heuristic" if inp is not None else None,
    }


def run_one(arm, model, case, rep, out_dir: Path):
    case_id = case["id"]
    iso = evidence_iso(out_dir, arm, case_id, rep)
    ws = iso / "ws"
    if iso.exists():
        shutil.rmtree(iso)
    materialize(case, ws)
    (iso / "prompt.txt").write_text(case["task"])
    baseline_ok, _, baseline_tail, _ = run_expect(case, ws)
    if baseline_ok:
        return dict(
            case=case_id, arm=arm, model=model, rep=rep,
            status="UNJUDGEABLE_BASELINE_GREEN",
            evidence=str(iso),
            prompt_sha256=text_sha256(case["task"]),
        )
    # the probe may leave build artifacts; the harness starts from a clean tree
    subprocess.run(["git", "clean", "-qfdx"], cwd=ws, capture_output=True)
    subprocess.run(["git", "checkout", "--quiet", "--", "."], cwd=ws, capture_output=True)
    argv, env, cwd = launch(arm, model, ws, case["task"], iso)
    recorded_argv = [a if a != case["task"] else "<PROMPT>" for a in argv]
    (iso / "argv.json").write_text(json.dumps({
        "argv": recorded_argv,
        "cwd": str(cwd),
        "env_names": sorted(k for k in env if k in (
            "LEVELER_HOME", "DSH_HOME", "DSH_PERMISSION_MODE", "TSX_TSCONFIG_PATH", "RUSTC_WRAPPER",
        ) or k.startswith("LEVELER_") or k.startswith("DSH_")),
        "prompt_sha256": text_sha256(case["task"]),
    }, indent=2) + "\n")
    timeout = case["_meta"]["timeout_seconds"]
    started = time.time()
    started_iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(started))
    timed_out = False
    try:
        proc = subprocess.Popen(
            argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, start_new_session=True,
        )
        try:
            output, _ = proc.communicate(timeout=timeout)
            rc = proc.returncode
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(proc.pid, signal.SIGKILL)
            output, _ = proc.communicate()
            rc = -9
    except Exception as e:  # adapter/infra failure, never a task FAIL
        return dict(case=case_id, arm=arm, model=model, rep=rep,
                    status="INFRA_FAILURE", error=str(e), evidence=str(iso))
    wall = time.time() - started
    redacted = redact_text(output or "")
    (iso / "harness-output.log").write_text(redacted)
    diff = subprocess.run(["git", "diff", "HEAD"], cwd=ws, capture_output=True, text=True).stdout
    diff_stat = subprocess.run(["git", "diff", "--stat", "HEAD"], cwd=ws,
                               capture_output=True, text=True).stdout
    status_files = subprocess.run(["git", "status", "--porcelain"], cwd=ws,
                                  capture_output=True, text=True).stdout
    (iso / "git.diff").write_text(diff)
    (iso / "git.status").write_text(status_files)
    ok, expect_rc, expect_tail, expect_secs = run_expect(case, ws)
    claim = parse_claimed_completion(arm, redacted)
    false_completion = None
    if claim["claimed_done"] is True and not ok:
        false_completion = True
    elif claim["claimed_done"] is False:
        false_completion = False
    elif claim["claimed_done"] is True and ok:
        false_completion = False

    metrics = {"input_tokens": None, "output_tokens": None, "total_tokens": None}
    if arm == "leveler":
        metrics.update(collect_leveler_metrics(iso / "leveler-home"))
    else:
        metrics.update(parse_token_heuristic(arm, redacted))

    tool_calls = None
    if isinstance(metrics.get("parent_tool_names"), list):
        tool_calls = len(metrics["parent_tool_names"])

    row = dict(
        case=case_id, arm=arm, model=model or ("real-default" if arm == "atomcode" else HC001_MODEL),
        rep=rep,
        status=classify_harness_exit(arm, rc, timed_out, redacted, ok, wall),
        expect_passed=ok, expect_rc=expect_rc,
        hidden_passed=ok,
        harness_rc=rc, timed_out=timed_out,
        wall_seconds=round(wall, 1), expect_seconds=round(expect_secs, 1),
        started_at=started_iso,
        changed_files=len([l for l in status_files.splitlines() if l.strip()]),
        diff_stat_tail=diff_stat.strip().splitlines()[-1] if diff_stat.strip() else "",
        output_tail=redacted[-1200:],
        expect_tail=expect_tail if not ok else "",
        baseline_was_red=True,
        prompt_sha256=text_sha256(case["task"]),
        claimed_done=claim["claimed_done"],
        stop_class=claim["stop_class"],
        claim_method=claim["method"],
        false_completion=false_completion,
        user_rescue=0,
        input_tokens=metrics.get("input_tokens"),
        output_tokens=metrics.get("output_tokens"),
        total_tokens=metrics.get("total_tokens"),
        token_source=metrics.get("token_source") or ("leveler-eventlog" if arm == "leveler" else None),
        tool_calls=tool_calls,
        delegated=metrics.get("delegated"),
        rounds=metrics.get("rounds"),
        evidence=str(iso),
        timeout_seconds=timeout,
    )
    (iso / "result.json").write_text(json.dumps(row, ensure_ascii=False, indent=2) + "\n")
    return row


def _write_preflight(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2) + "\n")


def write_hc002_run_manifest(dest: Path, case: dict, prompt_sha: str) -> Path:
    """Freeze the six-run schedule before any paid execution."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    rows = []
    for arm, rep in HC002_ORDER:
        rows.append({
            "run_id": f"hc-002-{arm}-r{rep}",
            "arm": arm,
            "rep": rep,
            "case": HC002_CASE,
            "model": "real-default" if arm == "atomcode" else HC001_MODEL,
            "timeout_seconds": case["_meta"]["timeout_seconds"],
            "prompt_sha256": prompt_sha,
            "status": "PREPARED",
        })
    payload = {
        "suite": "hc-002",
        "case": HC002_CASE,
        "paid_runs": "HOLD",
        "hold_reason": "Completion Reconciliation / icg-6r not accepted; CODELEVELER_EVAL_BASELINE still 3b400357",
        "timeout_seconds": case["_meta"]["timeout_seconds"],
        "prompt_sha256": prompt_sha,
        "order": rows,
        "token_ranking": "DISABLED",
        "token_fairness": "LIMITED",
        "permission_fairness": "ACCEPTABLE",
        "model_upstream_match": "PARTIAL",
    }
    dest.write_text(json.dumps(payload, indent=2) + "\n")
    return dest


def prepare_hc002(evidence: Path) -> dict:
    """Materialize icg-5, prove baseline red, freeze the run list. No model calls."""
    cases = load_cases([HC002_CASE])
    if len(cases) != 1:
        raise SystemExit(f"HC-002 case {HC002_CASE} not in comparative manifest")
    case = cases[0]
    timeout = case["_meta"]["timeout_seconds"]
    if timeout != HC002_TIMEOUT_SECONDS:
        raise SystemExit(f"HC-002 timeout drifted: manifest {timeout} vs freeze {HC002_TIMEOUT_SECONDS}")
    prompt_sha = text_sha256(case["task"])
    (evidence / "prompt.txt").write_text(case["task"])
    ws = evidence / "baseline-ws"
    if ws.exists():
        shutil.rmtree(ws)
    materialize(case, ws)
    ok, rc, tail, secs = run_expect(case, ws)
    baseline = {
        "case": HC002_CASE,
        "baseline_ok": ok,
        "expect_rc": rc,
        "expect_seconds": round(secs, 2),
        "prompt_sha256": prompt_sha,
        "timeout_seconds": timeout,
        "fixture_repo": case.get("repo"),
        "BASELINE_RED": "YES" if not ok else "NO",
        "expect_tail": (tail or "")[-800:],
    }
    _write_preflight(evidence / "baseline.json", baseline)
    if ok:
        raise SystemExit("HC-002 UNJUDGEABLE_BASELINE_GREEN: icg-5 expect is already green")
    write_hc002_run_manifest(evidence / "run-manifest.json", case, prompt_sha)
    dummy = evidence / "dummy-leveler-config.toml"
    dummy.write_text('default_model = "deepseek/deepseek-v4-flash"\n')
    smoke_iso = evidence / "adapter-smoke-iso"
    smoke_iso.mkdir(parents=True, exist_ok=True)
    argv, env, cwd = launch(
        "leveler", HC001_MODEL, Path(os.path.relpath(ws)), case["task"],
        smoke_iso,
        leveler_bin=Path("/bin/echo"),
        leveler_config=dummy,
    )
    adapter = {
        "ADAPTER_PATH_REGRESSION": "PASS" if Path(argv[argv.index("--repo") + 1]).is_absolute() else "FAIL",
        "repo": argv[argv.index("--repo") + 1],
        "cwd": str(cwd),
        "LEVELER_HOME": env.get("LEVELER_HOME"),
    }
    atom_argv, atom_env, atom_cwd = launch(
        "atomcode", "", ws, case["task"], evidence / "adapter-smoke-iso",
        atomcode_bin=Path("/bin/echo"),
    )
    adapter["atomcode_C"] = atom_argv[atom_argv.index("-C") + 1]
    adapter["atomcode_C_absolute"] = Path(atom_argv[atom_argv.index("-C") + 1]).is_absolute()
    adapter["atomcode_has_model_flag"] = "--model" in atom_argv
    _write_preflight(evidence / "adapter-path.json", adapter)
    if adapter["ADAPTER_PATH_REGRESSION"] != "PASS" or not adapter["atomcode_C_absolute"]:
        raise SystemExit("ADAPTER_PATH_REGRESSION=FAIL")
    return {
        "case": HC002_CASE,
        "prompt_sha256": prompt_sha,
        "BASELINE_RED": "YES",
        "ADAPTER_PATH_REGRESSION": "PASS",
        "HC002_PAID_RUNS": "HOLD",
        "timeout_seconds": timeout,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", default="")  # e.g. leveler:deepseek/deepseek-v4-flash
    ap.add_argument("--cases", default="")
    ap.add_argument("--rep", type=int, default=1)
    ap.add_argument("--rep-id", type=int, default=0, help="Run a single repetition number (1-based)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--evidence-dir", default="")
    ap.add_argument("--hc001", action="store_true")
    ap.add_argument("--hc002", action="store_true")
    ap.add_argument("--prepare-only", action="store_true",
                    help="HC-002: freeze case + baseline-red + run list; no model calls")
    ap.add_argument("--skip-preflight", action="store_true")
    args = ap.parse_args()
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(args.evidence_dir) if args.evidence_dir else out.parent / (out.stem + "-evidence")
    evidence.mkdir(parents=True, exist_ok=True)

    if not args.skip_preflight:
        pf = preflight()
        _write_preflight(evidence / "preflight.json", pf)
        if args.hc001 or True:
            print(json.dumps({k: pf[k] for k in (
                "leveler_version", "atomcode_version", "dsh_sha", "dsh_version",
                "codeleveler_version_drift", "atomcode_version_drift", "dsh_version_drift",
                "atomcode_config_sha256",
            )}, indent=2), flush=True)
        if args.hc001:
            for key in ("codeleveler_version_drift", "atomcode_version_drift", "dsh_version_drift"):
                if pf[key] != "NO":
                    raise SystemExit(f"STOP: {key}={pf[key]} preflight={evidence / 'preflight.json'}")
        if args.hc002:
            for key in ("atomcode_version_drift", "dsh_version_drift"):
                if pf[key] != "NO":
                    raise SystemExit(f"STOP: {key}={pf[key]} preflight={evidence / 'preflight.json'}")

    if args.hc002 and args.prepare_only:
        prep = prepare_hc002(evidence)
        _write_preflight(evidence / "prepare.json", prep)
        print(json.dumps(prep, indent=2), flush=True)
        print("HC002_PAID_RUNS=HOLD", flush=True)
        return

    if args.hc002:
        hc002_paid_gate(os.environ.get("CODELEVELER_EVAL_BASELINE"))
        if not args.skip_preflight:
            pf = preflight()
            if pf["codeleveler_version_drift"] != "NO":
                raise SystemExit(
                    f"STOP: codeleveler_version_drift={pf['codeleveler_version_drift']}"
                )

    schedule = []
    if args.hc001:
        for arm, rep in HC001_ORDER:
            model = "" if arm == "atomcode" else HC001_MODEL
            schedule.append((arm, model, HC001_CASE, rep))
    elif args.hc002:
        for arm, rep in HC002_ORDER:
            model = "" if arm == "atomcode" else HC001_MODEL
            schedule.append((arm, model, HC002_CASE, rep))
    else:
        if not args.arm:
            raise SystemExit("--arm is required unless --hc001/--hc002")
        arm, _, model = args.arm.partition(":")
        ids = [x for x in args.cases.split(",") if x]
        reps = [args.rep_id] if args.rep_id else list(range(1, args.rep + 1))
        for case in load_cases(ids):
            for rep in reps:
                schedule.append((arm, model, case["id"], rep))

    # group load to avoid re-reading yaml per row
    needed = sorted({cid for _, _, cid, _ in schedule})
    cases = {c["id"]: c for c in load_cases(needed)}

    before_hash = file_sha256(ATOMCODE_CONFIG) if ATOMCODE_CONFIG.exists() else None
    with out.open("a") as sink:
        for arm, model, case_id, rep in schedule:
            case = cases[case_id]
            row = run_one(arm, model, case, rep, evidence)
            sink.write(json.dumps(row, ensure_ascii=False) + "\n")
            sink.flush()
            print(f"[{arm}:{row.get('model')}] {row['case']} r{rep}: {row['status']} "
                  f"({row.get('wall_seconds', '?')}s)", flush=True)
    after_hash = file_sha256(ATOMCODE_CONFIG) if ATOMCODE_CONFIG.exists() else None
    integrity = {
        "atomcode_config_sha256_before": before_hash,
        "atomcode_config_sha256_after": after_hash,
        "ATOMCODE_REAL_CONFIG_UNCHANGED": "YES" if before_hash == after_hash else "NO",
    }
    _write_preflight(evidence / "atomcode-config-integrity.json", integrity)
    if integrity["ATOMCODE_REAL_CONFIG_UNCHANGED"] != "YES":
        print("WARNING: ~/.atomcode/config.toml hash changed during the batch", flush=True)


if __name__ == "__main__":
    main()
