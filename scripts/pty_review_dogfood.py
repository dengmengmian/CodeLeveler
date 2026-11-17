#!/usr/bin/env python3
"""Real-model `/review` dogfood through the real TUI on a real PTY.

Runs the release binary against an isolated LEVELER_HOME (the user's provider
config is copied; the user's state is untouched). The prompt is typed into the
composer, the review card is allowed to settle, and its report page is opened
and paged. After the TUI exits, the durable review row is read from the
isolated database and saved with the screen evidence.

    LEVELER_BIN=target/release/leveler python3 scripts/pty_review_dogfood.py \
        --repo /path/to/fixture --prompt '/review @project ...' \
        --evidence /path/to/evidence [--seed-project-db PATH]

Exit 0 requires: card started, terminal status observed, report page opened,
and the reviewed worktree unchanged (git status before == after).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_dogfood as dog  # noqa: E402

ROOT = dog.ROOT
BIN = os.environ.get("LEVELER_BIN", os.path.join(ROOT, "target/release/leveler"))
SETTLED = ("查看报告", "审查未完成", "审查已取消", "审查失败")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True)
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument(
        "--seed-project-db",
        help="copy this sessions.db into the isolated home's project state",
    )
    parser.add_argument(
        "--watch-session",
        help="assert this session's turns/messages/events are unchanged across the review",
    )
    parser.add_argument("--cols", type=int, default=140)
    parser.add_argument("--rows", type=int, default=48)
    return parser.parse_args()


def git_status(repo: str) -> str:
    return subprocess.check_output(["git", "status", "--porcelain"], cwd=repo, text=True)


def isolated_home(repo: str, seed_project_db: str | None) -> str:
    home = tempfile.mkdtemp(prefix="leveler-review-dogfood-")
    os.makedirs(home, exist_ok=True)
    shutil.copyfile(
        os.path.expanduser("~/.leveler/config.toml"), os.path.join(home, "config.toml")
    )
    if seed_project_db:
        # The state directory slug is derived from the workspace path in the
        # same way the runtime derives it. Copy the whole projects directory
        # so the seeded DB lands at the path this repo will resolve.
        source = os.path.dirname(seed_project_db)
        # Keep the per-project slug directory: the runtime resolves the state
        # path from the workspace, so flattening the copy makes the seeded DB
        # invisible (and the review reports "session not found").
        target = os.path.join(home, "state", "projects", os.path.basename(source))
        shutil.copytree(source, target)
    return home


def run_env(home: str) -> dict[str, str]:
    env = dict(os.environ)
    env.update(
        {
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LEVELER_HOME": home,
            "LEVELER_LOG": "warn",
        }
    )
    return env


def wait_started(pty: dog.Pty, timeout: float) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        text = pty.screen.text()
        if "审查中" in text:
            return text
        if any(marker in text for marker in SETTLED):
            return text
        time.sleep(1.0)
    raise TimeoutError("the review card never appeared:\n" + pty.screen.text()[-4000:])


def wait_settled(pty: dog.Pty, timeout: float) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        text = pty.screen.text()
        if any(marker in text for marker in SETTLED):
            return text
        time.sleep(2.0)
    raise TimeoutError("the review never settled:\n" + pty.screen.text()[-6000:])


def session_snapshot(home: str, session_id: str) -> dict | None:
    for dirpath, _dirnames, filenames in os.walk(os.path.join(home, "state", "projects")):
        if "sessions.db" not in filenames:
            continue
        path = os.path.join(dirpath, "sessions.db")
        try:
            connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        except sqlite3.Error:
            continue
        try:
            exists = connection.execute(
                "SELECT COUNT(*) FROM sessions WHERE id = ?", (session_id,)
            ).fetchone()[0]
            if not exists:
                continue
            snapshot = {"db": path}
            for table in ("turns", "session_messages", "events", "model_requests"):
                snapshot[table] = connection.execute(
                    f"SELECT COUNT(*) FROM {table} WHERE session_id = ?", (session_id,)
                ).fetchone()[0]
            snapshot["last_event_sequence"] = connection.execute(
                "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE session_id = ?",
                (session_id,),
            ).fetchone()[0]
            return snapshot
        except sqlite3.Error:
            continue
        finally:
            connection.close()
    return None


def read_reviews(home: str) -> list[dict]:
    rows: list[dict] = []
    for dirpath, _dirnames, filenames in os.walk(os.path.join(home, "state", "projects")):
        if "sessions.db" not in filenames:
            continue
        path = os.path.join(dirpath, "sessions.db")
        try:
            connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        except sqlite3.Error:
            continue
        try:
            tables = {
                row[0]
                for row in connection.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'"
                )
            }
            if "reviews" not in tables:
                continue
            columns = [
                "id",
                "session_id",
                "focus",
                "scope_json",
                "model",
                "status",
                "findings_json",
                "report",
                "error",
                "stop_detail",
                "verdict_json",
                "usage_json",
                "object_json",
                "object_changed",
                "modified_source",
                "source_session_id",
                "source_turn_id",
                "created_at",
                "finished_at",
            ]
            for row in connection.execute(
                f"SELECT {', '.join(columns)} FROM reviews ORDER BY created_at"
            ):
                item = dict(zip(columns, row))
                item["db"] = path
                rows.append(item)
        except sqlite3.Error:
            continue
        finally:
            connection.close()
    return rows


def main() -> int:
    args = parse_args()
    os.makedirs(args.evidence, exist_ok=True)
    before = git_status(args.repo)
    home = isolated_home(args.repo, args.seed_project_db)
    env = run_env(home)
    pty = dog.Pty(
        [
            BIN,
            "--repo",
            args.repo,
            "tui",
            "--auto-approve",
            "--in-process",
            "--permission",
            "full-access",
        ],
        env,
        args.cols,
        args.rows,
    )
    try:
        if not (pty.wait_for("leveler", timeout=90) or pty.wait_stable(1.0, 90)):
            raise RuntimeError("the TUI never painted a frame")
        with open(os.path.join(args.evidence, "boot.txt"), "w") as handle:
            handle.write(pty.screen.text())

        before_target = None
        if args.watch_session:
            time.sleep(1.0)
            before_target = session_snapshot(home, args.watch_session)
            if before_target is None:
                raise RuntimeError("the watched session is not in the isolated store")
            with open(
                os.path.join(args.evidence, "target-before.json"), "w"
            ) as handle:
                json.dump(before_target, handle, ensure_ascii=False, indent=2)

        pty.write(args.prompt)
        time.sleep(0.5)
        pty.key("\r")
        started = wait_started(pty, 120)
        with open(os.path.join(args.evidence, "started.txt"), "w") as handle:
            handle.write(started)

        settled = wait_settled(pty, args.timeout)
        with open(os.path.join(args.evidence, "settled.txt"), "w") as handle:
            handle.write(settled)
        after_target = None
        if args.watch_session:
            after_target = session_snapshot(home, args.watch_session)
            with open(
                os.path.join(args.evidence, "target-after.json"), "w"
            ) as handle:
                json.dump(after_target, handle, ensure_ascii=False, indent=2)
            if after_target != before_target:
                raise RuntimeError(
                    "the reviewed session changed during the review: "
                    f"{before_target} -> {after_target}"
                )

        pos = pty.screen.find("查看报告")
        if pos is not None:
            pty.click(*pos)
            if pty.wait_for("审查报告", timeout=45):
                with open(os.path.join(args.evidence, "report.txt"), "w") as handle:
                    handle.write(pty.screen.text())
                for page in range(1, 9):
                    pty.key("\x1b[6~")
                    time.sleep(0.5)
                    with open(
                        os.path.join(args.evidence, f"report-page{page}.txt"), "w"
                    ) as handle:
                        handle.write(pty.screen.text())
                # Resize while the report page is open: the page must survive
                # both directions without dropping back to the conversation.
                pty.resize(args.cols - 24, args.rows - 10)
                with open(
                    os.path.join(args.evidence, "report-resized-small.txt"), "w"
                ) as handle:
                    handle.write(pty.screen.text())
                if "审查报告" not in pty.screen.text():
                    raise RuntimeError("the report page was lost on resize")
                pty.resize(args.cols, args.rows)
                with open(
                    os.path.join(args.evidence, "report-resized-back.txt"), "w"
                ) as handle:
                    handle.write(pty.screen.text())
                if "审查报告" not in pty.screen.text():
                    raise RuntimeError("the report page was lost on resize back")
            else:
                with open(os.path.join(args.evidence, "report-open-failed.txt"), "w") as handle:
                    handle.write(pty.screen.text())
        else:
            with open(os.path.join(args.evidence, "report-entry-missing.txt"), "w") as handle:
                handle.write(settled)

        after = git_status(args.repo)
        if after != before:
            with open(os.path.join(args.evidence, "workspace-before.txt"), "w") as handle:
                handle.write(before)
            with open(os.path.join(args.evidence, "workspace-after.txt"), "w") as handle:
                handle.write(after)
            raise RuntimeError("the review changed the working tree")

        reviews = read_reviews(home)
        with open(os.path.join(args.evidence, "reviews.json"), "w") as handle:
            json.dump(reviews, handle, ensure_ascii=False, indent=2)
        if not reviews:
            raise RuntimeError("no durable review row was written")

        summary = {
            "repo": args.repo,
            "prompt": args.prompt,
            "review": reviews[-1],
            "started": "审查中" in started,
            "settled_markers": [marker for marker in SETTLED if marker in settled],
            "report_entry": pos,
            "workspace_unchanged": True,
            "evidence": args.evidence,
            "home": home,
        }
        with open(os.path.join(args.evidence, "result.json"), "w") as handle:
            json.dump(summary, handle, ensure_ascii=False, indent=2)
        print(json.dumps(summary, ensure_ascii=False, indent=2))
        return 0
    finally:
        pty.close()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - evidence-first dogfood script
        evidence = parse_args().evidence if "--evidence" in sys.argv else "."
        os.makedirs(evidence, exist_ok=True)
        with open(os.path.join(evidence, "failure.txt"), "w") as handle:
            handle.write(repr(error))
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
