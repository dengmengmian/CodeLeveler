#!/usr/bin/env python3
"""Real-model TUI rendering dogfood driver.

Drives the real CodeLeveler TUI (real provider, real PTY, real terminal byte
stream) for the rendering-correctness dogfood tasks A-D. The screen model is
the same VT subset used by the deterministic PTY harness: absolute cursor
moves, CSI erase, SGR and 8-column tab stops. Frames are reconstructed from the
bytes the terminal actually received; nothing here reads the TUI's in-memory
buffer or its logs.

This is supervisor tooling only. It never imports CodeLeveler code.
"""
import argparse
import codecs
import fcntl
import glob
import json
import os
import pty
import select
import signal
import sqlite3
import struct
import sys
import termios
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Screen  # noqa: E402  (same VT model as the mock harness)

SPINNERS = set("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⣾⣽⣻⢿⡿⣟⣯⣷")
RESIDUE = ["impofmt", "pack\"fmt\"", "paos\" mt\"", "))]", "pt\"", "il)", "funce:="]


def now():
    return time.strftime("%H:%M:%S")


class Tui:
    def __init__(self, binary, repo, model, cols, rows, extra_args, env_extra):
        self.binary, self.repo, self.model = binary, repo, model
        self.cols, self.rows = cols, rows
        self.extra_args = extra_args
        self.env_extra = env_extra
        self.raw = bytearray()
        self.screen = Screen(cols, rows)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pid = self.fd = None
        self.dead = False

    def start(self):
        env = dict(os.environ)
        env["TERM"] = "xterm-256color"
        env.update(self.env_extra)
        argv = [
            self.binary,
            "--repo",
            self.repo,
            "tui",
            "--model",
            self.model,
            "--auto-approve",
            "--permission",
            "full-access",
            "--in-process",
        ] + list(self.extra_args)
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.repo)
            try:
                os.execve(self.binary, argv, env)
            except Exception:
                os._exit(127)
        self.pid, self.fd = pid, fd
        self.win(self.rows, self.cols)
        return self

    def win(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(self, timeout):
        got = False
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                ready, _, _ = select.select([self.fd], [], [], 0.1)
            except (OSError, ValueError):
                self.dead = True
                break
            if not ready:
                break
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                self.dead = True
                break
            if not chunk:
                self.dead = True
                break
            self.raw.extend(chunk)
            self.screen.feed(self.decoder.decode(chunk))
            got = True
        return got

    def send(self, data):
        if isinstance(data, str):
            data = data.encode()
        try:
            os.write(self.fd, data)
        except OSError:
            self.dead = True

    def text(self):
        return "\n".join(self.screen.lines())

    def rows_now(self):
        return self.screen.lines()

    def busy(self):
        text = self.text()
        return (
            any(ch in text for ch in SPINNERS)
            or "等待模型" in text
            or "Esc 打断" in text
            or "执行中" in text
        )

    def idle_chrome(self):
        text = self.text()
        return ("/ 命令" in text or "Shift+Tab" in text) and not self.busy()

    def resize(self, rows, cols):
        self.win(rows, cols)
        self.screen.resize(cols, rows)
        self.rows, self.cols = rows, cols
        try:
            os.kill(self.pid, signal.SIGWINCH)
        except OSError:
            self.dead = True

    def close(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError, OSError):
            pass


def db_find(repo):
    real = os.path.realpath(repo)
    for db in glob.glob(
        os.path.expanduser("~/.leveler/state/projects/*/sessions.db")
    ):
        try:
            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=0.1)
            row = con.execute(
                "SELECT 1 FROM sessions WHERE repository=? LIMIT 1", (real,)
            ).fetchone()
            con.close()
            if row:
                return db
        except sqlite3.Error:
            continue
    return None


def db_retry(fn, default=None, tries=5):
    for i in range(tries):
        try:
            return fn()
        except sqlite3.Error:
            time.sleep(0.1 * (i + 1))
    return default


def sessions_in(db):
    def q():
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=0.1)
        rows = con.execute("SELECT id FROM sessions").fetchall()
        con.close()
        return {r[0] for r in rows}
    return db_retry(q, set())


def session_state(db, sid):
    def q():
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=0.1)
        status = con.execute(
            "SELECT status FROM sessions WHERE id=?", (sid,)
        ).fetchone()
        running = con.execute(
            "SELECT COUNT(*) FROM turns WHERE session_id=? AND"
            " (status='running' OR finished_at IS NULL)",
            (sid,),
        ).fetchone()[0]
        turns = con.execute(
            "SELECT COUNT(*) FROM turns WHERE session_id=?", (sid,)
        ).fetchone()[0]
        con.close()
        return {"status": status[0] if status else None, "turns": turns,
                "running": running}
    return db_retry(q, {"status": None, "turns": 0, "running": 1})


class Run:
    def __init__(self, args):
        self.args = args
        self.out = args.out
        self.frames = os.path.join(self.out, "frames")
        os.makedirs(self.frames, exist_ok=True)
        self.log = open(os.path.join(self.out, "driver.log"), "a")
        self.t0 = time.time()
        self.frame_ix = 0
        self.last_frame = 0.0
        self.sid = args.resume_session
        self.db = None
        self.before = set()
        self.result = {"task": args.task, "frames": 0, "notes": []}

    def note(self, kind, **kw):
        rec = {"t": round(time.time() - self.t0, 2), "kind": kind, **kw}
        self.log.write(json.dumps(rec, ensure_ascii=False) + "\n")
        self.log.flush()
        print(f"[{now()}] {kind} {kw}", flush=True)

    def snap(self, tui, label):
        p = os.path.join(self.frames, f"{self.frame_ix:03d}_{label}.txt")
        with open(p, "w") as fh:
            fh.write("\n".join(tui.rows_now()))
        self.frame_ix += 1
        self.result["frames"] += 1
        return p

    def periodic(self, tui, min_gap=2.0):
        if time.time() - self.last_frame >= min_gap:
            self.snap(tui, "stream")
            self.last_frame = time.time()

    def wait(self, tui, pred, timeout, label):
        deadline = time.time() + timeout
        started = time.time()
        while time.time() < deadline:
            tui.pump(0.15)
            if tui.dead:
                raise RuntimeError(f"{label}: TUI died")
            if pred(tui):
                return time.time() - started
        return None

    def wait_boot(self, tui, timeout=120):
        if self.wait(tui, lambda t: t.idle_chrome(), timeout, "boot") is None:
            raise RuntimeError("boot: no idle chrome")

    def ensure_session(self, timeout=30):
        """Discover the session the first submit created (none until then).

        The project DB does not exist until the first turn is persisted, so it
        is re-discovered here rather than only at boot.
        """
        deadline = time.time() + timeout
        while not self.sid and time.time() < deadline:
            if not self.db:
                self.db = db_find(self.args.repo)
            if self.db:
                new = sessions_in(self.db) - self.before
                if new:
                    self.sid = sorted(new)[0]
                    self.note("session", sid=self.sid)
                    return self.sid
            time.sleep(0.2)
        return self.sid


def submit(tui, text):
    tui.send(text + "\r")


def find_answer_frame(rows):
    return "\n".join(rows)


def c0_counts(raw):
    allowed = {0x09, 0x0A, 0x0D, 0x1B, 0x07}
    counts = {"tab": 0, "nul": 0, "bel": 0, "other_c0": 0}
    for b in raw:
        if b == 0x09:
            counts["tab"] += 1
        elif b == 0x00:
            counts["nul"] += 1
        elif b == 0x07:
            counts["bel"] += 1
        elif b < 0x20 and b not in allowed:
            counts["other_c0"] += 1
    return counts


def scan_frame(rows):
    hits = {}
    for sig in RESIDUE:
        for i, row in enumerate(rows):
            if sig in row:
                hits.setdefault(sig, []).append([i, row])
    return hits


TASKS = {
    "A": (
        "请用中文写一篇较长的 Go 语言序列化技术说明，并严格按以下结构输出：\n"
        "1. 一个二级标题「背景」和两段中文说明；\n"
        "2. 一个 Bash 代码块（三行左右）；\n"
        "3. 一个 Go 代码块，程序用于演示 bcrypt 校验，import 区必须包含 "
        "\"fmt\"、\"os\"、\"golang.org/x/crypto/bcrypt\" 三行，并且这三行要用真实的制表符"
        "（\\t，不是空格）缩进；\n"
        "4. 一个无序列表，列出四个要点；\n"
        "5. 一段包含行内代码和反斜杠、引号、方括号的说明文字；\n"
        "6. 以一句话总结结尾。不要省略任何代码，直接给出完整内容。"
    ),
    "B": (
        "请用中文分十个小节详细讲解 Go 的错误处理演进，每节至少 120 字，"
        "并在其中穿插两个 Go 代码块和两个 Bash 代码块。代码块请使用真实的制表符缩进。"
        "尽量输出完整、不要提前结束。"
    ),
    "C": (
        "仓库是 duf（Go）。请完成一个小功能并自行验证："
        "让 stringToSize 同时接受小写和大写的容量前缀（例如 10g 与 10G 等值，"
        "2m 与 2M 等值；2Gb 这种多字母前缀仍然非法）；"
        "错误信息里保留原有语义并说明允许 K/M/G/T/P/E 及其小写形式；"
        "在 config_test.go 或新的测试文件里补充表驱动测试覆盖大写、小写和非法输入；"
        "最后运行 go test ./...，把结果总结出来。"
    ),
    "D": (
        "请用中文写一段较长的 Go 并发编程说明，包含至少三个代码块和一个小节标题，"
        "内容要足够长，让我可以在生成过程中测试打断和继续。"
    ),
    # B2 covers the brief's B content that B's ten-section prompt does not
    # force: a Markdown table, three languages and a long code line.
    "B2": (
        "请用中文输出一份完整的 Go 并发与错误处理工程分析，要求："
        "1) 多级标题（至少三级）；2) 有序和无序列表；"
        "3) 一个 Markdown 表格（至少 3 列、4 行数据，含中英文混排）；"
        "4) 分别给出一个 Go、一个 Bash、一个 JSON 代码块；"
        "5) 至少一条超过 120 个字符的长代码行；6) 中英文技术术语混排。"
        "内容尽量完整，不要提前结束。"
    ),
}


def task_a(run, tui):
    prompt = TASKS[run.args.task]
    run.note("submit", prompt_chars=len(prompt))
    submit(tui, prompt)
    streaming = run.wait(tui, lambda t: t.busy(), 120, "A streaming")
    if streaming is None:
        raise RuntimeError("A: never entered streaming")
    run.note("streaming", after_s=round(streaming, 2))
    idle = wait_idle(run, tui, "A")
    if idle is None:
        raise RuntimeError("A: timeout")
    run.snap(tui, "final")
    rows = tui.rows_now()
    text = find_answer_frame(rows)
    run.result["residue"] = scan_frame(rows)
    run.result["has_go_imports"] = sum(
        1
        for r in rows
        if r.startswith(" " * 10) and ('"fmt"' in r or '"os"' in r or "golang.org/x/crypto/bcrypt" in r)
    )
    run.result["tab_expanded_rows"] = [
        r for r in rows if '"fmt"' in r or "golang.org/x/crypto/bcrypt" in r
    ][:8]
    run.result["codepoints"] = len(text)
    run.result["session"] = run.sid
    return "idle"


def task_b(run, tui, db):
    prompt = TASKS["B"]
    run.note("submit", prompt_chars=len(prompt))
    submit(tui, prompt)
    if run.wait(tui, lambda t: t.busy(), 120, "B streaming") is None:
        raise RuntimeError("B: never entered streaming")
    run.note("streaming")
    tui.pump(1.0)
    run.snap(tui, "before_scroll")
    tui.send("\x1b[5~")
    tui.pump(1.0)
    run.snap(tui, "scrolled_up")
    tui.send("\x1b[5~")
    tui.pump(1.0)
    run.snap(tui, "scrolled_up_more")
    tui.resize(30, 90)
    tui.pump(1.5)
    run.snap(tui, "resized_90x30")
    tui.send("草稿输入")
    tui.pump(1.0)
    run.snap(tui, "composer_draft")
    tui.resize(run.args.rows, run.args.cols)
    tui.pump(1.0)
    run.snap(tui, "resized_back")
    # Clear the draft with Backspace, not Ctrl+C: while a turn is streaming
    # Ctrl+C escalates to cancel, which would end the run under test.
    tui.send("\x7f" * len("草稿输入"))
    tui.pump(0.3)
    tui.send("\x1b[6~")
    tui.pump(0.5)
    idle_since = wait_idle(run, tui, "B")
    run.snap(tui, "stream_final")
    if idle_since is None:
        raise RuntimeError("B: stream did not finish")
    tui.send("/recap\r")
    run.wait(tui, lambda t: "回顾" in t.text() and not t.busy(), 120, "B recap")
    tui.pump(1.0)
    run.snap(tui, "recap")
    run.result["recap_seen"] = "回顾" in tui.text()
    submit(tui, "谢谢，请用一句话总结上面的内容。")
    if run.wait(tui, lambda t: t.busy(), 60, "B turn2") is None:
        raise RuntimeError("B: turn 2 never started")
    wait_idle(run, tui, "B turn2")
    run.snap(tui, "turn2_final")
    return "idle"


def wait_idle(run, tui, label):
    """TUI idle chrome held for 4s and no running turn in the runtime DB."""
    idle_since = None
    deadline = time.time() + run.args.timeout
    while time.time() < deadline:
        tui.pump(0.15)
        run.periodic(tui)
        if tui.dead:
            raise RuntimeError(f"{label}: TUI died")
        busy = tui.busy()
        if not run.sid:
            run.ensure_session(timeout=0.5)
        st = session_state(run.db, run.sid) if run.sid and run.db else None
        if not busy and st:
            busy = st.get("running", 0) > 0
        if busy:
            idle_since = None
        else:
            idle_since = idle_since or time.time()
            if time.time() - idle_since > 4.0:
                return time.time() - idle_since
    return None


def task_c(run, tui, db):
    prompt = TASKS["C"]
    run.note("submit", prompt_chars=len(prompt))
    submit(tui, prompt)
    if run.wait(tui, lambda t: t.busy(), 120, "C start") is None:
        raise RuntimeError("C: never started")
    run.note("running")
    idle = wait_idle(run, tui, "C")
    run.snap(tui, "final")
    if idle is None:
        raise RuntimeError("C: timeout")
    run.result["session"] = run.sid
    run.result["session_state"] = (
        session_state(run.db, run.sid) if run.db and run.sid else None
    )
    return "idle"


def task_d(run, tui, db):
    prompt = TASKS["D"]
    run.note("submit", prompt_chars=len(prompt))
    submit(tui, prompt)
    if run.wait(tui, lambda t: t.busy(), 120, "D streaming") is None:
        raise RuntimeError("D: never entered streaming")
    run.note("streaming")
    tui.pump(2.0)
    run.snap(tui, "streaming_before_cancel")
    tui.send("\x1b")
    run.note("cancel_sent")
    if run.wait(tui, lambda t: not t.busy(), 60, "D cancel") is None:
        raise RuntimeError("D: cancel did not settle")
    tui.pump(1.0)
    run.snap(tui, "after_cancel")
    run.result["cancel_text"] = tui.text()[-600:]
    submit(tui, "继续刚才的要求，把内容写完。")
    if run.wait(tui, lambda t: t.busy(), 60, "D continue") is None:
        raise RuntimeError("D: continue never started")
    idle = wait_idle(run, tui, "D continue")
    run.snap(tui, "continued_final")
    if idle is None:
        raise RuntimeError("D: continue timeout")
    first_sid = run.sid
    tui.send("\x03")
    tui.pump(0.4)
    tui.send("\x03")
    run.note("quit_requested")
    deadline = time.time() + 8
    while time.time() < deadline and not tui.dead:
        tui.pump(0.2)
    run.result["quit"] = "exited" if tui.dead else "still_running"
    run.result["session"] = first_sid
    return "idle"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--task", required=True, choices=sorted(TASKS))
    ap.add_argument("--bin", default="target/debug/leveler")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--model", default="deepseek/deepseek-v4-pro")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--timeout", type=float, default=1500)
    ap.add_argument("--out", required=True)
    ap.add_argument("--session", default=None)
    ap.add_argument("--resume-session", default=None)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    run = Run(args)
    repo = os.path.realpath(args.repo)
    db = db_find(repo)
    run.note("boot", repo=repo, db=db, binary=os.path.abspath(args.bin),
             model=args.model, size=[args.cols, args.rows],
             session=args.resume_session)
    before = sessions_in(db) if db else set()
    run.db, run.before = db, before
    extra = []
    if args.resume_session:
        extra = ["--session", args.resume_session]
    tui = Tui(os.path.abspath(args.bin), repo, args.model, args.cols, args.rows,
              extra, {}).start()
    try:
        run.wait_boot(tui)
        run.note("booted")
        run.snap(tui, "boot")
        if args.resume_session:
            # Resume: the replayed history must be on screen before the turn.
            tui.pump(1.0)
            run.result["resume_text"] = tui.text()[-1200:]
            submit(tui, "继续：请用一句话确认你已经看到了上面的历史。")
            if run.wait(tui, lambda t: t.busy(), 60, "D resume turn") is None:
                raise RuntimeError("resume: turn never started")
            wait_idle(run, tui, "D resume turn")
            run.snap(tui, "resume_final")
            # Exit gracefully so a resume -> quit -> resume cycle is
            # evidenced by terminal bytes, not only by the driver's kill.
            tui.send("\x03")
            tui.pump(0.4)
            tui.send("\x03")
            run.note("quit_requested")
            deadline = time.time() + 8
            while time.time() < deadline and not tui.dead:
                tui.pump(0.2)
            run.result["quit"] = "exited" if tui.dead else "still_running"
            run.result["resume_session"] = args.resume_session
            run.result["resume_state"] = (
                session_state(db, args.resume_session) if db else None
            )
            run.result["status"] = "idle"
            return 0
        fn = {"A": task_a, "B2": task_a, "B": task_b, "C": task_c, "D": task_d}[args.task]
        if args.task in ("B", "C", "D"):
            status = fn(run, tui, db)
        else:
            status = fn(run, tui)
        tui.pump(0.5)
        run.result["status"] = status
        run.result["session"] = run.sid
    except Exception as exc:  # noqa: BLE001 - the evidence records the failure
        run.result["status"] = "error"
        run.result["error"] = f"{type(exc).__name__}: {exc}"
        run.note("error", error=run.result["error"])
        try:
            run.snap(tui, "error")
        except Exception:
            pass
    finally:
        tui.close()
        run.result["raw_bytes"] = len(tui.raw)
        run.result["control_bytes"] = c0_counts(tui.raw)
        # Terminal lifecycle evidence: alt-screen enter/leave and cursor
        # visibility. They must be balanced whenever the TUI exits on its own.
        run.result["terminal_state"] = {
            "alt_enter": tui.raw.count(b"\x1b[?1049h"),
            "alt_leave": tui.raw.count(b"\x1b[?1049l"),
            "cursor_show": tui.raw.count(b"\x1b[?25h"),
            "cursor_hide": tui.raw.count(b"\x1b[?25l"),
        }
        run.result["screens"] = tui.screen.lines()[-40:]
        with open(os.path.join(args.out, "raw.bin"), "wb") as fh:
            fh.write(tui.raw)
        with open(os.path.join(args.out, "result.json"), "w") as fh:
            json.dump(run.result, fh, ensure_ascii=False, indent=2)
        run.note("done", status=run.result["status"],
                 raw_bytes=run.result["raw_bytes"],
                 control_bytes=run.result["control_bytes"])
        run.log.close()
    return 0 if run.result.get("status") == "idle" else 1


if __name__ == "__main__":
    sys.exit(main())
