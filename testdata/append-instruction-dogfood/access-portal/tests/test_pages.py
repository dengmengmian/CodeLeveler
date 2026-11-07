"""Behavior tests for AccessPortal.

Run from the repository root:

    python3 -m unittest discover -s tests -v
"""

from __future__ import annotations

import threading
import unittest
import urllib.request

from app import make_server


class AccessPortalTest(unittest.TestCase):
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

    def test_access_lists_every_account(self):
        status, body = self.get("/access")
        self.assertEqual(status, 200)
        for name in ("Alice Chen", "Bob Li", "Carol Wang"):
            self.assertIn(name, body)

    def test_roles_show_permissions(self):
        status, body = self.get("/roles")
        self.assertEqual(status, 200)
        self.assertIn("role.write", body)
        self.assertIn("account.read", body)


if __name__ == "__main__":
    unittest.main()
