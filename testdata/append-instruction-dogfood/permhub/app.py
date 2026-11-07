"""PermHub: a deliberately minimal permission-management web app.

Standard library only: no database, no external service, no network. The
server binds to loopback and is started by tests on an ephemeral port.

Routes in the initial fixture:
  GET /            -> redirect to /accounts
  GET /accounts    -> account list, optional ?q= search
  GET /roles       -> roles and their permissions
  anything else    -> 404 (including /admin, which this project does not
                      implement yet)
"""

from __future__ import annotations

import html
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from store import ACCOUNTS, ROLES, search_accounts


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


def render_accounts(query: str = "") -> str:
    rows = "".join(
        "<tr>"
        f"<td>{account['id']}</td>"
        f"<td>{html.escape(account['name'])}</td>"
        f"<td>{html.escape(account['email'])}</td>"
        f"<td>{html.escape(account['status'])}</td>"
        f"<td>{html.escape(account['role'])}</td>"
        "</tr>"
        for account in search_accounts(query)
    )
    return render_page(
        "账户管理",
        f"""
<form method="get" action="/accounts">
  <input name="q" value="{html.escape(query)}" placeholder="搜索账户">
  <button type="submit">搜索</button>
</form>
<table>
<thead><tr><th>ID</th><th>姓名</th><th>邮箱</th><th>状态</th><th>角色</th></tr></thead>
<tbody>{rows}</tbody>
</table>
""",
    )


def render_roles() -> str:
    rows = "".join(
        "<tr>"
        f"<td>{html.escape(role)}</td>"
        f"<td>{html.escape(', '.join(permissions))}</td>"
        "</tr>"
        for role, permissions in ROLES.items()
    )
    return render_page(
        "角色与权限",
        f"<table><thead><tr><th>角色</th><th>权限</th></tr></thead><tbody>{rows}</tbody></table>",
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
            self.send_header("Location", "/accounts")
            self.end_headers()
            return
        if path == "/accounts":
            query = urllib.parse.parse_qs(parsed.query).get("q", [""])[0]
            self._send(200, render_accounts(query))
            return
        if path == "/roles":
            self._send(200, render_roles())
            return
        self._send(404, render_page("未找到", "<p>页面不存在。</p>"))


def make_server(host: str = "127.0.0.1", port: int = 0) -> ThreadingHTTPServer:
    return ThreadingHTTPServer((host, port), Handler)


def main() -> None:
    server = make_server(port=8000)
    print(f"PermHub listening on http://{server.server_address[0]}:{server.server_address[1]}")
    server.serve_forever()


if __name__ == "__main__":
    main()
