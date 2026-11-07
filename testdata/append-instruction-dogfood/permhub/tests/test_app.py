"""Behavior tests for PermHub.

Run from the repository root:

    python3 -m unittest discover -s tests -v

The email-search test is red in the initial code: ``store.search_accounts``
only matches the display name. The slow probe in test_00_slow.py runs first so
the suite also exercises a long-running check.
"""

from __future__ import annotations

import threading
import unittest
import urllib.request

from app import make_server


class PermHubTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = make_server(port=0)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = "http://127.0.0.1:%d" % cls.server.server_address[1]

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def get(self, path: str) -> tuple[int, str]:
        with urllib.request.urlopen(self.base + path, timeout=5) as response:
            return response.status, response.read().decode("utf-8")

    def test_accounts_lists_every_account(self):
        status, body = self.get("/accounts")
        self.assertEqual(status, 200)
        for name in ("Alice Chen", "Bob Li", "Carol Wang"):
            self.assertIn(name, body)

    def test_roles_show_permissions(self):
        status, body = self.get("/roles")
        self.assertEqual(status, 200)
        self.assertIn("role.write", body)
        self.assertIn("account.read", body)

    def test_search_by_name(self):
        _, body = self.get("/accounts?q=Alice")
        self.assertIn("Alice Chen", body)
        self.assertNotIn("Bob Li", body)

    def test_search_by_email(self):
        _, body = self.get("/accounts?q=bob%40example.com")
        self.assertIn("Bob Li", body)

    def test_unknown_page_is_404(self):
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.get("/admin")
        self.assertEqual(caught.exception.code, 404)


if __name__ == "__main__":
    unittest.main()
