#!/usr/bin/env python3
"""Pair a simulator with the TUI the way a person would: `/xremote-loc`.

`simulator_pairing.sh` drives the same chain through the `leveler remote …`
subcommands, which is the operator's path. This one drives the path a user
actually has — type `/remote` in the terminal UI, a QR code appears, the phone
scans it, and the terminal asks whether to let that phone in.

It runs the TUI under a pty because that is the only way to reach the real
screen: the QR, the payload line under it, the fingerprint the user compares,
and the y/n prompt are all rendered, not printed. `loc` is the local-testing
form — it starts a relay on this machine so nothing leaves the LAN.

    python3 tui_remote_pairing.py <simulator-udid>
"""

import os
import pty
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
LEVELER = os.path.join(REPO, "target", "debug", "leveler")

# Wide enough that the payload line under the QR is not clipped: the app needs
# every character of it, and a Paragraph clips rather than wraps.
COLUMNS, ROWS = 300, 80

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][A-Z0-9]|\x1b[=>]|\r")
PAYLOAD = re.compile(r'\{"runtime_id":.*?"pairing_secret":"[^"]+"\}')


class Tui:
    """The terminal UI, running under a pty, with everything it has drawn."""

    def __init__(self, env, repo):
        self.master, slave = pty.openpty()
        # Tell it how big the screen is, or ratatui assumes 80x24 and clips the
        # payload line in half.
        import fcntl
        import struct
        import termios

        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
        self.process = subprocess.Popen(
            [LEVELER, "--repo", repo, "tui"],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            close_fds=True,
        )
        os.close(slave)
        self.seen = ""

    def read(self, seconds=0.4):
        """Drain whatever has been drawn, stripped of escape codes."""
        import select

        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.1)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                break
            if not chunk:
                break
            self.seen += ANSI.sub("", chunk.decode("utf-8", "replace"))
        return self.seen

    def type(self, text):
        os.write(self.master, text.encode())

    def wait_for(self, pattern, seconds, what):
        deadline = time.time() + seconds
        while time.time() < deadline:
            self.read(0.5)
            found = re.search(pattern, self.seen)
            if found:
                return found
            if self.process.poll() is not None:
                raise SystemExit(f"TUI 退出了（等 {what} 时）：\n{self.tail()}")
        raise SystemExit(f"等了 {seconds} 秒也没等到 {what}：\n{self.tail()}")

    def tail(self, lines=40):
        return "\n".join(self.seen.splitlines()[-lines:])

    def stop(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGKILL)
        os.close(self.master)


def main():
    if len(sys.argv) < 2:
        raise SystemExit("用法: tui_remote_pairing.py <simulator-udid>")
    device = sys.argv[1]
    if not os.access(LEVELER, os.X_OK):
        raise SystemExit(f"缺少 {LEVELER}，先跑 cargo build -p leveler-cli")

    work = tempfile.mkdtemp()
    home = os.path.join(work, ".leveler")
    os.makedirs(home)
    scratch = os.path.join(work, "scratch-repo")
    os.makedirs(scratch)
    subprocess.run(["git", "init", "-q", "."], cwd=scratch, check=True)
    with open(os.path.join(scratch, "scratch.txt"), "w") as handle:
        handle.write("临时文件\n")
    subprocess.run(["git", "add", "-A"], cwd=scratch, check=True)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "scratch"],
        cwd=scratch,
        check=True,
    )

    provider = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "scripted_provider.py"), "18500"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    with open(os.path.join(home, "config.toml"), "w") as handle:
        handle.write(
            'default_model = "scripted"\n'
            'lang = "zh"\n\n'
            "[providers.scripted]\n"
            'base_url = "http://127.0.0.1:18500"\n'
            'api_key = "not-used"\n\n'
            "[models.scripted]\n"
            'provider = "scripted"\n'
            'model_id = "scripted"\n'
            "context_window = 100000\n"
            "max_output_tokens = 4096\n"
            "streaming = true\n"
            "tool_calling = true\n"
        )

    env = dict(os.environ, LEVELER_HOME=home, TERM="xterm-256color", COLUMNS=str(COLUMNS), LINES=str(ROWS))
    tui = Tui(env, scratch)
    flutter = None
    try:
        tui.read(3.0)
        print("== 在 TUI 里输入 /xremote-loc ==", flush=True)
        tui.type("/xremote-loc\r")

        # The QR itself: half-block rows. Its presence is the point of the
        # command — a payload with no code to scan is the old flow with extra
        # steps.
        tui.wait_for(r"用手机扫这个码", 60, "二维码界面")
        screen = tui.read(1.0)
        # Counted as glyphs, not rows: ratatui moves the cursor instead of
        # writing runs of blanks, so a pty transcript has no reliable lines.
        blocks = sum(screen.count(glyph) for glyph in "█▀▄")
        if blocks < 400:
            raise SystemExit(f"没有画出二维码（只有 {blocks} 个方块字符）：\n{tui.tail()}")
        print(f"== 二维码画出来了（{blocks} 个方块字符）==", flush=True)

        payload = PAYLOAD.search(screen)
        if not payload:
            raise SystemExit(f"屏幕上没有可粘贴的载荷：\n{tui.tail()}")
        payload = payload.group(0)
        print(f"== 载荷 ==\n{payload}", flush=True)
        if "127.0.0.1" in payload:
            print("!! relay 地址是回环地址，真手机会连不上（模拟器可以）", flush=True)

        # Derived from the key in the payload rather than scraped off the
        # screen: this is the number the user compares, so checking that the
        # terminal shows *this* value is the point, and a value read back off
        # the same terminal would check nothing.
        import base64
        import hashlib
        import json

        key = json.loads(payload)["runtime_pubkey"]
        raw = base64.urlsafe_b64decode(key + "=" * (-len(key) % 4))
        digest = hashlib.sha256(raw).hexdigest()[:16]
        fingerprint = " ".join(digest[i:i + 4] for i in range(0, 16, 4))
        shown = re.search(r"本机指纹：([0-9a-f ]{16,19})", screen)
        if not shown or shown.group(1).replace(" ", "") != digest:
            raise SystemExit(
                f"终端显示的指纹和载荷里的密钥对不上（应为 {fingerprint}）：\n{tui.tail()}"
            )
        print(f"== 本机指纹 {fingerprint} ==", flush=True)

        print("== 让模拟器上的 app 扫这个载荷 ==", flush=True)
        flutter = subprocess.Popen(
            [
                shutil.which("flutter") or "/opt/homebrew/bin/flutter",
                "test",
                "integration_test/pairing_flow_test.dart",
                "-d",
                device,
                f"--dart-define=PAIRING_PAYLOAD={payload}",
                f"--dart-define=HOST_FINGERPRINT={fingerprint}",
            ],
            cwd=os.path.join(REPO, "apps", "leveler-mobile"),
        )

        # The phone claims the secret and then waits for a person. This is the
        # half of the design that cannot be automated away: the terminal must
        # ask, and only a keystroke may answer.
        tui.wait_for(r"想要连接", 180, "手机提交配对")
        asked = tui.read(1.0)
        if "y 接受" not in asked:
            raise SystemExit(f"没有提示怎么接受：\n{tui.tail()}")
        phone = re.search(r"手机指纹：([0-9a-f ]{16,})", asked)
        print(f"== 手机来了，指纹 {phone.group(1).strip() if phone else '?'} ==", flush=True)
        # Hold comfortably longer than the app spends checking that it is
        # *not* yet paired (four ~2s pumps). Accepting inside that window makes
        # the phone look like it promoted itself, which is the one thing that
        # test is watching for.
        time.sleep(15)
        tui.type("y")

        tui.wait_for(r"已配对|配对完成|已接受", 60, "确认结果")
        print("== 已在 TUI 里接受 ==", flush=True)

        code = flutter.wait()
        flutter = None
        if code != 0:
            raise SystemExit(f"app 端集成测试失败（{code}）")
        if os.path.exists(os.path.join(scratch, "scratch.txt")):
            raise SystemExit("批准之后 scratch.txt 还在：审批没有真正执行")
        print("== 全通过：/xremote-loc → 扫码 → 接受 → 对话 → 审批 ==", flush=True)
    finally:
        if flutter and flutter.poll() is None:
            flutter.kill()
        tui.stop()
        provider.kill()


if __name__ == "__main__":
    main()
