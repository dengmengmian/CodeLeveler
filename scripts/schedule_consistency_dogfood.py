#!/usr/bin/env python3
"""CLI ↔ TUI consistency for scheduled automation (Phase 5, DF-56).

One isolated runtime home is shared by the real CLI and a real TUI session.
Every mutation is made on one surface and read back on the other: a CLI create
must appear in the page, a TUI grant must be visible to `schedule show`, a CLI
edit must change the page, a CLI revoke must make the page ask for a new grant,
a CLI pause must read as paused, a TUI resume must be visible to the CLI, and a
CLI rename must not duplicate or reset anything. The Desktop direction is
covered by `apps/leveler-desktop/scripts/schedules.mjs`, which drives the real
Electron app against the same store.

No model is called. The provider entry only satisfies boot; its port is
deliberately unreachable.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Pty, Report  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
EVIDENCE = os.path.join(ROOT, "target", "schedule-consistency-dogfood")
NAME = "DF-56 consistency"
RENAMED = "DF-56 renamed"


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
    with open(os.path.join(home, "config.toml"), "w") as fh:
        fh.write(
            'default_model = "mock-a"\nlang = "zh"\n\n'
            '[providers.mock]\nprotocol = "openai_chat"\n'
            'base_url = "http://127.0.0.1:1"\napi_key = "test-only"\n\n'
            '[models."mock-a"]\nprovider = "mock"\nmodel_id = "model-a"\n'
            "context_window = 200000\nmax_output_tokens = 8192\n"
        )
    return env


def cli_json(env: dict[str, str], args: list[str]):
    result = subprocess.run([BIN, *args], env=env, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            "`%s` failed with %d: %s"
            % (" ".join(args), result.returncode, result.stderr[-800:])
        )
    return json.loads(result.stdout)


def definitions(env: dict[str, str], repo: str) -> list[dict]:
    return cli_json(env, ["schedule", "list", "--repo", repo, "--json"])


def show(env: dict[str, str], repo: str, id: str) -> dict:
    return cli_json(env, ["schedule", "show", id, "--repo", repo, "--json"])


def wait_text(pty: Pty, text: str, timeout: float = 30.0) -> bool:
    return pty.wait_for(text, timeout)


def enter_page(pty: Pty):
    pty.write("/schedules")
    pty.key("\r")


def flat(pty: Pty) -> str:
    return "".join(pty.screen.text().split())


def wait_flat(pty: Pty, texts: list[str], timeout: float = 30.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        screen = flat(pty)
        if any(text in screen for text in texts):
            return True
        if not pty.alive():
            return False
        time.sleep(0.05)
    return False


def to_chat(pty: Pty):
    """Escape out of a detail, a form or a confirmation back to chat.

    The chat composer is the only surface whose footer names the local
    commands, so that marker is the test for "the next keystroke is text".
    """
    for _ in range(4):
        if "命令·" in flat(pty):
            return True
        pty.key("\x1b")
        time.sleep(0.3)
    return "命令·" in flat(pty)


def open_list(pty: Pty):
    assert to_chat(pty), "could not return to chat before opening the page"
    enter_page(pty)
    assert wait_text(pty, "定时任务", 20), "the schedules page did not open"


def open_detail(pty: Pty):
    open_list(pty)
    pty.key("\r")
    assert wait_text(pty, "运行历史", 20), "the schedule detail did not open"


def main() -> int:
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % BIN)
        return 2
    report = Report("schedule-consistency")
    root = tempfile.mkdtemp(prefix="lv-cons-", dir="/tmp")
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
    pty = Pty([BIN, "--repo", repo, "tui", "--auto-approve"], env, 120, 44)
    try:
        report.check("boot", "the real TUI paints a frame", pty.wait_for("CodeLeveler", 60))

        # ── CLI create → TUI ───────────────────────────────────────────────
        created = cli_json(
            env,
            [
                "schedule",
                "add",
                "--repo",
                repo,
                "--name",
                NAME,
                "--daily",
                "09:30",
                "--timezone",
                "Asia/Shanghai",
                "--command",
                "printf one",
                "--json",
            ],
        )
        report.check("cli→tui", "the CLI created one definition", bool(created.get("id")), json.dumps(created))
        id = created["id"]
        open_list(pty)
        report.check("cli→tui", "the CLI-created definition is on the page", wait_text(pty, NAME))
        report.check("cli→tui", "the page shows the CLI rule", wait_flat(pty, ["每天09:30(Asia/Shanghai)"]))
        pty.key("\r")
        report.check("cli→tui", "the detail opens", wait_text(pty, "运行历史"))
        report.check(
            "cli→tui",
            "the detail shows the CLI command",
            wait_flat(pty, ["printfone"]),
            pty.screen.text()[-800:],
        )
        report.check(
            "cli→tui",
            "a create did not invent an unattended grant",
            wait_flat(pty, ["未授权无人值守", "需要重新授权"]),
        )

        # ── CLI update (no model) → TUI ────────────────────────────────────
        updated = cli_json(
            env,
            [
                "schedule",
                "update",
                id,
                "--repo",
                repo,
                "--command",
                "printf two",
                "--daily",
                "10:45",
                "--timezone",
                "Asia/Shanghai",
                "--expected-revision",
                created["revision"],
                "--json",
            ],
        )
        report.check(
            "cli→tui",
            "the CLI update keeps the same definition identity",
            updated.get("id") == id and updated.get("revision") != created.get("revision"),
            json.dumps(updated),
        )
        open_list(pty)
        report.check("cli→tui", "the page shows the edited rule", wait_flat(pty, ["每天10:45(Asia/Shanghai)"]))
        pty.key("\r")
        report.check("cli→tui", "the detail shows the edited action", wait_flat(pty, ["printftwo"]))

        # ── TUI grant → CLI ────────────────────────────────────────────────
        open_detail(pty)
        pty.key("g")
        report.check("tui→cli", "the TUI grant asks for confirmation", wait_text(pty, "授权无人值守执行？"))
        pty.key("\x1b[B", "\r")
        report.check("tui→cli", "the TUI applies the grant", wait_text(pty, "已授权无人值守 ·"))
        granted = show(env, repo, id)
        report.check(
            "tui→cli",
            "the CLI reads the TUI grant from the same store",
            isinstance(granted.get("unattended_grant"), dict)
            and granted["unattended_grant"].get("definition_id") == id,
            json.dumps(granted, ensure_ascii=False),
        )

        # ── CLI revoke → TUI ───────────────────────────────────────────────
        subprocess.run(
            [BIN, "schedule", "revoke-grant", id, "--repo", repo],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        report.check(
            "cli→tui",
            "the CLI revoke removes the grant from the store",
            show(env, repo, id).get("unattended_grant") is None,
        )
        open_detail(pty)
        report.check(
            "cli→tui",
            "the page asks for a new grant after the CLI revoke",
            wait_flat(pty, ["未授权无人值守", "需要重新授权"]),
            pty.screen.text()[-800:],
        )

        # ── CLI pause → TUI, TUI resume → CLI ──────────────────────────────
        subprocess.run(
            [BIN, "schedule", "pause", id, "--repo", repo],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        open_list(pty)
        report.check(
            "cli→tui",
            "the page shows the CLI pause",
            wait_flat(pty, ["已暂停"]),
            pty.screen.text()[-800:],
        )
        pty.key("\r")
        wait_text(pty, "运行历史", 20)
        pty.key("p")
        report.check("tui→cli", "the TUI resume is applied", wait_text(pty, "已恢复 ·"))
        report.check(
            "tui→cli",
            "the CLI reads the TUI resume",
            show(env, repo, id).get("enabled") is True,
        )

        # ── CLI rename → TUI, without resetting the declaration ───────────
        renamed = cli_json(
            env,
            ["schedule", "update", id, "--repo", repo, "--name", RENAMED, "--json"],
        )
        report.check(
            "cli→tui",
            "a name-only update keeps the action and rule",
            renamed.get("action") == {"kind": "shell", "command": "printf two"}
            and renamed.get("trigger", {}).get("time") == "10:45",
            json.dumps(renamed),
        )
        open_list(pty)
        report.check("cli→tui", "the page shows the CLI rename", wait_text(pty, RENAMED))
        report.check("consistency", "there is still exactly one definition", len(definitions(env, repo)) == 1)
        report.dump("final-page", pty)
    except Exception as exc:  # a crashed scenario is a FAIL, not a skip
        report.check("scenario", "completed", False, repr(exc))
        import traceback

        traceback.print_exc()
        report.dump("failure", pty)
    finally:
        pty.close()
        import shutil

        shutil.rmtree(root, ignore_errors=True)
    path = report.save()
    print("\nassertions: %d, failures: %d, blocked: %d" % (len(report.results), report.failures, report.blocked))
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
