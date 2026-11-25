#!/usr/bin/env python3
"""Real-PTY acceptance for Secret Broker 1.0.

Two scenarios are driven through the REAL `leveler` binary on a real
pseudo-terminal, with the real runtime, reducer, renderer, tool executor and
SQLite store. The provider is a local OpenAI-wire server: it reads the model
request it receives (exactly what a provider would see) and answers with the
`secret_apply` tool call that request's reference makes possible.

Scenario A — automatic detection:
  type a known-format key, confirm secure capture, the model asks the broker to
  write it, and the turn completes.

Scenario B — explicit secure paste:
  `/secret`, paste a value with no recognizable format, continue with the
  instruction, and the model writes THAT value.

Assertions:
  * the review/masked field never paints the value;
  * no provider request body contains the value, and at least one carries a
    `[secret:sec_…]` reference;
  * the authorized `.env` contains the real value;
  * no other file under LEVELER_HOME or the repo contains it.

Exit 0 means every check held. Evidence is written to
`target/secret-broker-pty/`.
"""

from __future__ import annotations

import json
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import pty_dogfood as base  # noqa: E402

CANARY = "sk-CANARY-SECRET-DO-NOT-EXPOSE-001"
UNKNOWN = "xY9pQ2vT7"
EVIDENCE = os.path.join(base.ROOT, "target", "secret-broker-pty")
os.makedirs(EVIDENCE, exist_ok=True)
base.EVIDENCE = EVIDENCE


def secret_in(body: str) -> str | None:
    match = re.search(r"\[secret:(sec_[0-9a-f]{32})\]", body)
    return match.group(1) if match else None


def last_role(body: str) -> str:
    """The last conversational role; the runtime appends a trailing system
    reminder, so the tail of `messages` is not the conversation's tail."""
    try:
        messages = json.loads(body).get("messages", [])
    except (ValueError, AttributeError):
        return ""
    for message in reversed(messages):
        role = message.get("role", "")
        if role != "system":
            return role
    return ""


def transform(body: str, frames: list[str]) -> list[str]:
    """Answer like a model that read the reference from its own prompt."""
    if last_role(body) == "tool":
        return base.text_turn("已完成配置。")
    reference = secret_in(body)
    if reference is None:
        return base.text_turn("消息中没有可用的凭据引用。")
    field = "OTHER_TOKEN" if "OTHER_TOKEN" in body else "DEEPSEEK_API_KEY"
    arguments = json.dumps(
        {
            "secret_ref": reference,
            "field": field,
            "target": ".env",
        }
    )
    return base.tool_turn("secret_apply", arguments)


def scan(root: str, allowed: set[str]) -> list[str]:
    hits = []
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            if name in allowed:
                continue
            path = os.path.join(dirpath, name)
            try:
                with open(path, "rb") as fh:
                    if CANARY.encode() in fh.read():
                        hits.append(path)
            except OSError:
                continue
    return hits


def main() -> int:
    mock = base.MockProvider()
    mock.turns = [base.text_turn("placeholder")]
    mock.transform = transform
    env = base.make_env("mock", mock)
    report = base.Report("secret-broker")
    pty = base.boot(report, env, cols=132, rows=46)
    try:
        # ── Scenario A: automatic detection ────────────────────────────────
        pty.write(f"帮我把 {CANARY} 配置到 .env")
        time.sleep(0.4)
        pty.key("\r")
        opened = pty.wait_for("敏感凭据", 15)
        frame = pty.screen.text()
        report.check(
            "detect",
            "the credential review opens on Enter",
            opened,
            frame[-600:],
        )
        report.check(
            "detect",
            "the review never paints the value",
            CANARY not in frame,
            "the value appeared on screen",
        )
        report.dump("A-review", pty)

        # Default row is the safe capture.
        pty.key("\r")
        finished = pty.wait_for("已完成配置", 60) or pty.wait_for("配置", 20)
        time.sleep(1.0)
        screen = pty.screen.text()
        report.check(
            "detect",
            "the turn completes through the broker tool",
            finished,
            screen[-800:],
        )
        report.check(
            "detect",
            "the transcript never paints the value",
            CANARY not in screen,
            "the value appeared in the transcript",
        )
        env_path = os.path.join(env.repo, ".env")
        written = ""
        if os.path.exists(env_path):
            written = open(env_path).read()
        report.check(
            "detect",
            "the authorized .env holds the real value",
            f"DEEPSEEK_API_KEY={CANARY}" in written,
            written,
        )
        report.dump("A-done", pty)

        # ── Scenario B: explicit secure paste, unknown format ─────────────
        pty.write("/secret")
        time.sleep(0.2)
        pty.key("\r")
        paste_open = pty.wait_for("粘贴内容不会回显", 10)
        report.check(
            "paste",
            "the explicit secure-paste field opens",
            paste_open,
            pty.screen.text()[-500:],
        )
        pty.write(UNKNOWN)
        time.sleep(0.3)
        masked = pty.screen.text()
        report.check(
            "paste",
            "the field paints a mask, not the value",
            UNKNOWN not in masked,
            masked[-600:],
        )
        pty.key("\r")
        captured = pty.wait_for("已安全接管", 15)
        report.check(
            "paste",
            "capture succeeds and returns a reference",
            captured and "[secret:sec_" in pty.screen.text(),
            pty.screen.text()[-600:],
        )
        pty.write(" 请把它配置为 OTHER_TOKEN")
        time.sleep(0.3)
        pty.key("\r")
        done_b = pty.wait_for("已完成配置", 60)
        time.sleep(1.0)
        written = open(env_path).read() if os.path.exists(env_path) else ""
        report.check(
            "paste",
            "the pasted value is written under the chosen field",
            done_b and f"OTHER_TOKEN={UNKNOWN}" in written,
            written,
        )
        report.dump("B-done", pty)

        # ── Leak boundaries ───────────────────────────────────────────────
        requests = "\n".join(mock.requests)
        report.check(
            "leak",
            "no provider request body contains either value",
            CANARY not in requests and UNKNOWN not in requests,
            "a request body contained a captured value",
        )
        report.check(
            "leak",
            "the model received the reference token",
            "[secret:sec_" in requests,
            "no reference reached the model",
        )
        home_hits = scan(env.home, set())
        report.check(
            "leak",
            "no file under LEVELER_HOME contains either value",
            not home_hits and CANARY not in _read_home(env.home) and UNKNOWN not in _read_home(env.home),
            ", ".join(home_hits),
        )
        repo_hits = scan(env.repo, {".env"})
        report.check(
            "leak",
            "no repo file other than .env contains either value",
            not repo_hits,
            ", ".join(repo_hits),
        )

        report.save()
        return 0 if report.failures == 0 else 1
    finally:
        pty.close()


def _read_home(home: str) -> str:
    chunks = []
    for dirpath, _dirs, files in os.walk(home):
        for name in files:
            path = os.path.join(dirpath, name)
            try:
                chunks.append(open(path, "rb").read().decode("utf-8", "ignore"))
            except OSError:
                continue
    return "\n".join(chunks)


if __name__ == "__main__":
    sys.exit(main())
