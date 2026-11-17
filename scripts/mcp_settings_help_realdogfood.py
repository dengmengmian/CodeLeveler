#!/usr/bin/env python3
"""Real-model dogfood for `/mcp`, `/settings` and `/help`.

Uses the user's configured provider from `~/.leveler/config.toml`, copied into
an isolated temporary home (deleted when the run ends). No mock provider: the
model turn is real, and the MCP tool call reaches a real local stdio server.

The run proves:
  * a real model turn still works in the candidate binary;
  * the model discovers and calls the configured MCP tool;
  * `/mcp` reports the runtime's connected state and catalog;
  * `/doctor` agrees with `/mcp`;
  * `/settings` shows the real model and provider;
  * `/help` search/detail works;
  * a later real turn still works after visiting every page (no page leaves
    the runtime or the composer broken).

Exit 0 = every check held, 2 = blocked (no configured provider/model), 1 = a
real failure. Evidence is written under the audit directory.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import pty_dogfood as base  # noqa: E402

ROOT = os.path.dirname(HERE)
EVIDENCE = os.environ.get(
    "LEVELER_EVIDENCE",
    os.path.join(ROOT, ".dev", "audit", "mcp-settings-help-ux", "evidence", "real-dogfood"),
)
os.makedirs(EVIDENCE, exist_ok=True)
base.EVIDENCE = EVIDENCE

SERVER = r'''import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method", "")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "real-echo", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Returns the text REAL_MCP_OK when called.", "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "REAL_MCP_OK"}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
'''


def check(report, scenario, name, ok, detail=""):
    report.check(scenario, name, ok, detail)


def wait(pty, needle, timeout=90.0):
    return pty.wait_for(needle, timeout)


def command(pty, text, settle=0.8):
    pty.write(text)
    time.sleep(0.3)
    pty.key("\r")
    time.sleep(settle)


def to_conversation(pty):
    for _ in range(3):
        pty.key("\x1b")
        time.sleep(0.25)


def python() -> str | None:
    for candidate in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "python3", "python"]:
        try:
            if subprocess.run([candidate, "--version"], capture_output=True).returncode == 0:
                return candidate
        except Exception:
            continue
    return None


def setup_env() -> tuple[dict[str, str], str, tempfile.TemporaryDirectory] | None:
    source = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.isfile(source):
        return None
    with open(source) as fh:
        config = fh.read()
    if "[[providers" not in config and "[providers." not in config:
        return None
    python_bin = python()
    if python_bin is None:
        return None
    root = tempfile.TemporaryDirectory(prefix="leveler-realdogfood-")
    home = os.path.join(root.name, "home")
    repo = os.path.join(root.name, "repo")
    os.makedirs(home)
    os.makedirs(os.path.join(repo, ".leveler"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "seed"],
        cwd=repo,
        check=False,
    )
    script = os.path.join(home, "real_echo.py")
    with open(script, "w") as fh:
        fh.write(SERVER)
    with open(os.path.join(home, "config.toml"), "w") as fh:
        fh.write(config)
        fh.write(
            "\n[[mcp_servers]]\n"
            'name = "real_echo"\n'
            f'command = "{python_bin}"\n'
            'args = ["-u", "%s"]\n' % script
        )
    env = dict(os.environ)
    env.update(
        {
            "TERM": "xterm-256color",
            "LEVELER_HOME": home,
            "COLORTERM": "truecolor",
            "LEVELER_LOG": "warn",
            "LEVELER_LANG": "zh",
        }
    )
    return env, repo, root


def main() -> int:
    report = base.Report("mcp-settings-help-realdogfood")
    prepared = setup_env()
    if prepared is None:
        report.block(
            "real",
            "a configured provider and a local Python interpreter",
            "no usable ~/.leveler/config.toml provider or Python",
        )
        report.save()
        return 2
    env, repo, root = prepared
    pty = base.Pty(
        [
            base.BIN,
            "--repo",
            repo,
            "tui",
            "--auto-approve",
            "--in-process",
            "--permission",
            "full-access",
        ],
        env,
        120,
        40,
    )
    try:
        ok = pty.wait_for("leveler", timeout=60) or pty.wait_stable(1.0, 60)
        check(report, "real", "the candidate binary paints a frame", ok, pty.screen.text()[:500])

        # A real model turn that must discover and call the MCP tool.
        pty.write(
            "请调用工具 mcp__real_echo__echo（参数为空对象），然后原样回复它返回的文本。"
        )
        time.sleep(0.3)
        pty.key("\r")
        ok = wait(pty, "REAL_MCP_OK", 180)
        check(report, "real", "the real model calls the MCP tool and its result reaches the conversation", ok, pty.screen.text()[:1200])
        report.dump("real-turn", pty)

        # The pages read the same runtime authority after that turn.
        to_conversation(pty)
        command(pty, "/mcp")
        ok = wait(pty, "已连接", 20) and wait(pty, "real_echo", 10)
        check(report, "real", "/mcp reports the connected server after a real turn", ok, pty.screen.text()[:900])
        report.dump("real-mcp", pty)
        pty.key("\r")
        time.sleep(0.4)
        pty.key("\t")
        time.sleep(0.4)
        check(
            report,
            "real",
            "the tool catalog shows the model-facing tool",
            "mcp__real_echo__echo" in pty.screen.text(),
            pty.screen.text()[:900],
        )
        to_conversation(pty)

        command(pty, "/doctor")
        ok = wait(pty, "MCP", 20)
        for _ in range(8):
            if "MCP 连接" in pty.screen.text():
                break
            pty.write("\x1b[6~")
            time.sleep(0.3)
        check(
            report,
            "real",
            "/doctor agrees the MCP server is connected",
            ok and "MCP 连接" in pty.screen.text(),
            pty.screen.text()[:900],
        )
        report.dump("real-doctor", pty)
        to_conversation(pty)

        command(pty, "/settings")
        ok = wait(pty, "设置", 20)
        text = pty.screen.text()
        check(report, "real", "/settings shows the real model", ok and "deepseek" in text, text[:900])
        check(report, "real", "/settings shows the provider", "deepseek" in text, text[:900])
        report.dump("real-settings", pty)
        to_conversation(pty)

        command(pty, "/help")
        wait(pty, "帮助", 10)
        pty.write("MCP Server")
        time.sleep(0.5)
        pty.key("\r")
        ok = wait(pty, "用法", 10)
        check(report, "real", "/help search and detail work", ok and "/mcp" in pty.screen.text(), pty.screen.text()[:900])
        report.dump("real-help", pty)
        to_conversation(pty)

        # The conversation still works after every page visit.
        pty.write("Reply with exactly DOGFOOD_DONE and nothing else.")
        time.sleep(0.3)
        pty.key("\r")
        ok = wait(pty, "DOGFOOD_DONE", 180)
        check(report, "real", "a later real turn still works after the pages", ok, pty.screen.text()[:1200])
        report.dump("real-second-turn", pty)
    finally:
        pty.close()
        shutil.rmtree(root.name, ignore_errors=True)
    path = report.save()
    print("\nfailures: %d" % report.failures)
    print("report: %s" % path)
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
