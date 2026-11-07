"""A deliberately slow, real conformance probe.

It performs 30 real HTTP round trips with a 0.7 s pause between them (about
21 s of wall clock) and writes ``.slow_started`` when the first probe begins,
so a person watching a run can tell the silent window is real work rather than
a hang.
"""

from __future__ import annotations

import os
import threading
import time
import unittest
import urllib.request

from app import make_server

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MARKER = os.path.join(REPO_ROOT, ".slow_started")


class SlowConformanceProbe(unittest.TestCase):
    def test_permission_pages_stay_consistent(self):
        with open(MARKER, "w", encoding="utf-8") as handle:
            handle.write(str(time.time()))

        server = make_server(port=0)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = "http://127.0.0.1:%d" % server.server_address[1]
        try:
            for index in range(30):
                time.sleep(0.7)
                with urllib.request.urlopen(base + "/accounts", timeout=5) as response:
                    self.assertEqual(response.status, 200)
                    self.assertIn("Alice Chen", response.read().decode("utf-8"))
                with urllib.request.urlopen(base + "/roles", timeout=5) as response:
                    self.assertEqual(response.status, 200)
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
