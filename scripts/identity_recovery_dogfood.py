#!/usr/bin/env python3
"""Identity and recovery dogfood with REAL binaries, a REAL daemon and a REAL
provider.

`pty_dogfood.py` owns the PTY/screen/process helpers; this file adds the four
fault scenarios the build-identity and crash-window work must survive:

  idle-old-runtime   an idle runtime of a different build already owns the
                     endpoint; a new client must hand over safely and work.
  busy-old-runtime   the old runtime is executing a real tool call; the new
                     client must never interrupt, terminate or replay it, and
                     must take over only after the work is done.
  conflict-recovery  the launcher is replaced mid-flight: exactly one build
                     conflict is recorded, replacement is not retried in a
                     loop, and restoring the launch artifact recovers.
  unknown-outcome    the runtime is killed during a side-effecting tool call:
                     the outcome stays unknown (never failed), recovery is
                     acknowledged explicitly, nothing is replayed, and both
                     the raw log marker and a history replay keep it unknown.

The two binaries come from two REAL builds of this source with different
compiled fingerprints (LEVELER_OLD_BIN / LEVELER_NEW_BIN). They are distinct
build identities, not a mocked version. This is a controlled real conflict;
the historical 2026-10-09/10-11 environment is not reproduced here.

Every scenario runs under a fresh LEVELER_HOME in /tmp, never the user's own
state. Provider config is copied from the user's config.toml, so turns reach
the real model.

Usage:
    python3 scripts/identity_recovery_dogfood.py --provider real
    python3 scripts/identity_recovery_dogfood.py --provider real --only busy-old-runtime
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import pty_dogfood as base  # noqa: E402

OLD_BIN = os.environ.get("LEVELER_OLD_BIN") or "/tmp/identity-dogfood/leveler-a"
NEW_BIN = os.environ.get("LEVELER_NEW_BIN") or "/tmp/identity-dogfood/leveler-b"

# The exact durable marker written by `acknowledge_crash_window`; the engine's
# `USER_ACKNOWLEDGED_CRASH_PREVIEW`. Spelled out so the check reads the raw
# event log, not the UI's copy of it.
CRASH_PREVIEW = (
    "user-acknowledged crash recovery: the interrupted call's outcome is unknown "
    "and the call was not replayed"
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def client_argv(env: base.Env, bin_path: str, extra: list[str] | None = None) -> list[str]:
    argv = [
        bin_path,
        "--repo",
        env.repo,
        "tui",
        "--auto-approve",
        "--permission",
        "full-access",
    ]
    return argv + (extra or [])


def isolated(provider: str, mock) -> base.Env:
    """A fresh home/repo. The UI language is pinned so the CLI handoff copy the
    client prints before the alternate screen is deterministic."""
    env = base.Env(provider, mock, short=True)
    env.env["LEVELER_LANG"] = "zh"
    return env


def copy_launcher(env: base.Env, source: str, name: str) -> str:
    path = os.path.join(env.root, name)
    staged = path + ".staged"
    shutil.copy2(source, staged)
    os.chmod(staged, 0o755)
    os.replace(staged, path)
    return path


def boot_client(
    report: base.Report,
    env: base.Env,
    bin_path: str,
    extra: list[str] | None = None,
) -> base.Pty:
    return base.boot(report, env, 120, 44, extra=extra or [], bin_path=bin_path)


def flat(pty_: base.Pty) -> str:
    return "".join(pty_.screen.text().split())


def wait_for(condition, timeout: float) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if condition():
            return True
        time.sleep(0.1)
    return False


def wait_exists(path: str, timeout: float) -> bool:
    return wait_for(lambda: os.path.exists(path), timeout)


def pid_alive(pid: int) -> bool:
    """True only for a live process; a reaped-but-not-yet-waited child is a
    zombie and is NOT a running runtime."""
    out = subprocess.run(
        ["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    return bool(out) and not out.startswith("Z")


def socket_path(env: base.Env) -> str:
    root = os.path.realpath(env.repo)
    digest = hashlib.sha256(root.encode()).hexdigest()[:16]
    return os.path.join(env.home, "run", "sockets", digest + ".sock")


def start_old_daemon(env: base.Env, bin_path: str) -> tuple[subprocess.Popen, str]:
    cfg = os.path.join(env.repo, "configs")
    os.makedirs(cfg, exist_ok=True)
    ready = os.path.join(env.root, "old-ready.json")
    log_path = os.path.join(env.root, "old-daemon.log")
    log = open(log_path, "w")
    proc = subprocess.Popen(
        [
            bin_path,
            "--config-dir",
            cfg,
            "--repo",
            env.repo,
            "serve",
            "--ready-json",
            ready,
        ],
        env=env.env,
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    if not wait_exists(ready, 60):
        raise RuntimeError("the manually started old runtime never became ready")
    return proc, log_path


def prove_two_builds(report: base.Report, scenario: str) -> bool:
    a, b = base.fingerprint_of(OLD_BIN), base.fingerprint_of(NEW_BIN)
    ok = bool(a) and bool(b) and a != b
    report.check(
        scenario,
        "the old and new binaries carry different build identities",
        ok,
        "old %s · new %s" % (a or "<none>", b or "<none>"),
    )
    return ok


def log_text(env: base.Env) -> str:
    try:
        with open(os.path.join(env.home, "leveler.log")) as fh:
            return fh.read()
    except OSError:
        return ""


def wait_log(env: base.Env, needle: str, timeout: float) -> bool:
    return wait_for(lambda: needle in log_text(env), timeout)


def log_lines(env: base.Env, needle: str) -> list[str]:
    return [line for line in log_text(env).splitlines() if needle in line]


def ask_shell_turn(
    pty_: base.Pty,
    env: base.Env,
    marker: str,
    file_name: str,
    timeout: float = 300,
) -> tuple[bool, str]:
    """Ask the real model to run one shell command, then answer a marker.

    The command is a separate turn from the marker answer: asking for both in
    one sentence lets a model answer the marker without calling the tool, which
    would make the file check vacuous. The file is outside the model's control:
    only the runtime executing the tool creates it.
    """
    path = os.path.join(env.repo, file_name)
    pty_.write(
        "调用 shell_command 工具，只调用一次，cmd 为：echo %s > %s。"
        "这是前台命令，不要设置 background，不要调用其他工具。" % (marker, path)
    )
    pty_.key("\r")
    ran = base.wait_file(path, marker, timeout)
    # Let the tool turn settle before the next prompt: input submitted while
    # the turn runs is a different product path (staged, not this one).
    base.wait_until(pty_, lambda text: "Esc 打断" not in text, timeout=120)
    pty_.write("只回复这个标记，不要任何其他内容：%s" % marker)
    pty_.key("\r")

    def answered() -> bool:
        return (
            pty_.screen.text().count(marker) >= 2
            and marker not in base.composer_text(pty_)
        )

    ok = base.wait_until(pty_, lambda _: answered(), timeout=timeout)
    content = ""
    try:
        with open(path) as fh:
            content = fh.read().strip()
    except OSError:
        pass
    return ok and ran, content


def kill_isolated_processes(env: base.Env, binaries: list[str]) -> None:
    for token in ("serve", "execution-host"):
        for binary in binaries:
            try:
                for pid in base.repo_processes(env.repo, token, binary):
                    os.kill(pid, signal.SIGKILL)
            except Exception:
                pass


# ---------------------------------------------------------------------------
# Scenario A — an idle runtime of another build owns the endpoint
# ---------------------------------------------------------------------------


def scenario_idle_old_runtime(report: base.Report, provider: str, mock) -> base.Pty | None:
    scenario = "idle-old-runtime"
    env = isolated(provider, mock)
    if not prove_two_builds(report, scenario):
        return None
    launcher = copy_launcher(env, NEW_BIN, "leveler")
    proc = None
    pty_ = None
    try:
        proc, _ = start_old_daemon(env, OLD_BIN)
        old_pids = base.daemon_pids(env.repo, OLD_BIN)
        report.check(
            scenario,
            "an idle runtime of a different build owns the endpoint",
            bool(old_pids),
            repr(old_pids),
        )
        report.check(
            scenario,
            "the old runtime's socket is bound before the new client starts",
            os.path.exists(socket_path(env)),
            socket_path(env),
        )
        if not old_pids:
            return None
        old_pid = old_pids[0]
        pty_ = boot_client(report, env, launcher)

        took_over = wait_for(
            lambda: (not pid_alive(old_pid))
            and any(pid != old_pid for pid in base.daemon_pids(env.repo, launcher)),
            120,
        )
        new_pids = [pid for pid in base.daemon_pids(env.repo, launcher) if pid != old_pid]
        report.dump("A-1-handover", pty_)
        report.check(
            scenario,
            "the idle old runtime handed over and a runtime of the new build serves",
            took_over and bool(new_pids),
            "old %d alive=%s new %r" % (old_pid, pid_alive(old_pid), new_pids),
        )
        report.check(
            scenario,
            "an idle handover is not reported as a version conflict",
            "版本不一致" not in flat(pty_)
            and "无法自动接管" not in flat(pty_),
            pty_.screen.text()[-700:],
        )

        marker = "IDLE-OLD-OK"
        answered, content = ask_shell_turn(
            pty_, env, marker, "idle-old-ok.txt"
        )
        report.dump("A-2-real-turn", pty_)
        report.check(
            scenario,
            "the handed-over runtime executes a real agent turn",
            answered and content == marker,
            "answered=%s file=%r" % (answered, content),
        )
        return pty_
    finally:
        if pty_ is not None:
            pty_.close()
        if proc is not None:
            try:
                os.kill(proc.pid, signal.SIGKILL)
            except OSError:
                pass
        kill_isolated_processes(env, [launcher, OLD_BIN])


# ---------------------------------------------------------------------------
# Scenario B — the old runtime is executing a real turn
# ---------------------------------------------------------------------------


def scenario_busy_old_runtime(report: base.Report, provider: str, mock) -> base.Pty | None:
    scenario = "busy-old-runtime"
    env = isolated(provider, mock)
    if not prove_two_builds(report, scenario):
        return None
    old_launcher = copy_launcher(env, OLD_BIN, "old-leveler")
    new_launcher = copy_launcher(env, NEW_BIN, "new-leveler")
    busy = os.path.join(env.repo, "busy.txt")
    pty_old = None
    pty_new = None
    try:
        pty_old = boot_client(report, env, old_launcher)
        # Keep the whole command inside the 15s foreground handoff window
        # (leveler-tools FOREGROUND_HANDOFF_AFTER): a longer command is adopted
        # as a `goal`-lifetime background task, and the turn's own terminal
        # settles that task by design. The scenario is about the handover
        # leaving the old turn alone, not about background lifetime, so the
        # command must finish while the turn that owns it is still running.
        prompt = (
            "调用 shell_command 工具，只调用一次，cmd 为："
            "echo start >> %s; sleep 12; echo end >> %s。"
            "这是前台命令，不要设置 background，不要调用其他工具，"
            "等待命令结束后回复完成。" % (busy, busy)
        )
        pty_old.write(prompt)
        pty_old.key("\r")
        began = base.wait_file(busy, "start", 300)
        report.dump("B-1-old-turn-running", pty_old)
        report.check(
            scenario,
            "the old runtime is executing a real tool call",
            began,
            pty_old.screen.text()[-700:],
        )
        if not began:
            return pty_old
        old_pids = base.daemon_pids(env.repo, old_launcher)
        report.check(
            scenario,
            "the old runtime is the detached owner of this repository",
            bool(old_pids),
            repr(old_pids),
        )
        if not old_pids:
            return pty_old

        # Same isolated home and repo, a second client of the other build.
        pty_new = base.Pty(client_argv(env, new_launcher), dict(env.env), 120, 44)
        deferred = base.wait_until(
            pty_new,
            lambda text: "旧版本仍在执行任务" in "".join(text.split())
            or "旧版本 CodeLeveler 仍在执行任务" in "".join(text.split()),
            timeout=90,
        )
        report.dump("B-2-new-client-blocked", pty_new)
        report.check(
            scenario,
            "the new client reports that the old runtime is still working",
            deferred,
            pty_new.screen.text()[-900:],
        )
        report.check(
            scenario,
            "the new client did not terminate the old runtime",
            all(pid_alive(pid) for pid in old_pids)
            and sorted(base.daemon_pids(env.repo, old_launcher)) == sorted(old_pids),
            "before %r now %r" % (old_pids, base.daemon_pids(env.repo, old_launcher)),
        )
        with open(busy) as fh:
            during = fh.read().splitlines()
        report.check(
            scenario,
            "the old work was neither cancelled nor replayed while it was blocked",
            during == ["start"],
            repr(during),
        )

        # Closing the TUI must not touch the detached runtime or its work. The
        # old client is the only other supervisor of this endpoint, so exiting
        # it also removes the reviver that raced the handover in the old design.
        report.check(
            scenario,
            "the new client has not painted a TUI of its own while the old work runs",
            "╭" not in pty_new.screen.text()
            and not base.daemon_pids(env.repo, new_launcher),
            "new daemons=%r" % (base.daemon_pids(env.repo, new_launcher),),
        )
        pty_old.close()
        pty_old = None
        finished = base.wait_file(busy, "end", 180)
        with open(busy) as fh:
            after = fh.read().splitlines()
        report.dump("B-3-work-finished-after-client-exit", pty_new)
        report.check(
            scenario,
            "the detached runtime finished the work exactly once after its client exited",
            finished and after == ["start", "end"],
            repr(after),
        )
        # The new client's retirement request was deferred while the turn was
        # running. It re-asks on its own interval; the runtime becoming idle is
        # the only thing that lets the handover proceed.
        took_over = base.wait_until(
            pty_new,
            lambda text: "╭" in text,
            timeout=180,
        ) and wait_for(lambda: bool(base.daemon_pids(env.repo, new_launcher)), 120)
        report.dump("B-4-new-client-took-over", pty_new)
        report.check(
            scenario,
            "once the old runtime is idle the waiting client takes over without a restart",
            took_over
            and all(not pid_alive(pid) for pid in old_pids)
            and bool(base.daemon_pids(env.repo, new_launcher)),
            "old=%r now=%r" % (old_pids, base.daemon_pids(env.repo, new_launcher)),
        )

        marker = "BUSY-OLD-OK"
        answered, content = ask_shell_turn(
            pty_new, env, marker, "busy-old-ok.txt"
        )
        report.dump("B-5-new-client-real-turn", pty_new)
        report.check(
            scenario,
            "the new client's runtime executes a real agent turn",
            answered and content == marker,
            "answered=%s file=%r" % (answered, content),
        )
        return pty_new
    finally:
        for pty_ in (pty_old, pty_new):
            if pty_ is not None:
                pty_.close()
        kill_isolated_processes(env, [old_launcher, new_launcher])


# ---------------------------------------------------------------------------
# Scenario C — a repeated build conflict and its recovery
# ---------------------------------------------------------------------------


def scenario_conflict_recovery(report: base.Report, provider: str, mock) -> base.Pty | None:
    scenario = "conflict-recovery"
    env = isolated(provider, mock)
    env.env["RUST_LOG"] = "leveler=info"
    if not prove_two_builds(report, scenario):
        return None
    launcher = copy_launcher(env, OLD_BIN, "leveler")
    pty_ = None
    try:
        pty_ = boot_client(report, env, launcher)
        first = base.daemon_pids(env.repo, launcher)
        report.check(
            scenario,
            "the client attached to its own runtime before the fault",
            bool(first),
            repr(first),
        )
        if not first:
            return pty_

        # Replace the launch artifact atomically, then kill the runtime so the
        # client's next revival starts the OTHER build.
        staged = launcher + ".new"
        shutil.copy2(NEW_BIN, staged)
        os.chmod(staged, 0o755)
        os.replace(staged, launcher)
        for pid in first:
            os.kill(pid, signal.SIGKILL)

        detected = wait_log(env, "runtime build conflict detected", 120)
        report.check(
            scenario,
            "a real build conflict is detected",
            detected,
            log_text(env)[-900:],
        )
        if not detected:
            return pty_

        # Observe the whole stable window: one daemon, no respawn churn.
        samples: list[set[int]] = []
        end = time.time() + 12
        while time.time() < end:
            samples.append(set(base.daemon_pids(env.repo, launcher)))
            time.sleep(0.5)
        seen = set().union(*samples) if samples else set()
        backoffs = log_lines(env, "replacement is not re-attempted")
        attempts = []
        for line in backoffs:
            for word in line.split():
                if word.startswith("attempt="):
                    attempts.append(int(word.split("=", 1)[1]))
        detected_lines = log_lines(env, "runtime build conflict detected")
        report.dump("C-1-conflict-window", pty_)
        report.check(
            scenario,
            "the unchanged conflict is recorded once, not once per retry",
            len(detected_lines) == 1,
            "%d detected lines" % len(detected_lines),
        )
        report.check(
            scenario,
            "later attempts report the same conflict and do not re-attempt replacement",
            len(backoffs) >= 2 and attempts == sorted(attempts),
            "backoffs=%d attempts=%r" % (len(backoffs), attempts),
        )
        report.check(
            scenario,
            "the conflict window spawns no replacement storm",
            len(seen) == 1 and all(len(sample) <= 1 for sample in samples),
            "pids seen %r" % (sorted(seen),),
        )

        # Restore the launch artifact the client re-execs. That is a real
        # identity change, so the conflict must stop applying.
        restore = launcher + ".restore"
        shutil.copy2(OLD_BIN, restore)
        os.chmod(restore, 0o755)
        os.replace(restore, launcher)
        cleared = wait_log(env, "runtime build conflict no longer applies", 60)
        recovered = base.wait_until(
            pty_,
            lambda text: "问题 1" not in text and "版本不一致" not in text,
            timeout=120,
        )
        report.dump("C-2-recovered", pty_)
        report.check(
            scenario,
            "restoring the launch artifact releases the conflict pin",
            cleared,
            log_text(env)[-900:],
        )
        report.check(
            scenario,
            "the recovered client is served by the restored build and shows no version issue",
            recovered,
            pty_.screen.text()[-700:],
        )
        new_pids = [
            pid
            for pid in base.daemon_pids(env.repo, launcher)
            if pid not in first
        ]
        report.check(
            scenario,
            "a replacement runtime of the restored build is serving",
            bool(new_pids) and not any(pid_alive(pid) for pid in seen - set(new_pids)),
            "conflict pids %r now %r" % (sorted(seen), new_pids),
        )

        marker = "CONFLICT-RECOVERED"
        answered, content = ask_shell_turn(
            pty_, env, marker, "conflict-ok.txt"
        )
        report.dump("C-3-real-turn", pty_)
        report.check(
            scenario,
            "the recovered runtime executes a real agent turn",
            answered and content == marker,
            "answered=%s file=%r" % (answered, content),
        )
        return pty_
    finally:
        if pty_ is not None:
            pty_.close()
        kill_isolated_processes(env, [launcher, OLD_BIN, NEW_BIN])


# ---------------------------------------------------------------------------
# Scenario D — an interrupted side-effecting call keeps its Unknown outcome
# ---------------------------------------------------------------------------


def project_database(env: base.Env) -> str | None:
    root = os.path.join(env.home, "state", "projects")
    try:
        names = os.listdir(root)
    except OSError:
        return None
    for name in names:
        path = os.path.join(root, name, "sessions.db")
        if os.path.isfile(path):
            return path
    return None


def read_events(database: str) -> list[dict]:
    connection = sqlite3.connect("file:%s?mode=ro" % database, uri=True, timeout=10)
    try:
        rows = connection.execute(
            "SELECT session_id, sequence, type, payload FROM events ORDER BY sequence"
        ).fetchall()
    finally:
        connection.close()
    events = []
    for session_id, sequence, kind, payload in rows:
        try:
            body = json.loads(payload)
        except (TypeError, ValueError):
            body = {}
        events.append(
            {
                "session_id": session_id,
                "sequence": sequence,
                "type": kind,
                "payload": body.get("payload", body),
            }
        )
    return events


def scenario_unknown_outcome(report: base.Report, provider: str, mock) -> base.Pty | None:
    scenario = "unknown-outcome"
    env = isolated(provider, mock)
    launcher = copy_launcher(env, OLD_BIN, "leveler")
    dup = os.path.join(env.repo, "crash-window.txt")
    pty_ = None
    try:
        pty_ = boot_client(report, env, launcher)
        daemons = base.daemon_pids(env.repo, launcher)
        report.check(
            scenario,
            "the client attached to its own runtime before the fault",
            bool(daemons),
            repr(daemons),
        )
        if not daemons:
            return pty_

        prompt = (
            "调用 shell_command 工具，只调用一次，cmd 为："
            "echo start >> %s; sleep 20; echo end >> %s。"
            "这是前台命令，不要设置 background，不要调用其他工具，"
            "等待命令结束后回复完成。" % (dup, dup)
        )
        pty_.write(prompt)
        pty_.key("\r")
        began = base.wait_file(dup, "start", 300)
        report.dump("D-1-call-running", pty_)
        report.check(
            scenario,
            "the real model started a side-effecting tool call",
            began,
            pty_.screen.text()[-700:],
        )
        if not began:
            return pty_

        killed = base.daemon_pids(env.repo, launcher)
        for pid in killed:
            os.kill(pid, signal.SIGKILL)
        restarted = base.wait_until(
            pty_, lambda text: "刚才的任务已中断" in text, timeout=90
        )
        report.dump("D-2-interrupted", pty_)
        report.check(
            scenario,
            "a restart during the call is reported",
            restarted,
            pty_.screen.text()[-700:],
        )
        unknown = base.wait_until(
            pty_, lambda text: "状态未知" in text, timeout=60
        )
        report.check(
            scenario,
            "the interrupted call is presented as unknown, not as a failure",
            unknown,
            pty_.screen.text()[-700:],
        )

        # With an empty composer `/recover` is reachable. It opens the
        # acknowledgement and does not choose for the user.
        pty_.write("/recover")
        pty_.key("\r")
        overlay = base.wait_until(
            pty_, lambda text: "恢复确认" in text and "确认恢复" in text, timeout=30
        )
        report.dump("D-3-recovery-overlay", pty_)
        report.check(
            scenario,
            "/recover opens the acknowledgement instead of acting on its own",
            overlay,
            pty_.screen.text()[-700:],
        )
        if not overlay:
            return pty_
        pty_.key("\x1b")  # cancel: nothing is acknowledged
        cancelled = base.wait_until(
            pty_, lambda text: "恢复确认" not in text, timeout=15
        )
        report.check(
            scenario,
            "cancelling the acknowledgement leaves the gate in place",
            cancelled,
            pty_.screen.text()[-700:],
        )
        if not cancelled:
            return pty_

        # The runtime refuses to guess what the call did. The refused input
        # stays in the composer; nothing is executed.
        marker = "UNKNOWN-AFTER-ACK"
        pty_.write("只回复这个标记，不要任何其他内容：%s" % marker)
        pty_.key("\r")
        gated = base.wait_until(
            pty_, lambda text: "任务恢复已暂停" in text, timeout=120
        )
        report.dump("D-4-recovery-required", pty_)
        report.check(
            scenario,
            "an unresolved call stops the next turn instead of guessing",
            gated,
            pty_.screen.text()[-700:],
        )
        # `/recover` only parses at the START of the composer. Telling a user
        # to type it next to their preserved draft would ask them to discard
        # it, so the same acknowledgement is presented directly.
        reopened = base.wait_until(
            pty_, lambda text: "恢复确认" in text and "确认恢复" in text, timeout=30
        )
        report.dump("D-5-acknowledgement-presented", pty_)
        report.check(
            scenario,
            "the refusal presents the acknowledgement without clearing the draft",
            reopened,
            pty_.screen.text()[-700:],
        )
        if not reopened:
            return pty_
        pty_.key("\x1b[B")  # move from the safe default (cancel) to confirm
        pty_.key("\r")
        acknowledged = base.wait_until(
            pty_, lambda text: "已确认恢复" in text, timeout=30
        )
        report.dump("D-6-acknowledged", pty_)
        report.check(
            scenario,
            "acknowledgement is explicit and names the unknown calls",
            acknowledged,
            pty_.screen.text()[-700:],
        )
        report.check(
            scenario,
            "the refused input is preserved for the user",
            marker in base.composer_text(pty_),
            pty_.screen.text()[-400:],
        )

        # The preserved input was never consumed; submit it now that the
        # runtime is unblocked and let a real turn run.
        pty_.key("\r")

        def answered() -> bool:
            return (
                pty_.screen.text().count(marker) >= 2
                and marker not in base.composer_text(pty_)
            )

        reached = base.wait_until(pty_, lambda _: answered(), timeout=300)
        report.dump("D-7-new-turn", pty_)
        report.check(
            scenario,
            "after acknowledgement the preserved input reaches the real model",
            reached,
            pty_.screen.text()[-700:],
        )

        finished = base.wait_file(dup, "end", 60)
        with open(dup) as fh:
            lines = fh.read().splitlines()
        report.check(
            scenario,
            "the interrupted command finished on its own",
            finished,
            repr(lines),
        )
        report.check(
            scenario,
            "the interrupted call was never replayed",
            lines == ["start", "end"],
            repr(lines),
        )

        # Raw durable evidence: one start, one recovery marker, nothing else
        # for that call.
        database = project_database(env)
        events = read_events(database) if database else []
        starts = [e for e in events if e["type"] == "tool_call_started"]
        target = None
        for event in starts:
            if event["payload"].get("name") == "shell_command":
                target = event["payload"].get("call_id")
                break
        markers = [
            e
            for e in events
            if e["type"] == "tool_call_finished"
            and e["payload"].get("call_id") == target
            and e["payload"].get("preview") == CRASH_PREVIEW
        ]
        finishes = [
            e
            for e in events
            if e["type"] == "tool_call_finished"
            and e["payload"].get("call_id") == target
        ]
        starts_for_call = [
            e
            for e in events
            if e["type"] == "tool_call_started"
            and e["payload"].get("call_id") == target
        ]
        with open(os.path.join(base.EVIDENCE, "D-7-raw-events.json"), "w") as fh:
            json.dump(
                {
                    "database": database,
                    "target_call": target,
                    "starts": starts_for_call,
                    "finishes": finishes,
                    "markers": markers,
                },
                fh,
                indent=2,
                default=str,
            )
        report.check(
            scenario,
            "the durable log holds exactly one unknown-outcome marker for the call",
            target is not None
            and len(markers) == 1
            and len(finishes) == 1
            and len(starts_for_call) == 1,
            "call=%s starts=%d finishes=%d markers=%d"
            % (target, len(starts_for_call), len(finishes), len(markers)),
        )
        marker_payload = markers[0]["payload"] if markers else {}
        report.check(
            scenario,
            "the marker keeps the unknown outcome and does not claim success",
            marker_payload.get("is_error") is True
            and marker_payload.get("preview") == CRASH_PREVIEW,
            json.dumps(marker_payload, ensure_ascii=False)[:500],
        )
        report.check(
            scenario,
            "acknowledgement itself never wrote a success or a replay",
            all(
                e["payload"].get("is_error") is True
                for e in finishes
                if e["payload"].get("call_id") == target
            ),
            json.dumps(finishes, ensure_ascii=False)[:500],
        )

        # A fresh client replaying this session must reach the same fact from
        # durable history: the call is Unknown, never Failed.
        session_id = events[-1]["session_id"] if events else None
        if session_id:
            pty_.close()
            pty_ = boot_client(report, env, launcher, extra=["--session", session_id])
            replayed = base.wait_until(
                pty_,
                lambda text: "状态未知" in text,
                timeout=90,
            )
            report.dump("D-9-history-replay", pty_)
            lines = pty_.screen.lines()
            # The replayed tool group is the compact summary row; the command
            # text lives in the user turn above it.
            rows = [line for line in lines if "执行了 1 个命令" in line]
            unknown_row = any(
                "?" in line and "状态未知" in line for line in rows
            )
            report.check(
                scenario,
                "history replay still presents the call as unknown, not failed",
                replayed and unknown_row and "✗" not in pty_.screen.text(),
                "rows=%r\n%s" % (rows, pty_.screen.text()[-900:]),
            )
        return pty_
    finally:
        if pty_ is not None:
            pty_.close()
        kill_isolated_processes(env, [launcher, OLD_BIN])


SCENARIOS = {
    "idle-old-runtime": scenario_idle_old_runtime,
    "busy-old-runtime": scenario_busy_old_runtime,
    "conflict-recovery": scenario_conflict_recovery,
    "unknown-outcome": scenario_unknown_outcome,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["real"], default="real")
    parser.add_argument("--only", default="", help="comma-separated scenario names")
    args = parser.parse_args()

    base.BIN = NEW_BIN
    base.TRANSPORT = "daemon"
    os.makedirs(base.EVIDENCE, exist_ok=True)
    for path in (OLD_BIN, NEW_BIN):
        if not os.path.isfile(path):
            print("missing %s — build/stage two real builds first" % path)
            return 2

    selected = [s for s in (args.only.split(",") if args.only else SCENARIOS) if s]
    report = base.Report("identity-" + args.provider)
    for name in selected:
        print("=== scenario %s (%s provider) ===" % (name, args.provider))
        try:
            SCENARIOS[name](report, args.provider, None)
        except Exception as exc:  # a crashed scenario is a FAIL, not a skip
            report.check(name, "scenario completed", False, repr(exc))
            import traceback

            traceback.print_exc()
    path = report.save()
    print(
        "\nassertions: %d, failures: %d, blocked: %d"
        % (len(report.results), report.failures, report.blocked)
    )
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
