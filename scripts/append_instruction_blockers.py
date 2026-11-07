#!/usr/bin/env python3
"""Pre-registered real-PTY experiments for the appended-instruction blockers.

Normal B/G/N runs use the unchanged production daemon, TUI and provider path.
C removes one exact model-facing runtime pin in a recording diagnostic proxy;
it is a counterfactual experiment and never final production acceptance.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
ORIGINAL_PROMPT = (
    "优化 Access 页面布局。请先运行项目现有测试了解基线："
    "python3 -m unittest discover -s tests -v（timeout_seconds=180）。"
    "然后按 tests/test_access_layout.py 的契约完成布局改造，并实际请求页面确认。"
    "不要只回复说明，要实际修改和验证。"
)
ORIGINAL_STEER = "还有管理员管理也没有。"
RELATED_STEER = "管理员管理也一起补上吧，和这次 Access 页面改造一并完成。"
HEALTH_STEER = (
    '另外，虽然与布局不同，请在本次任务一并增加 /health 接口，'
    '返回 JSON {"status":"ok"}，并实际请求确认。'
)
ARMS = {
    "B": {"label": "original", "prompt": ORIGINAL_PROMPT, "steer": ORIGINAL_STEER, "diagnostic": False},
    "C": {"label": "without-pin", "prompt": ORIGINAL_PROMPT, "steer": ORIGINAL_STEER, "diagnostic": True},
    "G": {"label": "goal-entry", "prompt": "/goal " + ORIGINAL_PROMPT, "steer": ORIGINAL_STEER, "diagnostic": False},
    "N1": {"label": "related-explicit", "prompt": ORIGINAL_PROMPT, "steer": RELATED_STEER, "diagnostic": False},
    "N2": {"label": "different-explicit", "prompt": ORIGINAL_PROMPT, "steer": HEALTH_STEER, "diagnostic": False},
}
SCHEDULE = [("B", 1), ("C", 1), ("B", 2), ("C", 2), ("B", 3), ("C", 3),
            ("G", 1), ("G", 2), ("G", 3), ("N1", 1), ("N2", 1)]


def sha_bytes(value):
    return hashlib.sha256(value).hexdigest()


def sha_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def pin_text(objective):
    return ("[Active objective — host-pinned]\n<objective>\n" + objective.strip()
            + "\n</objective>\nThis is the active request for this turn.")


def without_original_pin(request, objective):
    """Change only an exact user-role pin copy, preserving all other rows."""
    changed = copy.deepcopy(request)
    messages = changed.get("messages")
    if not isinstance(messages, list):
        raise ValueError("diagnostic request must have a messages array")
    indices = [i for i, message in enumerate(messages)
               if isinstance(message, dict) and message.get("role") == "user"
               and message.get("content") == pin_text(objective)]
    if len(indices) > 1:
        raise ValueError("ambiguous duplicate runtime pins; diagnostic request refused")
    if indices:
        del messages[indices[0]]
    return changed, indices


def valid_health_response(page):
    """Accept the requested live HTTP/JSON contract, not a text mention."""
    if page.get("status") != 200 or str(page.get("type", "")).split(";", 1)[0].strip().lower() != "application/json":
        return False
    try:
        body = json.loads(page.get("body", ""))
    except (TypeError, ValueError):
        return False
    return isinstance(body, dict) and body.get("status") == "ok"


def load_phase3():
    spec = importlib.util.spec_from_file_location("blockers_phase3", ROOT / "scripts/append_instruction_phase3.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def support_hashes():
    return {relative: sha_file(ROOT / relative) for relative in (
        "scripts/append_instruction_phase3.py", "scripts/append_instruction_dogfood.py", "scripts/pty_dogfood.py")}


def fixture_hash():
    root = ROOT / "testdata/append-instruction-dogfood/access-portal"
    files = {str(path.relative_to(root)): sha_file(path) for path in sorted(root.rglob("*"))
             if path.is_file() and not any(part in (".git", "__pycache__") for part in path.parts)
             and path.name != ".slow_started"}
    return sha_bytes(canonical(files))


def source_identity():
    def git(*args):
        return subprocess.check_output(["git", *args], cwd=ROOT)
    production = {}
    for raw in git("ls-files", "--cached", "--others", "--exclude-standard", "-z").split(b"\0"):
        if not raw:
            continue
        relative = os.fsdecode(raw)
        path = ROOT / relative
        relevant = relative in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml") or (
            relative.startswith("crates/") and ("/src/" in relative or "/prompts/" in relative
            or "/migrations/" in relative or path.name in ("Cargo.toml", "build.rs")))
        if relevant and path.is_file():
            production[relative] = sha_file(path)
    return {"head": git("rev-parse", "HEAD").decode().strip(),
            "working_diff_sha256": sha_bytes(git("diff", "--binary", "HEAD")),
            "production_content_sha256": sha_bytes(canonical(production))}


def write_json(path, value, mode="x"):
    with open(path, mode, encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2)


def register_plan(root, config):
    root.mkdir(parents=True, exist_ok=True)
    if root == ROOT / "target" or ROOT / "target" in root.parents:
        raise ValueError("experiment evidence must be outside target/")
    plan = {"version": 1, "registered_at": time.time(), "arms": ARMS, "schedule": SCHEDULE,
            "source_config_path": str(config), "source_config_sha256": sha_file(config),
            "fixture_sha256": fixture_hash(), "driver_sha256": sha_file(__file__),
            "support_script_sha256": support_hashes(),
            "expected_main_model": "deepseek-v4.1-flash-1017", "expected_reasoning_effort": "high",
            "decision_rule": "Keep every production failure. C is diagnostic only. A small mixed sample does not establish a stable pin defect.",
            "pin_control": "Remove only the exact role=user original runtime pin copy; preserve original user task and all later user messages. Save original and actual outgoing bodies, hashes and position differences; never record headers.",
            "production_acceptance": ["B", "G", "N1", "N2"],
            "order": "Single-arm fresh processes, sequential pre-registered order; do not mutate a frozen driver between arms."}
    write_json(root / "EXPERIMENT_PLAN.json", plan)
    (root / "EXPERIMENT_PLAN.md").write_text(
        "# Appended-instruction blocker experiments\n\n"
        "Schedule: B1, C1, B2, C2, B3, C3, G1, G2, G3, N1, N2.\n\n"
        "B: original Access task and original append, unchanged production path.\n"
        "C: exact pin-removal counterfactual at the recording proxy; diagnostic only, never production acceptance.\n"
        "G: real /goal entry with the same task and original append.\n"
        "N1: naturally worded related addition; retain all original administrator/layout assertions.\n"
        "N2: explicitly different addition within this task; live /health HTTP200 JSON status=ok plus original layout/suite contracts.\n\n"
        "All runs retain receipt, journal, identity, provider body, PTY, SQLite, business evidence and failures. "
        "Model configuration, driver, production source, fixture and fixed binary changes invalidate a run. "
        "Source config contents and credentials are not archived. See EXPERIMENT_PLAN.json for exact inputs and hashes.\n",
        encoding="utf-8")
    return plan


def freeze_baseline(root, binary, build_record):
    plan_path = root / "EXPERIMENT_PLAN.json"
    plan = json.loads(plan_path.read_text())
    checks = [
        (sha_file(__file__) == plan["driver_sha256"], "driver changed after registration; register a new plan"),
        (sha_file(plan["source_config_path"]) == plan["source_config_sha256"], "source config changed after registration"),
        (fixture_hash() == plan["fixture_sha256"], "fixture changed after registration"),
        (support_hashes() == plan["support_script_sha256"], "support scripts changed after registration"),
        (plan["arms"] == ARMS and plan["schedule"] == [list(item) for item in SCHEDULE], "registered arm definitions changed"),
    ]
    for valid, reason in checks:
        if not valid:
            raise ValueError(reason)
    record = {"frozen_at": time.time(), "plan_sha256": sha_file(plan_path), "binary_path": str(binary),
              "binary_sha256": sha_file(binary), "source": source_identity(), "plan": plan,
              "build_record_path": str(build_record) if build_record else None,
              "build_record_sha256": sha_file(build_record) if build_record else None,
              "notice": "Binary and source identities are recorded separately; the build record binds their provenance."}
    write_json(root / "FIXED_BASELINE.json", record)
    return record


def install_run_classes(phase3, baseline, arm, repeat_index):
    """Install one process-local case binding; one CLI invocation runs one arm."""
    plan = baseline["plan"]
    config = Path(plan["source_config_path"])
    expected = {"binary": baseline["binary_sha256"], "config": plan["source_config_sha256"],
                "fixture": plan["fixture_sha256"], "driver": plan["driver_sha256"],
                "production": baseline["source"]["production_content_sha256"],
                "support_scripts": plan["support_script_sha256"]}

    def current():
        return {"binary": sha_file(baseline["binary_path"]), "config": sha_file(config),
                "fixture": fixture_hash(), "driver": sha_file(__file__),
                "production": source_identity()["production_content_sha256"], "support_scripts": support_hashes()}

    before = current()
    if before != expected:
        raise ValueError("fixed baseline changed before run: " + repr([key for key in expected if before[key] != expected[key]]))
    phase3.BIN = baseline["binary_path"]
    phase3.dog.source_config = lambda: str(config)
    original_proxy = phase3.Phase3Proxy

    class DiagnosticProxy(original_proxy):
        def handle(self, handler, body):
            request = json.loads(body.decode("utf-8"))
            changed, indices = without_original_pin(request, ORIGINAL_PROMPT)
            actual = json.dumps(changed, ensure_ascii=False, separators=(",", ":")).encode() if indices else body
            transform_id = uuid.uuid4().hex
            root = Path(self.path).parent
            for name in ("inbound-requests", "actual-outbound-requests", "pin-transform-diffs"):
                (root / name).mkdir(exist_ok=True)
            (root / "inbound-requests" / (transform_id + ".json")).write_bytes(body)
            (root / "actual-outbound-requests" / (transform_id + ".json")).write_bytes(actual)
            write_json(root / "pin-transform-diffs" / (transform_id + ".json"), {
                "diagnostic_only": True, "main_call": bool(request.get("tools")),
                "removed_message_indices": indices, "inbound_sha256": sha_bytes(body),
                "actual_outbound_sha256": sha_bytes(actual), "actual_canonical_sha256": sha_bytes(canonical(changed)),
                "roles_before": [m.get("role") for m in request.get("messages", [])],
                "roles_after": [m.get("role") for m in changed.get("messages", [])],
                "preserved_user_rows": [sha_bytes(canonical(m)) for i, m in enumerate(request.get("messages", []))
                                        if m.get("role") == "user" and i not in indices],
            })
            return super().handle(handler, actual)

    if arm == "C":
        phase3.Phase3Proxy = DiagnosticProxy
    original_case = phase3.Case
    original_finalize = phase3.dog.finalize_evidence

    class BoundCase(original_case):
        def __init__(self, name, fixture=None, model_limits=None):
            super().__init__("blockers-%s-%d-%s" % (arm, repeat_index, name), fixture=fixture, model_limits=model_limits)
            self.baseline_before = before
            # Normalize only the proxy URL on both copies; never archive config contents.
            source_text = config.read_text()
            copied = (Path(self.home) / "config.toml").read_text()
            normalize = lambda text: phase3.dog.rewrite_base_url(text, "https://experiment-config-comparison.invalid/")
            self.copied_config_matches = normalize(source_text) == normalize(copied)
            self.result.check("fixed_config_copy_matches_source", self.copied_config_matches, "only the proxy base_url differs")
            # The old finish closes all writers before invoking this callback.
            phase3.dog.finalize_evidence = self.finalize_experiment

        def finalize_experiment(self, directory):
            try:
                after = current()
            except (OSError, subprocess.CalledProcessError) as error:
                after = {"measurement_error": str(error)}
            self.result.check("fixed_baseline_unchanged", after == expected, repr([key for key in expected if after.get(key) != expected[key]]))
            mains = []
            for path in sorted((Path(self.dir) / "requests").glob("*.json")):
                request = json.loads(path.read_text())
                if request.get("tools"):
                    mains.append({"file": path.name, "model": request.get("model"),
                                  "reasoning_effort": request.get("reasoning_effort"),
                                  "canonical_sha256": sha_bytes(canonical(request))})
            model_valid = bool(mains) and all(
                m["model"] == plan["expected_main_model"] and m["reasoning_effort"] == plan["expected_reasoning_effort"]
                for m in mains)
            self.result.check("fixed_main_model_configuration", model_valid, json.dumps(mains, ensure_ascii=False))
            transforms = []
            if arm == "C":
                for path in sorted((Path(self.dir) / "pin-transform-diffs").glob("*.json")):
                    item = json.loads(path.read_text()); item["diff_file"] = str(path.relative_to(self.dir))
                    item["recorded_actual_request_files"] = [m["file"] for m in mains if m["canonical_sha256"] == item["actual_canonical_sha256"]]
                    transforms.append(item)
                main_transforms = [item for item in transforms if item["main_call"]]
                self.result.check("diagnostic_removed_exact_pin_from_every_main_call", bool(main_transforms) and all(
                    len(item["removed_message_indices"]) == 1 and item["recorded_actual_request_files"] for item in main_transforms),
                    "diagnostic only; not production acceptance")
            write_json(Path(self.dir) / "experiment.json", {
                "arm": arm, "repeat_index": repeat_index, "definition": ARMS[arm],
                "diagnostic_only": ARMS[arm]["diagnostic"], "fixed_baseline": baseline,
                "observed_before": self.baseline_before, "observed_after": after,
                "copied_config_matches_source": self.copied_config_matches,
                "baseline_valid": after == expected and self.copied_config_matches and model_valid,
                "invalidated_by": [key for key in expected if after.get(key) != expected[key]]
                                  + ([] if self.copied_config_matches else ["copied_config"])
                                  + ([] if model_valid else ["actual_main_model_configuration"]),
                "actual_main_requests": mains, "diagnostic_transform_mapping": transforms,
                "interpretation": "Delivery/mentions are not semantic completion. Independent reviewers must compare intent, goal amendments, plans, actual business results and the full final answer."})
            # Include integrity failures in this run's result before its first seal.
            self.result.write(os.path.join(directory, "result.json"))
            original_finalize(directory)

        def finish(self):
            try:
                super().finish()
            finally:
                phase3.dog.finalize_evidence = original_finalize

    phase3.Case = BoundCase
    phase3.ACCESS_PROMPT = ARMS[arm]["prompt"]
    phase3.ACCESS_STEER = ARMS[arm]["steer"]


def run_health_case(phase3, args):
    """A distinct user contract; it does not replace the administrator test."""
    original_paths = '("/access", "/roles", "/admin", "/static/app.css")'
    if phase3.ACCESS_PROBE_SRC.count(original_paths) != 1:
        raise ValueError("the production probe path declaration changed; health experiment refused")
    probe_src = phase3.ACCESS_PROBE_SRC.replace(original_paths,
        '("/access", "/roles", "/admin", "/static/app.css", "/health")', 1)
    case = phase3.Case("product-health", fixture=phase3.ACCESS_FIXTURE)
    result = case.result
    try:
        baseline = phase3.run_python_tests(case.repo, ["tests.test_access_layout"], timeout=120)
        write_json(Path(case.dir) / "baseline-tests.json", baseline)
        result.check("fixture_starts_red_on_layout", baseline["code"] != 0 and "FAILED" in baseline["out"] + baseline["err"], str(baseline["code"]))
        contracts = {name: sha_file(Path(case.repo) / "tests" / name) for name in ["test_access_layout.py", "test_pages.py", "test_00_slow.py"]}
        seed = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=case.repo).decode().strip()
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        tui = case.tui("A"); tui.spawn()
        sid = case.wait_session(); result.check("session_created", sid is not None, str(sid))
        tui.type_and_enter(ORIGINAL_PROMPT, settle=0.6)
        result.check("baseline_suite_started", bool(phase3.wait_until(lambda: os.path.exists(os.path.join(case.repo, ".slow_started")), 180)), case.repo)
        before = case.snapshot("before-steer") or {}
        result.check("turn_running_when_appended", before.get("status") == "running", str(before.get("status")))
        at = time.time(); tui.type_and_enter(HEALTH_STEER, settle=0.4)
        admitted = phase3.wait_until(lambda: case.journal(HEALTH_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check("journal_pending_while_running", bool(admitted and admitted[0]["state"] == "pending"), str(admitted))
        result.check("frame_shows_saved", case.frame_has(tui, "已保存 · 等待投递", 20), "real PTY")
        hit = case.wait_request_with(HEALTH_STEER, after=at, timeout=600)
        result.check("provider_received_independent_user_append", bool(hit), json.dumps(hit))
        result.check("frame_shows_delivered", case.frame_has(tui, "已进入当前任务", 30), "real PTY")
        tui.dump("steer-delivered")
        delivered = case.snapshot("steer-delivered") or {}
        result.check("delivered_inside_same_turn", bool(hit and before.get("latest_turn_ordinal") is not None
                     and delivered.get("latest_turn_ordinal") == before.get("latest_turn_ordinal")), json.dumps({"before": before.get("latest_turn_ordinal"), "after": delivered.get("latest_turn_ordinal")}))
        occurrences = case.occurrences(HEALTH_STEER, after=at)
        result.check("append_once_per_request", bool(hit) and all(row["count"] <= 1 for row in occurrences), json.dumps(occurrences))
        consumed = phase3.wait_until(lambda: (case.journal(HEALTH_STEER) or [{}])[0].get("state") == "consumed", 420)
        result.check("journal_consumed", bool(consumed), str(case.journal(HEALTH_STEER)))
        rows = case.user_message_rows(HEALTH_STEER)
        result.check("one_transcript_row", len(rows) == 1, str(len(rows)))
        result.check("row_keeps_journal_identity", bool(rows and admitted and rows[0].get("input_id") == admitted[0]["id"]), str(rows and rows[0].get("input_id")))
        terminal = case.wait_all_turns_terminal(at, timeout=900)
        result.check("turn_completed", bool(terminal and terminal.get("status") == "completed"), str(terminal and terminal.get("status")))
        tui.dump("terminal")
        # Reuse the actual loopback server probe with one additional GET path.
        proc = subprocess.run([sys.executable, "-c", probe_src], cwd=case.repo, capture_output=True, text=True, timeout=60)
        probe = json.loads(proc.stdout.strip().splitlines()[-1]) if proc.returncode == 0 and proc.stdout.strip() else {}
        write_json(Path(case.dir) / "health-http-probe.json", {"probe": probe, "exit_code": proc.returncode, "stderr": proc.stderr})
        result.check("health_http_json_contract", valid_health_response((probe.get("pages") or {}).get("/health") or {}), json.dumps((probe.get("pages") or {}).get("/health"), ensure_ascii=False))
        for name, target in [("layout", ["tests.test_access_layout"]), ("suite", ["discover", "-s", "tests"])]:
            tests = phase3.run_python_tests(case.repo, target, timeout=300)
            write_json(Path(case.dir) / (name + "-tests.json"), tests)
            result.check(name + "_tests_green", tests["code"] == 0, str(tests["code"]))
        result.check("contract_tests_not_modified", all(sha_file(Path(case.repo) / "tests" / name) == digest for name, digest in contracts.items()), json.dumps(contracts))
        changed = subprocess.check_output(["git", "diff", "--name-only", seed], cwd=case.repo).decode().splitlines()
        new_paths = subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard"], cwd=case.repo).decode().splitlines()
        result.check("implementation_changed_project", any(not name.startswith("tests/") for name in changed + new_paths), json.dumps(changed + new_paths))
        access = ((probe.get("pages") or {}).get("/access") or {}).get("body", "")
        result.check("layout_keeps_navigation_and_stylesheet", '<nav' in access and 'rel="stylesheet"' in access and '/static/app.css' in access, "actual /access HTML")
    finally:
        case.finish()
    return 0 if all(item["ok"] for item in result.checks) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["register", "freeze", "run"])
    parser.add_argument("--archive-root", required=True, type=Path)
    parser.add_argument("--source-config", type=Path, default=Path.home() / ".leveler/config.toml")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--build-record", type=Path)
    parser.add_argument("--arm", choices=list(ARMS))
    parser.add_argument("--repeat-index", type=int, default=1)
    args = parser.parse_args(); root = args.archive_root.expanduser().resolve()
    if args.action == "register":
        register_plan(root, args.source_config.expanduser().resolve()); print(root / "EXPERIMENT_PLAN.json"); return 0
    if args.action == "freeze":
        if not args.binary:
            parser.error("freeze requires --binary")
        freeze_baseline(root, args.binary.expanduser().resolve(), args.build_record.resolve() if args.build_record else None)
        print(root / "FIXED_BASELINE.json"); return 0
    if not args.arm:
        parser.error("run requires --arm")
    if (args.arm, args.repeat_index) not in SCHEDULE:
        parser.error("arm/repeat is outside the pre-registered schedule")
    baseline_path = root / "FIXED_BASELINE.json"; baseline = json.loads(baseline_path.read_text())
    if sha_file(root / "EXPERIMENT_PLAN.json") != baseline["plan_sha256"]:
        raise ValueError("registered plan changed after freeze")
    os.environ["LEVELER_APPEND_EVIDENCE_ROOT"] = str(root / "runs")
    os.environ["LEVELER_BIN"] = baseline["binary_path"]
    phase3 = load_phase3()
    if phase3.ACCESS_PROMPT != ORIGINAL_PROMPT or phase3.ACCESS_STEER != ORIGINAL_STEER:
        raise ValueError("original production scenario changed")
    install_run_classes(phase3, baseline, args.arm, args.repeat_index)
    if args.arm == "N2":
        return run_health_case(phase3, args)
    return phase3.product_case(args, args.repeat_index - 1)


if __name__ == "__main__":
    sys.exit(main())
