"""AccessPortal: a deliberately minimal access-management web app.

Standard library only: no database, no external service, no network. The
server binds to loopback and is started by tests on an ephemeral port.

Routes in the initial fixture:
  GET /         -> redirect to /access
  GET /access   -> accounts and roles on one plain page
  GET /roles    -> roles and their permissions
  anything else -> 404 (including /admin: administrator management is not
                     implemented in this project yet)
"""

from __future__ import annotations

import html
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from store import ACCOUNTS, ROLES


def render_page(title: str, body: str) -> str:
    return f"""<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><title>{html.escape(title)}</title></head>
<body>
<h1>{html.escape(title)}</h1>
{body}
</body>
</html>
"""


def account_rows() -> str:
    return "".join(
        "<tr>"
        f"<td>{account['id']}</td>"
        f"<td>{html.escape(account['name'])}</td>"
        f"<td>{html.escape(account['email'])}</td>"
        f"<td>{html.escape(account['status'])}</td>"
        f"<td>{html.escape(account['role'])}</td>"
        "</tr>"
        for account in ACCOUNTS
    )


def role_rows() -> str:
    return "".join(
        "<tr>"
        f"<td>{html.escape(role)}</td>"
        f"<td>{html.escape(', '.join(permissions))}</td>"
        "</tr>"
        for role, permissions in ROLES.items()
    )


def render_access() -> str:
    return render_page(
        "Access",
        f"""<h2>账户</h2>
<table>
<thead><tr><th>ID</th><th>姓名</th><th>邮箱</th><th>状态</th><th>角色</th></tr></thead>
<tbody>{account_rows()}</tbody>
</table>
<h2>角色</h2>
<table>
<thead><tr><th>角色</th><th>权限</th></tr></thead>
<tbody>{role_rows()}</tbody>
</table>
""",
    )


def render_roles() -> str:
    return render_page(
        "Roles",
        f"""<table>
<thead><tr><th>角色</th><th>权限</th></tr></thead>
<tbody>{role_rows()}</tbody>
</table>
""",
    )


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # keep test output readable
        pass

    def _send(self, status: int, body: str, content_type: str = "text/html; charset=utf-8"):
        payload = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        parsed = urllib.parse.urlsplit(self.path)
        path = parsed.path.rstrip("/") or "/"
        if path == "/":
            self.send_response(302)
            self.send_header("Location", "/access")
            self.end_headers()
            return
        if path == "/access":
            self._send(200, render_access())
            return
        if path == "/roles":
            self._send(200, render_roles())
            return
        self._send(404, render_page("未找到", "<p>页面不存在。</p>"))


def make_server(host: str = "127.0.0.1", port: int = 0) -> ThreadingHTTPServer:
    return ThreadingHTTPServer((host, port), Handler)


def main() -> None:
    server = make_server(port=8000)
    print(f"AccessPortal listening on http://{server.server_address[0]}:{server.server_address[1]}")
    server.serve_forever()


if __name__ == "__main__":
    main()
