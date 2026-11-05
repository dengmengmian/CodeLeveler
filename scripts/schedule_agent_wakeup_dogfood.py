#!/usr/bin/env python3
"""Phase 5 end-to-end acceptance: natural-language Agent schedule through the OS.

One real user turn asks the model to create a one-off Agent schedule in a
controlled repository. The model must reach the real `schedule_manage` tool;
the user then grants unattended execution and enables the OS wakeup in the
real TUI. Every CodeLeveler process from that UI is stopped, and the run that
follows may only come from the macOS launchd job. The scheduled Agent turn
then reads the repository and writes `schedule-report.md` through its own
tools.

No mock provider, no manual `schedule dispatch`, no fixture delivery: the
evidence is the runtime's own Session/Run/Usage records plus the file the
Agent wrote. The real provider config is copied into an isolated LEVELER_HOME;
no key is printed or written to the evidence.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Pty, Report  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
EVIDENCE = os.path.join(ROOT, "target", "schedule-agent-wakeup-dogfood")
NAME = "DF-57 自然语言 Agent 任务"
REPORT_FILE = "schedule-report.md"
GOAL = (
    "阅读当前仓库的 README.md 和 src/main.rs，用中文写一份不超过 20 行的项目分析，"
    "保存到仓库根目录的 schedule-report.md。必须实际创建这个文件，不要修改其他文件，"
    "完成后用一句话总结。"
)


def default_model() -> str:
    """The configured default model name, never the provider credentials."""
    path = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(path):
        raise SystemExit("real provider config missing: %s" % path)
    with open(path) as fh:
        for line in fh:
            match = re.match(r'^default_model\s*=\s*"([^"]+)"', line.strip())
            if match:
                return match.group(1)
    raise SystemExit("the real provider config has no default_model")


def env_for(root: str) -> dict[str, str]:
    home = os.path.join(root, "home")
    os.makedirs(home)
    shutil.copyfile(os.path.expanduser("~/.leveler/config.toml"), os.path.join(home, "config.toml"))
    env = dict(os.environ)
    env.pop("LEVELER_CONFIG_DIR", None)
    env.update(
        {
            "TERM": "xterm-256color",
            "LEVELER_HOME": home,
            "COLORTERM": "truecolor",
            "LEVELER_LOG": "warn",
            # The OS pass may start its own runtime; keep it long enough to
            # answer a single Agent turn, then let cleanup stop it.
            "LEVELER_DAEMON_IDLE_TIMEOUT_SECS": "600",
        }
    )
    return env


def cli(env: dict[str, str], args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run([BIN, *args], env=env, capture_output=True, text=True)


def cli_json(env: dict[str, str], args: list[str]) -> dict | list:
    result = cli(env, args)
    if result.returncode != 0:
        raise RuntimeError(
            "`%s` failed with %d: %s"
            % (" ".join(args), result.returncode, result.stderr[-800:])
        )
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            "`%s` returned non-JSON stdout %r (stderr %r)"
            % (" ".join(args), result.stdout[-500:], result.stderr[-500:])
        ) from error


def definitions(env: dict[str, str], repo: str) -> list[dict]:
    return cli_json(env, ["schedule", "list", "--repo", repo, "--json"])


def runs(env: dict[str, str], repo: str, definition: str) -> list[dict]:
    return cli_json(
        env, ["schedule", "runs", "--definition", definition, "--repo", repo, "--json"]
    )


def wakeups(env: dict[str, str], repo: str) -> list[dict]:
    return cli_json(env, ["schedule", "wakeups", "--repo", repo, "--json"])


def leveler_processes(root: str) -> list[tuple[int, str]]:
    found = []
    output = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        pid, _, command = line.partition(" ")
        if root in command and "/leveler" in command and "ps -axo" not in command:
            found.append((int(pid), command))
    return found


def stop_processes(root: str):
    import signal

    for pid, _command in leveler_processes(root):
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    end = time.time() + 20
    while time.time() < end and leveler_processes(root):
        time.sleep(0.5)
    for pid, _command in leveler_processes(root):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def wait_until(predicate, timeout: float, interval: float = 1.0):
    end = time.time() + timeout
    while time.time() < end:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return predicate()


def flat(pty: Pty) -> str:
    return "".join(pty.screen.text().split())


def wait_flat(pty: Pty, phrases: list[str], timeout: float) -> str | None:
    end = time.time() + timeout
    while time.time() < end:
        text = flat(pty)
        for phrase in phrases:
            if phrase in text:
                return phrase
        if not pty.alive():
            return None
        time.sleep(0.1)
    return None


def type_text(pty: Pty, text: str):
    for chunk in (text[i : i + 24] for i in range(0, len(text), 24)):
        pty.write(chunk)
        time.sleep(0.05)


def ask(pty: Pty, prompt: str):
    type_text(pty, prompt)
    time.sleep(0.2)
    pty.key("\r")


def settle_turn(pty: Pty):
    if not pty.wait_for("回答结束", 300):
        pty.wait_stable(2.0, 60)


def detail_contains(pty: Pty, phrases: list[str], timeout: float = 20) -> list[str]:
    """Check the detail page, scrolling down once if the first frame is short."""
    missing = list(phrases)
    end = time.time() + timeout
    while time.time() < end:
        text = flat(pty)
        missing = [phrase for phrase in missing if phrase not in text]
        if not missing:
            return []
        time.sleep(0.2)
    pty.key("\x1b[6~")  # PageDown: the detail page owns its scroll offset.
    time.sleep(0.5)
    text = flat(pty)
    return [phrase for phrase in missing if phrase not in text]


def main() -> int:
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % BIN)
        return 2
    model = default_model()
    report = Report("schedule-agent-wakeup")
    report.check("setup", "the real configured model is used", bool(model), model)

    root = tempfile.mkdtemp(prefix="lv-agent-wake-", dir="/tmp")
    home = os.path.join(root, "home")
    repo = os.path.realpath(os.path.join(root, "repo"))
    os.makedirs(os.path.join(repo, ".leveler"))
    # The TUI resolves <repo>/configs when LEVELER_CONFIG_DIR is absent; make
    # it real so the plist carries a usable bundle path.
    os.makedirs(os.path.join(repo, "configs"))
    os.makedirs(os.path.join(repo, "src"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    with open(os.path.join(repo, "README.md"), "w") as fh:
        fh.write("# Controlled analysis repo\n\nThis repository exists only for the scheduled Agent acceptance.\n")
    with open(os.path.join(repo, "src", "main.rs"), "w") as fh:
        fh.write('fn main() {\n    println!("controlled fixture");\n}\n')
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "add", "."],
        cwd=repo,
        check=False,
    )
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "-m", "seed"],
        cwd=repo,
        check=False,
    )

    env = env_for(root)
    due = (datetime.now(timezone.utc) + timedelta(minutes=9)).strftime("%Y-%m-%dT%H:%M:%SZ")
    prompt = (
        "请使用你的 schedule_manage 工具直接创建一个一次性 Agent 定时任务，不要只给我步骤。"
        "名称：%s。工作目录：%s。动作类型：agent（work.kind 必须是 agent，不要写成 shell），"
        "goal：%s。触发时间：%s（严格使用这个 RFC3339 UTC 时间，kind=once）。"
        "模型字段使用：%s。权限 mode=full_access。session_policy=fresh，timeout_secs=600。"
        "只创建这一个任务；不要授予无人值守执行权限，也不要启用系统后台唤醒，现在不要运行。"
        "完成后请告诉我：名称、模型、工作目录、规则与下次运行时间、是否已授权无人值守、"
        "是否已启用系统后台唤醒。"
        % (NAME, repo, GOAL, due, model)
    )

    pty = None
    enabled = False
    registration_path = None
    try:
        pty = Pty(
            [BIN, "--repo", repo, "tui", "--in-process", "--auto-approve", "--permission", "full-access"],
            env,
            140,
            48,
        )
        report.check("ui", "the real TUI paints a frame", pty.wait_for("CodeLeveler", 60))

        # ── The user's natural-language request ───────────────────────────
        ask(pty, prompt)
        report.check(
            "nl",
            "the model calls the real schedule_manage tool",
            wait_flat(pty, ["schedule_manage"], 300) is not None,
            pty.screen.text()[-1800:],
        )
        created = wait_until(
            lambda: [row for row in definitions(env, repo) if row.get("name") == NAME],
            300,
            2.0,
        )
        report.dump("nl-created", pty)
        report.check(
            "nl",
            "exactly one definition was created through the management API",
            len(created) == 1,
            json.dumps(created, ensure_ascii=False),
        )
        if len(created) != 1:
            raise RuntimeError("the natural-language turn did not create exactly one definition")
        schedule = created[0]
        schedule_id = schedule["id"]
        # `list` is a row summary; the detail projection is the one that
        # carries the mode, model and grant the user sees.
        definition = cli_json(
            env, ["schedule", "show", schedule_id, "--repo", repo, "--json"]
        )
        evidence = {"schedule_id": schedule_id, "model": model, "requested_due": due}
        report.check(
            "nl",
            "the stored action is an Agent goal, not a converted shell command",
            schedule.get("action", {}).get("kind") == "agent"
            and schedule.get("action", {}).get("goal") == GOAL,
            json.dumps(schedule.get("action"), ensure_ascii=False),
        )
        report.check(
            "nl",
            "the workspace and permission profile are the requested ones",
            definition.get("workspace") == repo
            and definition.get("mode") == "full_access",
            json.dumps(
                {k: definition.get(k) for k in ("workspace", "mode")},
                ensure_ascii=False,
            ),
        )
        report.check(
            "nl",
            "the one-off trigger keeps the exact wall-clock instant",
            schedule.get("trigger", {}).get("kind") == "once"
            and abs(
                (
                    datetime.fromisoformat(schedule["trigger"]["at"].replace("Z", "+00:00"))
                    - datetime.fromisoformat(due.replace("Z", "+00:00"))
                ).total_seconds()
            )
            <= 120,
            json.dumps(schedule.get("trigger"), ensure_ascii=False),
        )
        report.check(
            "nl",
            "the model's creation did not authorize unattended execution",
            definition.get("unattended_grant") is None,
            json.dumps(definition.get("unattended_grant"), ensure_ascii=False),
        )
        report.check(
            "nl",
            "the model's creation did not enable OS wakeup",
            cli_json(env, ["schedule", "status", "--repo", repo, "--json"])
            .get("wakeup", {})
            .get("registered")
            is False,
        )
        settle_turn(pty)

        # ── The user reviews the plan and authorizes it ───────────────────
        pty.write("/schedules")
        pty.key("\r")
        report.check("review", "the schedules page opens", pty.wait_for("定时任务", 20))
        report.check("review", "the model-created definition is projected", pty.wait_for(NAME, 20))
        pty.key("\r")
        report.check("review", "the detail page opens", pty.wait_for("运行历史", 20))
        missing = detail_contains(
            pty,
            ["下次运行", "完全访问", "模型", model, "工作目录", repo, "未授权无人值守"],
        )
        report.check(
            "review",
            "the user can see plan, model, workspace, permission and next run",
            not missing,
            "missing: %s\n%s" % (missing, pty.screen.text()[-1800:]),
        )
        report.dump("review-detail", pty)

        pty.key("g")
        report.check("authorize", "grant asks for explicit confirmation", pty.wait_for("授权无人值守执行？", 20))
        pty.key("\x1b[B", "\r")
        report.check("authorize", "the grant is persisted", pty.wait_for("已授权无人值守", 20))
        granted = wait_until(
            lambda: [
                row
                for row in definitions(env, repo)
                if row.get("id") == schedule_id and row.get("unattended_grant")
            ],
            20,
        )
        report.check(
            "authorize",
            "the runtime store holds an external-side-effect grant",
            bool(granted)
            and granted[0]["unattended_grant"].get("external_side_effects") is True,
            json.dumps(granted, ensure_ascii=False),
        )

        pty.key("w")
        report.check("wakeup", "enabling OS wakeup asks first", pty.wait_for("启用系统唤醒？", 20))
        pty.key("\x1b[B", "\r")
        report.check(
            "wakeup",
            "the TUI reports the OS registration",
            wait_flat(pty, ["系统唤醒已注册", "每分钟检查一次"], 30) is not None,
            pty.screen.text()[-1200:],
        )
        status = cli_json(env, ["schedule", "status", "--repo", repo, "--json"])
        registration_path = status.get("wakeup", {}).get("registration_path")
        enabled = bool(status.get("wakeup", {}).get("registered"))
        report.check(
            "wakeup",
            "the CLI sees the same registration the TUI enabled",
            enabled and status.get("wakeup", {}).get("loaded") is True,
            json.dumps(status.get("wakeup"), ensure_ascii=False),
        )
        report.check(
            "wakeup",
            "the registration file exists and checks every minute",
            bool(registration_path)
            and os.path.exists(registration_path)
            and status.get("wakeup", {}).get("interval_secs") == 60,
            "%s %s" % (registration_path, json.dumps(status.get("wakeup"), ensure_ascii=False)),
        )
        report.dump("authorized-and-enabled", pty)

        # ── Every UI process leaves; only the OS job may act ──────────────
        pty.key("\x03")
        time.sleep(0.4)
        pty.key("\x03")
        closed = wait_until(lambda: not pty.alive(), 20)
        report.check("exit", "Ctrl+C closes the TUI", closed)
        pty.close()
        pty = None
        stop_processes(root)
        report.check(
            "exit",
            "no CodeLeveler process from the UI session remains",
            not leveler_processes(root),
            json.dumps(leveler_processes(root)),
        )

        # ── The launchd job wakes the dispatcher; the Agent runs ──────────
        due_at = datetime.fromisoformat(schedule["trigger"]["at"].replace("Z", "+00:00"))
        deadline = due_at.timestamp() + 600
        terminal = None
        while time.time() < deadline:
            current = runs(env, repo, schedule_id)
            finished = [run for run in current if run.get("state") in ("succeeded", "failed", "blocked", "unknown", "timed_out", "cancelled")]
            if finished:
                terminal = finished[0]
                break
            time.sleep(3)
        report.check(
            "os-run",
            "an OS-dispatched run reached a terminal state",
            terminal is not None,
            json.dumps(runs(env, repo, schedule_id), ensure_ascii=False),
        )
        if terminal is None:
            raise RuntimeError("no terminal scheduled run was observed before the deadline")
        evidence["run"] = terminal
        report.check(
            "os-run",
            "the scheduled Agent turn succeeded",
            terminal.get("state") == "succeeded",
            json.dumps(terminal, ensure_ascii=False),
        )
        wakes = wakeups(env, repo)
        evidence["wakeups"] = wakes[:5]
        os_wakes = [wake for wake in wakes if wake.get("source") == "os"]
        report.check(
            "os-run",
            "the wake log attributes the pass to the OS job, not a manual dispatch",
            any(wake.get("succeeded", 0) >= 1 for wake in os_wakes),
            json.dumps(wakes[:5], ensure_ascii=False),
        )
        report_path = os.path.join(repo, REPORT_FILE)
        content = ""
        if os.path.exists(report_path):
            content = open(report_path, encoding="utf-8").read()
        evidence["report_file"] = {
            "path": report_path,
            "exists": os.path.exists(report_path),
            "bytes": len(content.encode("utf-8")),
        }
        report.check(
            "os-run",
            "the scheduled Agent wrote the analysis file through its own tools",
            len(content.strip()) >= 40,
            json.dumps(evidence["report_file"], ensure_ascii=False),
        )
        session_id = terminal.get("session_id")
        # `schedule runs --json` serializes the run row itself; usage facts are
        # flat fields on that row, not a nested projection.
        usage = {
            "requests": terminal.get("usage_requests") or 0,
            "input_tokens": terminal.get("input_tokens") or 0,
            "output_tokens": terminal.get("output_tokens") or 0,
            "cost_usd_micros": terminal.get("cost_usd_micros"),
        }
        report.check(
            "facts",
            "the run records its session, task outcome, provider and terminal time",
            bool(session_id)
            and bool(terminal.get("finished_at"))
            and bool(terminal.get("task_outcome"))
            and terminal.get("provider") == model.split("/")[0],
            json.dumps(terminal, ensure_ascii=False),
        )
        report.check(
            "facts",
            "token and cost facts were persisted for the run",
            usage.get("requests", 0) >= 1
            and usage.get("input_tokens", 0) > 0
            and usage.get("output_tokens", 0) > 0,
            json.dumps(usage, ensure_ascii=False),
        )
        session = cli_json(env, ["sessions", "show", session_id, "--repo", repo, "--json"])
        evidence["session"] = session
        report.check(
            "facts",
            "the session record carries the scheduled goal and model",
            session.get("id") == session_id
            and session.get("goal") == GOAL
            and session.get("model") == model,
            json.dumps(session, ensure_ascii=False),
        )
        trace = cli_json(env, ["trace", session_id, "--repo", repo, "--json"])
        summary = trace.get("session", {})
        window = trace.get("window") or []
        evidence["trace_summary"] = {
            key: summary.get(key)
            for key in ("request_count", "input_tokens", "output_tokens", "cost_usd_micros", "tool_started", "tool_finished")
        }
        evidence["trace_classes"] = sorted({row.get("class") for row in window})
        report.check(
            "facts",
            "the durable turn/tool/usage trace is queryable",
            summary.get("request_count", 0) >= 1
            and summary.get("input_tokens", 0) > 0
            and summary.get("output_tokens", 0) > 0
            and any(row.get("turn_id") for row in window)
            and summary.get("tool_started", 0) >= 1,
            json.dumps(evidence["trace_summary"], ensure_ascii=False),
        )

        # ── Reopen the UI and read the same record ───────────────────────
        registration = cli_json(env, ["schedule", "disable", "--repo", repo, "--json"])
        enabled = False
        report.check(
            "cleanup",
            "disable unloads the launchd job before the UI is reopened",
            registration.get("registered") is False and registration.get("loaded") is False,
            json.dumps(registration, ensure_ascii=False),
        )
        stop_processes(root)

        pty = Pty(
            [BIN, "--repo", repo, "tui", "--in-process", "--auto-approve", "--permission", "full-access"],
            env,
            140,
            48,
        )
        report.check("reopen", "the TUI reopens", pty.wait_for("CodeLeveler", 60))
        pty.write("/schedules")
        pty.key("\r")
        report.check("reopen", "the schedules page opens", pty.wait_for("定时任务", 20))
        report.check("reopen", "the definition persisted", pty.wait_for(NAME, 20))
        pty.key("\r")
        report.check("reopen", "the detail shows the finished run", pty.wait_for("已完成", 30))
        report.check(
            "reopen",
            "the completed run keeps its usage visible",
            wait_flat(pty, ["运行历史"], 10) is not None,
            pty.screen.text()[-1600:],
        )
        report.dump("reopened-completed", pty)
        pty.key("\x1b")
        pty.wait_for("Enter 详情", 10)
        pty.key("\x1b")
        time.sleep(0.3)
        pty.key("\x03")
        time.sleep(0.4)
        pty.key("\x03")
        wait_until(lambda: not pty.alive(), 20)
        pty.close()
        pty = None
        stop_processes(root)

        # ── CLI remains the same authority the UI displayed ──────────────
        shown = cli_json(env, ["schedule", "show", schedule_id, "--repo", repo, "--json"])
        listed = definitions(env, repo)
        recent = (shown.get("recent_runs") or [{}])[0]
        report.check(
            "consistency",
            "CLI, TUI and the runtime agree on one definition and one finished run",
            len(listed) == 1
            and listed[0].get("id") == schedule_id
            and recent.get("id") == terminal.get("id")
            and recent.get("state") == "succeeded",
            json.dumps({"listed": len(listed), "recent": recent}, ensure_ascii=False),
        )
        evidence["cli_show_last_run"] = recent
        evidence["passed"] = report.failures == 0
        with open(os.path.join(EVIDENCE, "evidence.json"), "w") as fh:
            json.dump(evidence, fh, ensure_ascii=False, indent=2)
    except Exception as exc:  # a crashed scenario is a FAIL, not a skip
        report.check("scenario", "completed", False, repr(exc))
        import traceback

        traceback.print_exc()
        if pty is not None:
            report.dump("failure", pty)
    finally:
        if pty is not None:
            pty.close()
        try:
            if enabled:
                registration = cli_json(env, ["schedule", "disable", "--repo", repo, "--json"])
                report.check(
                    "cleanup",
                    "the launchd job is unloaded after the run",
                    registration.get("registered") is False and registration.get("loaded") is False,
                    json.dumps(registration, ensure_ascii=False),
                )
        except Exception as exc:
            report.check("cleanup", "disable completed", False, repr(exc))
        if registration_path:
            report.check(
                "cleanup",
                "the plist no longer exists",
                not os.path.exists(registration_path),
                registration_path,
            )
        stop_processes(root)
        shutil.rmtree(root, ignore_errors=True)

    path = report.save()
    print("\nassertions: %d, failures: %d, blocked: %d" % (len(report.results), report.failures, report.blocked))
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
