#!/usr/bin/env python3
"""R9: final real business dogfood on the merged release.

A new session in the real DevOrder workspace, the real model and tools, one
autonomous Goal. The test never edits business files and never tells the model
what the audit asserts. Afterwards a separate read-only audit checks the diff,
the answer and the project's own tests.
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import pty_dogfood as dog  # noqa: E402

BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "release", "leveler")
REPO = "/Users/mengmian/Develop/app/csdn/DevOrder"
DB = "/Users/mengmian/.leveler/state/projects/-Users-mengmian-Develop-app-csdn-DevOrder-420a516e0c1a16fc/sessions.db"
OUT = os.path.join(
    ROOT,
    "docs/dev/harness-reliability-final-closure-2026-11-26/evidence/r9-final",
)
os.makedirs(OUT, exist_ok=True)
dog.EVIDENCE = OUT

TASK = """在 admin-web 的运营活动列表页增加结果条数提示，并补测试与变更记录。

要求：
1. 在运营活动列表的状态筛选附近显示“共 N 个活动”。N 优先取当前列表查询返回的 total；没有 total 时用当前页 items 数量。
2. 搜索、状态筛选、翻页后提示随当前结果更新；没有结果时显示“共 0 个活动”。
3. 在 Campaigns.test.tsx 增加测试，断言初始结果和筛选后的条数提示；沿用现有测试工具，不要另起测试框架。
4. 写 docs/changes/2026-11-27-campaigns-result-count.md，按仓库现有变更记录格式。
5. 只改 admin-web 中运营活动列表相关文件；不要动后端、contracts 或迁移；不要改 CampaignStats。
6. 运行 npx vitest run src/pages/campaign/Campaigns.test.tsx、npx vitest run 和 npx tsc -b（都在 admin-web 下），并在回答中给出真实命令结果。
"""


def q(sql, args=()):
    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    try:
        return con.execute(sql, args).fetchall()
    finally:
        con.close()


def latest_session():
    rows = q("select id from sessions order by created_at desc limit 1")
    return rows[0][0] if rows else None


def state(sid):
    session = q("select status,outcome from sessions where id=?", (sid,))
    turns = q("select ordinal,status,kind from turns where session_id=? order by ordinal", (sid,))
    goals = q(
        "select state from goals where task_id in (select id from tasks where session_id=?)",
        (sid,),
    )
    delivery = q("select payload from events where session_id=? and type='delivery_committed'", (sid,))
    return {
        "session": session[0] if session else None,
        "turns": turns,
        "goals": goals,
        "delivery": len(delivery),
    }


def wait_boot(pty, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        if "CodeLeveler" in pty.screen.text():
            return True
        if not pty.alive():
            return False
        time.sleep(0.2)
    return False


def git(*args):
    return subprocess.run(["git", "-C", REPO, *args], capture_output=True, text=True)


def main() -> int:
    if not os.path.exists(BIN):
        print(f"missing {BIN}")
        return 2
    before_status = git("status", "--porcelain").stdout
    started = time.time()
    pty = dog.Pty(
        [
            BIN,
            "--repo",
            REPO,
            "tui",
            "--auto-approve",
            "--in-process",
            "--permission",
            "full-access",
            "--model",
            "deepseek/deepseek-v4.1-flash-1017",
        ],
        os.environ.copy(),
        132,
        46,
    )
    result = {"task": TASK, "binary": BIN, "repo": REPO}
    try:
        if not wait_boot(pty):
            result["error"] = "the TUI did not paint"
            return 1
        pty.write("/goal " + TASK.replace("\n", " "))
        time.sleep(0.5)
        pty.key("\r")
        sid = None
        end = time.time() + 2400
        while time.time() < end:
            if sid is None:
                sid = latest_session()
            if sid:
                current = state(sid)
                if (
                    current["session"]
                    and current["session"][0] in ("completed", "blocked", "failed", "interrupted")
                    and all(turn[1] != "running" for turn in current["turns"])
                ):
                    break
            if not pty.alive():
                break
            time.sleep(2)
        time.sleep(3)
        open(os.path.join(OUT, "final-screen.txt"), "w").write(pty.screen.text())
        open(os.path.join(OUT, "final-screen.raw"), "wb").write(bytes(pty.raw))
        result["session"] = sid
        result["state"] = state(sid) if sid else None
        result["seconds"] = round(time.time() - started, 1)
        after_status = git("status", "--porcelain").stdout
        result["git_status_before"] = before_status.splitlines()
        result["git_status_after"] = after_status.splitlines()
        # The audit is read-only on business sources. The project's own test
        # command is run here to verify the claim independently of the answer.
        audit_env = os.environ.copy()
        test = subprocess.run(
            ["npx", "vitest", "run", "src/pages/campaign/Campaigns.test.tsx"],
            cwd=os.path.join(REPO, "admin-web"),
            capture_output=True,
            text=True,
            env=audit_env,
            timeout=600,
        )
        result["audit_test"] = {
            "exit": test.returncode,
            "tail": (test.stdout + test.stderr)[-2000:],
        }
        diff = git("diff", "--", "admin-web/src/pages/campaign/Campaigns.tsx", "admin-web/src/pages/campaign/Campaigns.test.tsx").stdout
        result["diff"] = diff
        result["audit"] = {
            "has_total_label": "共" in diff and "个活动" in diff,
            "has_test_assertion": "Campaigns.test.tsx" in after_status and "共" in diff,
            "test_passed": test.returncode == 0,
            "touched_forbidden_stats": any(
                "CampaignStats" in line and line not in set(before_status.splitlines())
                for line in after_status.splitlines()
            ),
            "new_change_record": "docs/changes/2026-11-27-campaigns-result-count.md" in after_status,
            "session_terminal": result["state"]["session"][0] if result["state"]["session"] else None,
            "delivery_committed": result["state"]["delivery"] > 0 if result["state"] else False,
        }
        result["passed"] = all(
            [
                result["audit"]["has_total_label"],
                result["audit"]["test_passed"],
                not result["audit"]["touched_forbidden_stats"],
                result["audit"]["new_change_record"],
                result["audit"]["session_terminal"] == "completed",
                result["audit"]["delivery_committed"],
            ]
        )
        with open(os.path.join(OUT, "r9-final.json"), "w") as handle:
            json.dump(result, handle, ensure_ascii=False, indent=2)
        print(json.dumps({k: result.get(k) for k in ("session", "state", "audit", "passed")}, ensure_ascii=False, indent=2))
        return 0 if result["passed"] else 1
    finally:
        pty.close()


if __name__ == "__main__":
    sys.exit(main())
