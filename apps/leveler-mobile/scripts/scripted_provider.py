#!/usr/bin/env python3
"""An OpenAI-compatible endpoint that answers from a fixed script.

The acceptance run drives the phone through a whole turn: a question in
Chinese, one in English, and a command the user has to approve. A real model
would answer all three, but not the same way twice — and a test that cannot
say what should appear on screen cannot tell a rendering bug from a model
having a different idea. So the model is the one thing held still here.
Everything below it — runtime, agent, relay, socket, app — is real.

Replies are chosen by looking at the conversation the runtime sends, not by
counting requests: a turn that calls a tool comes back for a second completion
with the tool result appended, and position alone cannot tell those apart.

    python3 scripted_provider.py 18500
"""

import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Markdown on purpose: the phone renders assistant text as markdown, and a
# heading that arrives as a literal '## ' is a bug the plain-text answer hides.
CHINESE_ANSWER = (
    "## 中文回答\n\n"
    "这是一段**中文**输出，用来验证渲染：\n\n"
    "- 列表第一项\n"
    "- 列表第二项\n\n"
    "```rust\nfn main() { println!(\"你好\"); }\n```\n"
)

ENGLISH_ANSWER = (
    "## English answer\n\n"
    "Plain **English** output with a list:\n\n"
    "- first item\n"
    "- second item\n"
)

APPROVAL_ARGS = json.dumps(
    {"program": "rm", "args": ["scratch.txt"], "reason": "清理临时文件"}
)

DONE_AFTER_TOOL = "命令已执行完毕，任务完成。"

# A reply slow enough to be interrupted. Cancelling a turn cannot be tested
# against an answer that arrives before the finger leaves the screen, so one
# prompt asks for a long one and gets it delivered a piece at a time.
SLOW_ANSWER = "这是一段很长的回答，" + "".join(f"第{n}句。" for n in range(1, 40))


def last_user_text(body):
    """The newest thing the person actually typed.

    Skips the runtime's own synthetic user turns — the "you stopped without
    calling update_goal" nudges — because those are the newest user messages on
    the wire and would otherwise decide the reply instead of the question.
    """
    for message in reversed(body.get("messages", [])):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, list):
            content = " ".join(
                part.get("text", "") for part in content if isinstance(part, dict)
            )
        if not isinstance(content, str):
            continue
        if "update_goal" in content:
            continue
        return content
    return ""


def has_tool_result(body):
    """Whether the runtime is coming back with a tool's output in hand."""
    return any(m.get("role") == "tool" for m in body.get("messages", []))


def text_chunks(text):
    """Split into deltas, so the phone is streamed to rather than handed a blob."""
    return re.findall(r".{1,24}", text, flags=re.S)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass  # The script's own log is the interesting one.

    def do_GET(self):  # noqa: N802 - BaseHTTPRequestHandler's spelling
        # `/models` keeps `leveler doctor` and any probe from failing the run.
        payload = json.dumps({"data": [{"id": "scripted"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        prompt = last_user_text(body)
        if has_tool_result(body):
            print(f"[provider] 工具结果回来了 -> 收尾", flush=True)
            self.stream_text(DONE_AFTER_TOOL)
        elif "慢慢" in prompt:
            print(f"[provider] {prompt[:40]!r} -> 慢回答（可被取消）", flush=True)
            self.stream_text(SLOW_ANSWER, delay=0.6)
        elif "删除" in prompt or "delete" in prompt.lower():
            print(f"[provider] {prompt[:40]!r} -> 请求审批", flush=True)
            self.stream_tool_call()
        elif re.search(r"[一-鿿]", prompt):
            print(f"[provider] {prompt[:40]!r} -> 中文回答", flush=True)
            self.stream_text(CHINESE_ANSWER)
        else:
            print(f"[provider] {prompt[:40]!r} -> English answer", flush=True)
            self.stream_text(ENGLISH_ANSWER)

    def send_event(self, payload):
        self.wfile.write(b"data: " + json.dumps(payload).encode() + b"\n\n")
        self.wfile.flush()

    def stream_text(self, text, delay=0.0):
        for chunk in text_chunks(text):
            try:
                self.send_event({"choices": [{"index": 0, "delta": {"content": chunk}}]})
            except (BrokenPipeError, ConnectionResetError):
                # A cancelled turn closes the connection under us. That is the
                # success case for the cancel test, not an error worth a
                # traceback in the middle of the run's output.
                print("[provider] 对端断开（大概率是取消）", flush=True)
                return
            if delay:
                time.sleep(delay)
        self.finish_stream("stop")

    def stream_tool_call(self):
        self.send_event(
            {
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_scripted_1",
                                    "type": "function",
                                    "function": {
                                        "name": "run_command",
                                        "arguments": APPROVAL_ARGS,
                                    },
                                }
                            ]
                        },
                    }
                ]
            }
        )
        self.finish_stream("tool_calls")

    def finish_stream(self, reason):
        self.send_event({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18500
    print(f"[provider] listening on 127.0.0.1:{port}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
