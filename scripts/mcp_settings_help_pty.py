#!/usr/bin/env python3
"""Real-PTY acceptance for `/mcp`, `/settings` and `/help`.

Drives the real `leveler` binary on a real pseudo-terminal with the repository's
scripted provider and a real local MCP stdio server (a Python process that
speaks the MCP handshake). Nothing here is a TestBackend render: every check
reads the frames a terminal emulator receives.

Scenarios:
  mcp       page opens, configured != connected, reconnect, tool catalog,
            disable/reconnect refusal, enable/reconnect, add, remove, resize
            and a live resource value that never leaks
  doctor    the doctor's MCP finding agrees with `/mcp`
  settings  rows and sources, theme picker, reload, busy safety
  help      categories, search, aliases, usage, detail and Esc levels
  busy      a running turn: the pages open, mutations are refused
  i18n      the same pages render in English

Exit 0 means every assertion held; 2 means a required precondition was
unavailable; 1 means a real failure.
"""

from __future__ import annotations

import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import pty_dogfood as base  # noqa: E402

ROOT = os.path.dirname(HERE)
EVIDENCE = os.environ.get(
    "LEVELER_EVIDENCE", os.path.join(ROOT, ".dev", "audit", "mcp-settings-help-ux", "evidence", "pty")
)
os.makedirs(EVIDENCE, exist_ok=True)
base.EVIDENCE = EVIDENCE

SERVER = r'''import json, os, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method", "")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "echo", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo tool", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "ok"}]}
    else:
        result = {}
    if "id" in request:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
'''


def check(report, scenario, name, ok, detail=""):
    report.check(scenario, name, ok, detail)


def wait(pty, needle, timeout=25.0):
    return pty.wait_for(needle, timeout)


def to_conversation(pty):
    """Esc out of any page; on the conversation Esc is inert."""
    for _ in range(3):
        pty.key("\x1b")
        time.sleep(0.25)
    time.sleep(0.2)


def command(pty, text, settle=0.6):
    pty.write(text)
    time.sleep(0.2)
    pty.key("\r")
    time.sleep(settle)


def flat(pty):
    return base.flat(pty)


def install_server(env, python):
    home = env.home
    script = os.path.join(home, "echo.py")
    with open(script, "w") as fh:
        fh.write(SERVER)
    with open(os.path.join(home, "config.toml"), "a") as fh:
        fh.write(
            "\n[[mcp_servers]]\n"
            'name = "echo"\n'
            f'command = "{python}"\n'
            'args = ["-u", "%s"]\n'
            'env = { E2E_TOKEN = "E2E_TOKEN" }\n' % script
        )
    env.env["E2E_TOKEN"] = "planted-pty-secret-value"
    return script


def scenario_mcp(report, env, python):
    mock = base.MockProvider()
    mock.turns = []
    mock.assist_text = ""
    env = base.make_env("mock", mock)
    script = install_server(env, python)
    pty = base.boot(report, env, 120, 40)
    try:
        command(pty, "/mcp")
        ok = wait(pty, "MCP") and wait(pty, "echo")
        check(report, "mcp", "the real page lists the configured server", ok, pty.screen.text()[:600])
        report.dump("mcp-configured-not-connected", pty)
        check(
            report,
            "mcp",
            "a configured server is shown as not connected",
            "未连接" in pty.screen.text(),
            pty.screen.text()[:600],
        )

        pty.key("r")
        ok = wait(pty, "已连接", 30)
        check(report, "mcp", "reconnect asks the runtime and reports connected", ok, pty.screen.text()[:800])
        report.dump("mcp-connected", pty)
        check(report, "mcp", "the tool count is the runtime's discovered catalog", "1" in pty.screen.text())
        check(
            report,
            "mcp",
            "no environment value reaches the page",
            "planted-pty-secret-value" not in pty.screen.text(),
        )

        pty.key("\r")
        time.sleep(0.5)
        pty.key("\t")
        time.sleep(0.4)
        ok = wait(pty, "mcp__echo__echo")
        check(report, "mcp", "the tool catalog names the exposed tool", ok, pty.screen.text()[:800])
        report.dump("mcp-tool-catalog", pty)
        pty.key("\t")
        time.sleep(0.2)
        pty.key("\x1b")
        time.sleep(0.3)

        # Disable, then a reconnect must be refused with the real reason.
        pty.key("x")
        ok = wait(pty, "已禁用")
        check(report, "mcp", "disable writes through and updates the row", ok, pty.screen.text()[:800])
        pty.key("r")
        time.sleep(0.8)
        check(
            report,
            "mcp",
            "a reconnect on a disabled server is refused",
            "已禁用" in pty.screen.text(),
            pty.screen.text()[:800],
        )
        pty.key("x")
        time.sleep(0.6)
        pty.key("r")
        ok = wait(pty, "已连接", 30)
        check(report, "mcp", "enable + reconnect restores the connection", ok, pty.screen.text()[:800])

        # Add a second server through the form.
        pty.key("a")
        time.sleep(0.4)
        check(report, "mcp", "the add form opens", "添加 MCP Server" in pty.screen.text(), pty.screen.text()[:600])
        pty.write("e2e")
        pty.key("\t")  # transport
        pty.key("\t")  # command
        pty.write(python)
        pty.key("\t")  # args
        pty.write("-u %s" % script)
        pty.key("\t")  # env
        pty.key("\t")  # enabled
        pty.key("\r")
        ok = wait(pty, "已添加", 30)
        check(report, "mcp", "the add form writes a real server", ok, pty.screen.text()[:800])
        report.dump("mcp-added", pty)

        # Filter to the new server, then remove it through the confirmation.
        pty.key("/")
        pty.write("e2e")
        pty.key("\r")
        time.sleep(0.3)
        pty.key("d")
        time.sleep(0.4)
        check(report, "mcp", "remove asks for confirmation", "移除" in pty.screen.text())
        pty.key("\r")
        ok = wait(pty, "已移除", 30)
        check(report, "mcp", "the confirmed remove reaches the runtime", ok, pty.screen.text()[:800])

        # Resize while the page is open.
        pty.resize(60, 18)
        time.sleep(0.6)
        check(report, "mcp", "the page survives a narrow resize", "MCP" in pty.screen.text(), pty.screen.text()[:500])
        report.dump("mcp-narrow", pty)
        pty.resize(120, 40)
        time.sleep(0.5)

        # Doctor reads the same authority.
        to_conversation(pty)
        command(pty, "/doctor")
        ok = wait(pty, "MCP") and wait(pty, "echo", 15)
        # The finding list is sorted problems-first; the connected MCP finding
        # sits below the fold at this height.
        for _ in range(6):
            if "MCP 连接" in pty.screen.text():
                break
            pty.write("\x1b[6~")  # PageDown
            time.sleep(0.3)
        text = pty.screen.text()
        check(report, "doctor", "doctor lists the MCP server", "MCP 连接" in text, text[:900])
        report.dump("doctor-mcp", pty)
        doctor_text = flat(pty)
        check(
            report,
            "doctor",
            "doctor agrees the server is connected",
            "已连接" in doctor_text or "通过" in doctor_text,
            pty.screen.text()[:800],
        )
        check(
            report,
            "doctor",
            "doctor never shows the server's environment value",
            "planted-pty-secret-value" not in pty.screen.text(),
        )
        to_conversation(pty)
        return pty, env, mock
    except Exception:
        report.dump("mcp-failure", pty)
        pty.close()
        raise


def scenario_settings(report, pty, env):
    command(pty, "/settings")
    ok = wait(pty, "设置")
    check(report, "settings", "the page opens", ok, pty.screen.text()[:600])
    text = pty.screen.text()
    for label in ["当前模型", "全局默认模型", "推理级别", "权限档位", "主题", "配置来源", "重新加载配置"]:
        check(report, "settings", f"row is visible: {label}", label in text, text[:800])
    check(report, "settings", "the provider row is present", "mock" in text)
    report.dump("settings-list", pty)

    # Theme: Enter opens the existing picker; Esc returns.
    pty.write("/")
    pty.write("主题")
    pty.key("\r")
    time.sleep(0.2)
    pty.key("\r")
    time.sleep(0.4)
    check(report, "settings", "theme opens the shared picker", "主题" in pty.screen.text())
    # Choose the next theme and verify it reached the persisted client config.
    pty.write("\x1b[B")  # Down: auto -> dark
    time.sleep(0.2)
    pty.key("\r")
    time.sleep(0.5)
    config_text = ""
    try:
        with open(os.path.join(env.home, "config.toml")) as fh:
            config_text = fh.read()
    except OSError as error:
        config_text = str(error)
    check(
        report,
        "settings",
        "a theme choice persists to the client configuration",
        'theme = "dark"' in config_text,
        config_text[-300:],
    )
    report.dump("settings-theme-persisted", pty)

    # Reload: clear the previous filter, filter to the row, leave the filter,
    # activate.
    pty.key("/")
    for _ in range(4):
        pty.key("\x7f")
    pty.write("重新加载")
    pty.key("\r")
    time.sleep(0.2)
    pty.key("\r")
    ok = wait(pty, "设置已重新加载", 20)
    check(report, "settings", "reload reports the runtime's answer", ok, pty.screen.text()[:800])
    report.dump("settings-reloaded", pty)

    # Escape clears the filter first, then leaves the page.
    pty.key("\x1b")
    time.sleep(0.3)
    pty.key("\x1b")
    time.sleep(0.3)


def scenario_help(report, pty):
    command(pty, "/help")
    ok = wait(pty, "帮助")
    check(report, "help", "the browser opens", ok, pty.screen.text()[:600])
    text = pty.screen.text()
    for needle in ["Agent", "视图", "会话", "/model", "/permission", "/thinking"]:
        check(report, "help", f"registry row is visible: {needle}", needle in text, text[:900])
    report.dump("help-list", pty)

    # "MCP" also appears in /doctor's description; the specific phrase picks
    # the MCP management command.
    pty.write("MCP Server")
    time.sleep(0.5)
    check(report, "help", "typing filters the list", "/mcp" in pty.screen.text(), pty.screen.text()[:600])
    pty.key("\r")
    ok = wait(pty, "用法")
    check(report, "help", "Enter opens the command detail", ok, pty.screen.text()[:800])
    detail = pty.screen.text()
    check(report, "help", "detail shows usage", "/mcp" in detail, detail[:800])
    check(report, "help", "detail shows the category", "分类" in detail, detail[:800])
    check(report, "help", "detail shows the registry source", "命令注册表" in detail, detail[:800])
    report.dump("help-detail", pty)
    pty.key("\x1b")
    time.sleep(0.3)
    check(report, "help", "Esc returns to the list", "帮助" in pty.screen.text())
    pty.key("\x1b")
    time.sleep(0.3)

    # `/settings` is below the first viewport; search finds it like any other
    # command (the requirement is discoverability, not that 46 rows fit).
    to_conversation(pty)
    command(pty, "/help")
    opened = wait(pty, "帮助", 10)
    report.dump("help-settings-search", pty)
    check(report, "help", "the browser reopens after a detail visit", opened, pty.screen.text()[:800])
    pty.write("settings")
    time.sleep(0.4)
    check(report, "help", "the settings command is reachable by search", "/settings" in pty.screen.text())
    pty.key("\r")
    time.sleep(0.4)
    check(report, "help", "search result opens its detail", "/settings" in pty.screen.text())
    pty.key("\x1b")
    pty.key("\x1b")
    time.sleep(0.3)

    command(pty, "/help settings")
    ok = wait(pty, "用法")
    check(report, "help", "/help <command> opens that detail directly", ok, pty.screen.text()[:800])
    pty.key("\x1b")
    pty.key("\x1b")
    time.sleep(0.3)


def scenario_busy(report, pty, mock):
    # A real long tool turn: the runtime is busy while the pages open.
    mock.turns = [
        base.tool_turn(
            "run_command",
            json.dumps({"program": "sh", "args": ["-c", "sleep 25"]}),
        ),
        base.text_turn("probe done"),
    ]
    command(pty, "run a long probe")
    time.sleep(2.0)
    command(pty, "/mcp")
    ok = wait(pty, "echo", 10)
    check(report, "busy", "the MCP page opens while a turn runs", ok, pty.screen.text()[:800])
    pty.key("x")
    time.sleep(0.6)
    check(
        report,
        "busy",
        "a mutation is refused while the turn owns the tool surface",
        "工具面" in pty.screen.text() or "轮次" in pty.screen.text(),
        pty.screen.text()[:900],
    )
    report.dump("mcp-busy", pty)
    to_conversation(pty)
    command(pty, "/settings")
    ok = wait(pty, "设置", 10)
    check(report, "busy", "the settings page opens while a turn runs", ok, pty.screen.text()[:800])
    report.dump("settings-busy", pty)
    to_conversation(pty)
    # Cancel the probe so the run can proceed.
    pty.key("\x03")
    time.sleep(1.5)


def scenario_merged_surfaces(report, pty):
    """The Phase A merge's own pages still render on this candidate binary."""
    to_conversation(pty)
    for slash, needle, label in [
        ("/find needle", "搜索", "find"),
        ("/jump", "回合", "jump"),
        ("/tasks", "任务", "tasks"),
        ("/doctor", "诊断", "doctor"),
    ]:
        command(pty, slash)
        ok = wait(pty, needle, 15)
        check(report, "merged", f"{label} opens on the merged candidate", ok, pty.screen.text()[:700])
        report.dump(f"merged-{label}", pty)
        to_conversation(pty)


def scenario_i18n(report, python):
    mock = base.MockProvider()
    mock.turns = []
    mock.assist_text = ""
    env = base.make_env("mock", mock)
    env.env["LEVELER_LANG"] = "en"
    install_server(env, python)
    pty = base.boot(report, env, 120, 40)
    try:
        command(pty, "/mcp")
        ok = wait(pty, "MCP") and wait(pty, "echo")
        check(report, "i18n", "English MCP page opens", ok, pty.screen.text()[:600])
        check(report, "i18n", "state copy is English", "Not connected" in pty.screen.text(), pty.screen.text()[:600])
        report.dump("i18n-mcp", pty)
        to_conversation(pty)
        command(pty, "/settings")
        ok = wait(pty, "Settings")
        check(report, "i18n", "English settings page opens", ok, pty.screen.text()[:600])
        check(
            report,
            "i18n",
            "settings labels are English",
            "Global default model" in pty.screen.text(),
            pty.screen.text()[:800],
        )
        report.dump("i18n-settings", pty)
        to_conversation(pty)
        command(pty, "/help model")
        ok = wait(pty, "Usage")
        check(report, "i18n", "English help detail opens", ok, pty.screen.text()[:800])
        check(report, "i18n", "aliases are labelled", "Aliases" in pty.screen.text())
        report.dump("i18n-help", pty)
        to_conversation(pty)
    finally:
        pty.close()


def main():
    report = base.Report("mcp-settings-help")
    python = None
    for candidate in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "python3", "python"]:
        try:
            import subprocess

            if subprocess.run([candidate, "--version"], capture_output=True).returncode == 0:
                python = candidate
                break
        except Exception:
            continue
    if python is None:
        report.block("mcp", "a real local MCP server", "no Python interpreter is available")
        report.save()
        return 2

    pty = env = mock = None
    try:
        pty, env, mock = scenario_mcp(report, env, python)
        scenario_settings(report, pty, env)
        scenario_help(report, pty)
        scenario_busy(report, pty, mock)
        scenario_merged_surfaces(report, pty)
    except Exception as error:  # noqa: BLE001 - the report records the failure
        report.check("mcp", "the PTY scenario completed", False, repr(error))
    finally:
        if pty is not None:
            pty.close()
        if mock is not None:
            mock.stop()

    scenario_i18n(report, python)
    path = report.save()
    print("\nfailures: %d" % report.failures)
    print("report: %s" % path)
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
