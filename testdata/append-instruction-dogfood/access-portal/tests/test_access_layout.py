"""Layout contract for the Access page.

These assertions describe structure a reader can see in the served HTML, not
CSS values: a stylesheet is linked and served, the page has navigation, and
the content sits inside a main region made of panels. The initial fixture is
plain HTML, so this module is red until the layout work is done.
"""

from __future__ import annotations

import threading
import unittest
import urllib.request

from app import make_server


class AccessLayoutTest(unittest.TestCase):
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

    def get(self, path: str) -> tuple[int, str, str]:
        with urllib.request.urlopen(self.base + path, timeout=5) as response:
            return (
                response.status,
                response.read().decode("utf-8"),
                response.headers.get("Content-Type", ""),
            )

    def test_access_page_links_a_stylesheet(self):
        _, body, _ = self.get("/access")
        self.assertIn('rel="stylesheet"', body)
        self.assertIn("/static/app.css", body)

    def test_stylesheet_is_served(self):
        status, body, content_type = self.get("/static/app.css")
        self.assertEqual(status, 200)
        self.assertIn("text/css", content_type)
        self.assertIn(".panel", body)

    def test_access_page_has_navigation(self):
        _, body, _ = self.get("/access")
        self.assertIn("<nav", body)
        self.assertIn('href="/access"', body)
        self.assertIn('href="/roles"', body)

    def test_access_content_lives_in_a_main_region_of_panels(self):
        _, body, _ = self.get("/access")
        self.assertIn("<main", body)
        self.assertIn("panel", body)


if __name__ == "__main__":
    unittest.main()
