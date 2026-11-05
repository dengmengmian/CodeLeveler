#!/usr/bin/env python3
"""Real-PTY acceptance for the TUI scheduled-automation page (Phase 5, DF-53).

The binary, the runtime, the reducer, the renderer and the schedule store are
real. The PTY screen is read back from a VT-100 model, so every assertion below
is about bytes a terminal receives. A CLI-seeded definition proves the page is
a projection of the runtime store; the page then owns grant, run-now, pause,
resume, create, edit and delete through its real key map. The OS wakeup is
never enabled here: pressing `w` only opens the confirmation, which is
cancelled, so this run cannot leave a system registration behind.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Pty, Report  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
EVIDENCE = os.path.join(ROOT, "target", "schedule-tui-dogfood")


def env_for(root: str) -> dict[str, str]:
    home = os.path.join(root, "home")
    os.makedirs(home)
    env = dict(os.environ)
    env.pop("LEVELER_CONFIG_DIR", None)
    env.update(
        {
            "TERM": "xterm-256color",
            "LEVELER_HOME": home,
            "COLORTERM": "truecolor",
            "LEVELER_LOG": "warn",
        }
    )
    cfg = os.path.join(home, "config.toml")
    # No model is called by this acceptance. The provider exists so boot has a
    # configured default model; its port is deliberately unreachable.
    with open(cfg, "w") as fh:
        fh.write(
            'default_model = "mock-a"\nlang = "zh"\n\n'
            '[providers.mock]\nprotocol = "openai_chat"\n'
            'base_url = "http://127.0.0.1:1"\napi_key = "test-only"\n\n'
            '[models."mock-a"]\nprovider = "mock"\nmodel_id = "model-a"\n'
            "context_window = 200000\nmax_output_tokens = 8192\n"
        )
    return env


def cli(env: dict[str, str], args: list[str]) -> dict | list:
    out = subprocess.run(
        [BIN, *args], env=env, check=True, capture_output=True, text=True
    ).stdout
    return json.loads(out)


def type_text(pty: Pty, text: str):
    for chunk in (text[i : i + 24] for i in range(0, len(text), 24)):
        pty.write(chunk)
        time.sleep(0.06)


def submit(pty: Pty):
    pty.key("\r")
    time.sleep(0.15)


def wait(pty: Pty, needle: str, timeout: float = 30.0) -> bool:
    return pty.wait_for(needle, timeout)


def wait_any(pty: Pty, needles: list[str], timeout: float = 30.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        text = pty.screen.text()
        if any(needle in text for needle in needles):
            return True
        if not pty.alive():
            return False
        time.sleep(0.05)
    return False


def main() -> int:
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % BIN)
        return 2
    report = Report("schedule-tui")
    # The daemon socket lives under LEVELER_HOME/run/sockets; a short root
    # keeps the Unix socket path below SUN_LEN on macOS.
    root = tempfile.mkdtemp(prefix="lv-tui-", dir="/tmp")
    repo = os.path.join(root, "repo")
    os.makedirs(os.path.join(repo, ".leveler"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "seed"],
        cwd=repo,
        check=False,
    )
    env = env_for(root)
    seeded = cli(
        env,
        [
            "schedule",
            "add",
            "--repo",
            repo,
            "--name",
            "TUI seeded shell",
            "--every",
            "3600",
            "--command",
            "printf seeded > seeded.txt",
            "--json",
        ],
    )
    pty = Pty(
        [BIN, "--repo", repo, "tui", "--auto-approve", "--permission", "full-access"],
        env,
        120,
        44,
    )
    try:
        report.check("boot", "the real TUI paints a frame", pty.wait_for("CodeLeveler", 60))
        pty.write("/schedules")
        submit(pty)
        report.check("page", "/schedules opens the scheduled-task page", wait(pty, "定时任务"))
        report.check("page", "the CLI-seeded definition is projected", wait(pty, "TUI seeded shell"))
        report.check("wakeup", "OS wakeup is honestly off by default", wait(pty, "后台唤醒未启用"))
        report.dump("list", pty)
        pty.key("\r")
        report.check("detail", "Enter opens the detail", wait(pty, "运行历史"))
        report.check("detail", "the grant is absent before an explicit grant", wait_any(pty, ["需要重新授权", "未授权无人值守"]))

        # `w` opens the OS-wakeup confirmation; cancel it so this run cannot
        # register a real system wakeup.
        pty.key("w")
        report.check("wakeup", "enabling OS wakeup asks first", wait(pty, "启用系统唤醒？"))
        pty.key("\x1b")
        report.check("wakeup", "Esc cancels without registering", wait(pty, "定时任务", 5))
        report.dump("wakeup-confirm-cancelled", pty)

        pty.key("g")
        report.check("grant", "grant asks for confirmation", wait(pty, "授权无人值守执行？"))
        pty.key("\x1b[B", "\r")
        report.check("grant", "grant is persisted and shown", wait(pty, "已授权无人值守 ·"))
        report.dump("granted", pty)
        granted = [d for d in cli(env, ["schedule", "list", "--repo", repo, "--json"]) if d["name"] == "TUI seeded shell"][0]
        report.check(
            "grant",
            "the runtime store sees the grant",
            granted.get("unattended_grant") is not None,
            json.dumps(granted),
        )

        pty.key("r")
        report.check("run", "run-now is acknowledged", wait(pty, "已请求立即运行"))
        done = wait(pty, "已完成", 60) and wait(pty, "退出码 0", 20)
        report.check("run", "the real shell action finishes with exit 0", done, pty.screen.text()[-800:])
        report.check(
            "run",
            "the real workspace file was written",
            os.path.exists(os.path.join(repo, "seeded.txt")),
        )
        report.dump("run-complete", pty)

        pty.key("p")
        report.check("pause", "pause is persisted", wait(pty, "已暂停 ·"))
        paused = [d for d in cli(env, ["schedule", "list", "--repo", repo, "--json"]) if d["name"] == "TUI seeded shell"][0]
        report.check("pause", "the runtime store sees enabled=false", paused.get("enabled") is False)
        pty.key("p")
        report.check("pause", "resume is persisted", wait(pty, "已恢复 ·"))

        # Create through the real form: Shell is the default action kind and
        # Daily the default trigger; the command and wall-clock rule are typed.
        # The resume acknowledgement is a transient notice: Esc first dismisses
        # it, and only the next Esc returns to the list. Retry until the list's
        # own footer proves the page state instead of assuming one Esc is
        # enough (otherwise the following 'e' in the typed text opens the
        # editor and the create scenario never starts).
        returned = False
        for _ in range(4):
            pty.key("\x1b")
            if wait(pty, "Enter 详情", 3):
                returned = True
                break
        report.check("list", "Esc returns to the list", returned)
        pty.key("n")
        report.check("create", "n opens the new-task form", wait(pty, "新建定时任务"))
        type_text(pty, "TUI created shell")
        submit(pty)
        type_text(pty, repo)
        submit(pty)  # ActionKind (Shell)
        submit(pty)  # -> ActionText
        type_text(pty, "printf created > created.txt")
        submit(pty)  # -> TriggerKind (Daily)
        submit(pty)  # -> TriggerText
        type_text(pty, "09:30 Asia/Shanghai")
        submit(pty)  # -> Mode
        submit(pty)  # -> Grant
        submit(pty)  # submit
        created = wait(pty, "已创建 ·", 30)
        report.check("create", "the form saves through the runtime", created, pty.screen.text()[-600:])
        report.check("create", "the created definition opens with next run", wait(pty, "TUI created shell"))
        report.dump("created", pty)
        listed = [d for d in cli(env, ["schedule", "list", "--repo", repo, "--json"]) if d["name"] == "TUI created shell"]
        report.check(
            "create",
            "the runtime stored the typed action and timezone",
            bool(listed)
            and listed[0]["action"] == {"kind": "shell", "command": "printf created > created.txt"}
            and listed[0]["trigger"].get("kind") == "daily"
            and listed[0]["trigger"].get("timezone") == "Asia/Shanghai",
            json.dumps(listed),
        )

        pty.key("e")
        report.check("edit", "e opens the editor with the current rule", wait(pty, "编辑定时任务"))
        type_text(pty, " v2")
        for _ in range(7):
            submit(pty)
        report.check("edit", "the edit saves through the runtime", wait(pty, "已保存 ·", 30))
        report.check("edit", "the detail shows the new name", wait(pty, "TUI created shell v2", 10))
        report.dump("edited", pty)

        pty.key("d")
        report.check("delete", "delete asks for confirmation", wait(pty, "删除定时任务？"))
        pty.key("\x1b[B", "\r")
        report.check("delete", "delete is persisted", wait(pty, "已删除 ·", 20))
        gone = [d for d in cli(env, ["schedule", "list", "--repo", repo, "--json"]) if d["name"].startswith("TUI created shell")]
        report.check("delete", "the runtime store no longer has the definition", not gone, json.dumps(gone))
        report.dump("deleted", pty)

        # Layout: the page must reflow on a real resize without losing state.
        pty.resize(90, 30)
        report.check("layout", "90x30 keeps the page and the definition", wait(pty, "TUI seeded shell", 10))
        pty.resize(60, 20)
        report.dump("layout-60x20", pty)
        # At 60 columns the name column ellipsizes ("TUI seeded s…"), which is
        # the intended adaptive layout; the row's rule proves the definition is
        # still projected.
        report.check("layout", "60x20 keeps the page and the definition row", wait(pty, "每隔 1 小时", 10))
        pty.resize(120, 44)
        report.check("layout", "restoring the size keeps the page", wait(pty, "定时任务", 10))

        # The seeded definition is still the runtime's, not a page-owned copy.
        survivor = [d for d in cli(env, ["schedule", "list", "--repo", repo, "--json"]) if d["name"] == "TUI seeded shell"]
        report.check("consistency", "the surviving definition is the seeded one", bool(survivor))
        report.check("consistency", "no definition is duplicated", len([d for d in cli(env, ["schedule", "list", "--repo", repo, "--json"])]) == 1)
    except Exception as exc:  # a crashed scenario is a FAIL, not a skip
        report.check("scenario", "completed", False, repr(exc))
        import traceback

        traceback.print_exc()
        report.dump("failure", pty)
    finally:
        pty.close()
    path = report.save()
    print("\nassertions: %d, failures: %d, blocked: %d" % (len(report.results), report.failures, report.blocked))
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
