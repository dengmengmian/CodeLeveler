#!/usr/bin/env python3
"""Real-model natural-language acceptance for scheduled automation (DF-51/DF-52).

The model is the machine's configured provider (the real `~/.leveler/config.toml`
is copied into an isolated HOME, including its API key); the TUI, runtime,
tool registry, schedule store and shell execution are all real. The two turns
below are ordinary chat messages: nothing in this script parses the wording or
builds a schedule request, so a pass proves the model reached the same
management API every other client uses.

DF-51 creates a definition from a wall-clock request and checks the stored
rule, the absent unattended grant, the absent OS registration and the model's
own summary. DF-52 changes it through a second natural-language turn after a
grant was issued in the TUI: the definition must stay single, the trigger must
change, and the old grant must be gone so the surface asks for re-authorization.
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Pty, Report  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
EVIDENCE = os.path.join(ROOT, "target", "schedule-nl-dogfood")
NAME = "DF-51 自然语言创建"
UPDATED_NAME = NAME
REAUTH_QUESTION = "之前的无人值守授权是否仍然有效。"


def env_for(root: str) -> dict[str, str]:
    home = os.path.join(root, "home")
    os.makedirs(home)
    source = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(source):
        raise SystemExit("real provider config missing: %s" % source)
    with open(source) as fh:
        text = fh.read()
    # Deterministic Chinese surface; the provider block is untouched.
    if not re.search(r"^lang\s*=", text, re.M):
        text = 'lang = "zh"\n' + text
    with open(os.path.join(home, "config.toml"), "w") as fh:
        fh.write(text)
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
    return env


def cli_json(env: dict[str, str], args: list[str]) -> list | dict:
    result = subprocess.run(
        [BIN, *args], env=env, capture_output=True, text=True
    )
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


def wait_store(env: dict[str, str], repo: str, predicate, timeout: float):
    end = time.time() + timeout
    last: list[dict] = []
    while time.time() < end:
        try:
            last = definitions(env, repo)
        except subprocess.CalledProcessError:
            last = []
        if predicate(last):
            return last
        time.sleep(1.0)
    return last


def wait_reauth_claim(pty: Pty, after: str, timeout: float) -> str | None:
    """The model's own statement about the old grant, without negation traps.

    "不需要重新授权" contains "重新授权", so a plain substring check would read
    the opposite claim as a pass. Only text after the user's request is
    considered: the request itself ends with the question "是否仍然有效".
    """
    negatives = ("不需要重新授权", "无需重新授权", "不需要授权", "仍然有效", "继续有效", "仍然可用", "授权保持不变")
    positives = ("需要重新授权", "重新授权", "授权已失效", "授权失效", "需要新的授权", "已失效", "granted=false")
    end = time.time() + timeout
    while time.time() < end:
        text = flat(pty)
        index = text.rfind(after)
        tail = text[index + len(after) :] if index >= 0 else text
        if any(phrase in tail for phrase in negatives):
            return None
        for phrase in positives:
            if phrase in tail:
                return phrase
        if not pty.alive():
            return None
        time.sleep(0.1)
    return None


def settle_turn(pty: Pty):
    """Wait for the transcript's own turn-end receipt before typing a command.

    Slash commands are only admitted while the main task is idle; a model that
    is still streaming its summary would otherwise swallow the page key.
    """
    if not pty.wait_for("回答结束", 120):
        pty.wait_stable(2.0, 30)


def type_text(pty: Pty, text: str):
    for chunk in (text[i : i + 24] for i in range(0, len(text), 24)):
        pty.write(chunk)
        time.sleep(0.05)


def ask(pty: Pty, prompt: str):
    type_text(pty, prompt)
    time.sleep(0.2)
    pty.key("\r")


def main() -> int:
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % BIN)
        return 2
    report = Report("schedule-nl")
    root = tempfile.mkdtemp(prefix="lv-nl-", dir="/tmp")
    repo = os.path.realpath(os.path.join(root, "repo"))
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
    pty = Pty(
        [BIN, "--repo", repo, "tui", "--in-process", "--auto-approve", "--permission", "full-access"],
        env,
        130,
        46,
    )
    try:
        report.check("boot", "the real TUI paints a frame", pty.wait_for("CodeLeveler", 60))

        # ── DF-51: create from a wall-clock request ────────────────────────
        ask(
            pty,
            "请使用你的 schedule_manage 工具直接创建一个定时任务，需要时先用 "
            "schedule_inspect 读取，不要只给我步骤。名称：%s。规则：每天上午 9:30，"
            "IANA 时区 Asia/Shanghai。动作：在目录 %s 运行 shell 命令 "
            "`printf df51 > df51.txt`。只创建这一个任务；不要启用系统后台唤醒，"
            "也不要授予无人值守执行权限，现在不要运行。完成后请告诉我：名称、"
            "规则与下次运行时间、是否已启用系统后台唤醒、是否已授权无人值守。"
            % (NAME, repo),
        )
        seen_tool = wait_flat(pty, ["schedule_manage"], 180)
        report.check(
            "df51",
            "the model calls the real schedule_manage tool",
            seen_tool is not None,
            pty.screen.text()[-1200:],
        )
        created = wait_store(
            env,
            repo,
            lambda rows: any(row.get("name") == NAME for row in rows),
            180,
        )
        report.dump("df51-tool-turn", pty)
        mine = [row for row in created if row.get("name") == NAME]
        report.check("df51", "exactly one definition exists", len(mine) == 1, json.dumps(created, ensure_ascii=False))
        if mine:
            row = mine[0]
            report.check(
                "df51",
                "the stored action is the requested shell command in the requested workspace",
                row.get("action") == {"kind": "shell", "command": "printf df51 > df51.txt"}
                and row.get("workspace") == repo,
                json.dumps(row, ensure_ascii=False),
            )
            report.check(
                "df51",
                "the wall-clock request became a daily rule in the named zone",
                row.get("trigger") == {"kind": "daily", "time": "09:30", "timezone": "Asia/Shanghai"},
                json.dumps(row.get("trigger"), ensure_ascii=False),
            )
            report.check(
                "df51",
                "creation did not grant unattended execution",
                row.get("unattended_grant") is None,
                json.dumps(row, ensure_ascii=False),
            )
            report.check(
                "df51",
                "creation did not silently enable OS wakeup",
                row.get("enabled") is True,
            )
        # The runtime's own status is the authority for the OS registration.
        # Read it after the turn has produced its summary: the model is still
        # generating while the tool row alone is already on screen.
        wakeup_phrase = wait_flat(
            pty,
            ["后台唤醒未启用", "未启用系统后台唤醒", "没有启用系统后台唤醒", "不会自动启用"],
            180,
        )
        report.check(
            "df51",
            "the model states the OS-wakeup fact from the returned state",
            wakeup_phrase is not None,
            pty.screen.text()[-1600:],
        )
        grant_phrase = wait_flat(
            pty,
            ["未授权无人值守", "未授权", "需要授权", "没有授权", "没有授予", "未授予", "granted=false"],
            180,
        )
        report.check(
            "df51",
            "the model states that unattended execution is not authorized",
            grant_phrase is not None,
            pty.screen.text()[-1600:],
        )
        status = cli_json(env, ["schedule", "status", "--repo", repo, "--json"])
        report.check(
            "df51",
            "the OS wakeup is not registered",
            status.get("wakeup", {}).get("registered") is False,
            json.dumps(status, ensure_ascii=False),
        )

        # ── Grant in the TUI so DF-52 can observe invalidation ─────────────
        settle_turn(pty)
        pty.write("/schedules")
        pty.key("\r")
        report.check("grant", "the schedules page opens", pty.wait_for("定时任务", 20))
        report.check("grant", "the model-created definition is projected", pty.wait_for(NAME, 20))
        pty.key("\r")
        report.check("grant", "the detail opens", pty.wait_for("运行历史", 20))
        report.dump("grant-before", pty)
        pty.key("g")
        report.check("grant", "grant asks for confirmation", pty.wait_for("授权无人值守执行？", 20))
        pty.key("\x1b[B", "\r")
        report.check("grant", "grant is persisted", pty.wait_for("已授权无人值守 ·", 20))
        granted = wait_store(
            env,
            repo,
            lambda rows: any(row.get("name") == NAME and row.get("unattended_grant") for row in rows),
            20,
        )
        report.check(
            "grant",
            "the runtime store holds the grant",
            any(row.get("name") == NAME and row.get("unattended_grant") for row in granted),
            json.dumps(granted, ensure_ascii=False),
        )
        pty.key("\x1b")
        pty.wait_for("Enter 详情", 10)
        pty.key("\x1b")
        report.dump("df52-back-to-chat", pty)

        # ── DF-52: modify by natural language ──────────────────────────────
        ask(
            pty,
            "请继续使用 schedule_manage 工具修改「%s」：工作目录保持不变，但把 shell "
            "命令改成 `printf df52 > df52.txt`，并把运行时间改成每天上午 10:15，仍然"
            "是 Asia/Shanghai 时区。修改前先用 schedule_inspect show 读取最新 revision，"
            "再用它做 update，避免覆盖较新的修改。改完后告诉我：现在总共有几个同名任务、"
            "规则是什么、之前的无人值守授权是否仍然有效。" % NAME,
        )
        wait_flat(pty, ["schedule_manage"], 180)
        updated = wait_store(
            env,
            repo,
            lambda rows: any(
                row.get("name") == NAME
                and row.get("trigger", {}).get("time") == "10:15"
                and row.get("action", {}).get("command") == "printf df52 > df52.txt"
                for row in rows
            ),
            180,
        )
        report.dump("df52-updated", pty)
        mine = [row for row in updated if row.get("name") == NAME]
        report.check(
            "df52",
            "the update changed the existing definition instead of adding a duplicate",
            len(mine) == 1,
            json.dumps(updated, ensure_ascii=False),
        )
        if mine:
            row = mine[0]
            report.check(
                "df52",
                "the updated rule is daily 10:15 in the same zone",
                row.get("trigger") == {"kind": "daily", "time": "10:15", "timezone": "Asia/Shanghai"},
                json.dumps(row.get("trigger"), ensure_ascii=False),
            )
            report.check(
                "df52",
                "the new action is stored and the workspace survived the edit",
                row.get("action") == {"kind": "shell", "command": "printf df52 > df52.txt"}
                and row.get("workspace") == repo,
                json.dumps(row, ensure_ascii=False),
            )
            report.check(
                "df52",
                "the old unattended grant was invalidated",
                row.get("unattended_grant") is None,
                json.dumps(row, ensure_ascii=False),
            )
        reauth_phrase = wait_reauth_claim(pty, REAUTH_QUESTION, 120)
        report.check(
            "df52",
            "the model tells the user that re-authorization is required",
            reauth_phrase is not None,
            pty.screen.text()[-1800:],
        )

        # The TUI itself must say the grant is gone, not just the store.
        pty.write("/schedules")
        pty.key("\r")
        report.check("df52", "the schedules page reopens", pty.wait_for("定时任务", 20))
        report.check("df52", "the edited rule is visible", pty.wait_for("10:15", 20))
        pty.key("\r")
        report.check(
            "df52",
            "the detail asks for a new grant",
            pty.wait_for("需要重新授权", 20) or pty.wait_for("未授权无人值守", 5),
            pty.screen.text()[-800:],
        )
        report.dump("df52-needs-regrant", pty)

        all_rows = definitions(env, repo)
        report.check(
            "consistency",
            "the CLI, TUI and runtime agree on one definition",
            len(all_rows) == 1 and all_rows[0].get("name") == NAME,
            json.dumps(all_rows, ensure_ascii=False),
        )
        report.check(
            "consistency",
            "the OS wakeup stayed unregistered through both turns",
            cli_json(env, ["schedule", "status", "--repo", repo, "--json"])
            .get("wakeup", {})
            .get("registered")
            is False,
        )
    except Exception as exc:  # a crashed scenario is a FAIL, not a skip
        report.check("scenario", "completed", False, repr(exc))
        import traceback

        traceback.print_exc()
        report.dump("failure", pty)
    finally:
        pty.close()
        shutil.rmtree(root, ignore_errors=True)
    path = report.save()
    print("\nassertions: %d, failures: %d, blocked: %d" % (len(report.results), report.failures, report.blocked))
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
