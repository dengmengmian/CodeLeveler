"""Comparative adapter contracts for HC-001.

These tests lock the invocation and isolation rules. They do not run a
harness or talk to a model.
"""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = ROOT / "eval" / "comparative" / "runner.py"


def load_runner():
    spec = importlib.util.spec_from_file_location("comparative_runner", RUNNER_PATH)
    mod = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(mod)
    return mod


class ComparativeLaunchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = load_runner()

    def test_hc001_run_order_is_the_specified_rotation(self):
        self.assertEqual(
            self.r.HC001_ORDER,
            [
                ("leveler", 1),
                ("atomcode", 1),
                ("dsh", 1),
                ("dsh", 2),
                ("atomcode", 2),
                ("leveler", 2),
            ],
        )

    def test_evidence_path_includes_arm_and_rep(self):
        path = self.r.evidence_iso(Path("/e"), "leveler", "n3-caller-propagation", 2)
        self.assertEqual(path, Path("/e/leveler/n3-caller-propagation-r2"))

    def test_atomcode_real_default_omits_model_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            ws = Path(tmp) / "ws"
            ws.mkdir()
            iso = Path(tmp) / "iso"
            iso.mkdir()
            argv, env, cwd = self.r.launch(
                "atomcode",
                "deepseek/deepseek-v4-flash",
                ws,
                "the task",
                iso,
                atomcode_bin=Path("/bin/echo"),
            )
        self.assertNotIn("--model", argv)
        self.assertEqual(argv[0], "/bin/echo")
        for flag in ("-p", "-C", "-y", "-v", "--dev", "--no-telemetry"):
            self.assertIn(flag, argv)
        self.assertEqual(argv[argv.index("-p") + 1], "the task")
        self.assertEqual(Path(argv[argv.index("-C") + 1]), ws.resolve())
        self.assertTrue(Path(argv[argv.index("-C") + 1]).is_absolute())
        self.assertEqual(cwd, ws.resolve())
        self.assertNotIn("http_proxy", env)

    def test_relative_workspace_is_resolved_absolute(self):
        with tempfile.TemporaryDirectory() as tmp:
            ws = Path(tmp) / "ws"
            ws.mkdir()
            iso = Path(tmp) / "iso"
            iso.mkdir()
            rel_ws = Path(os.path.relpath(ws))
            argv, env, cwd = self.r.launch(
                "atomcode",
                "",
                rel_ws,
                "the task",
                iso,
                atomcode_bin=Path("/bin/echo"),
            )
        self.assertTrue(Path(argv[argv.index("-C") + 1]).is_absolute())
        self.assertEqual(Path(argv[argv.index("-C") + 1]), ws.resolve())
        self.assertTrue(cwd.is_absolute())

    def test_dsh_uses_isolated_home_and_matched_patch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "dsh"
            (root / "node_modules/tsx/dist/esm").mkdir(parents=True)
            (root / "node_modules/tsx/dist/esm/index.mjs").write_text("// shim\n")
            (root / "apps/cli/src").mkdir(parents=True)
            (root / "apps/cli/src/bin.ts").write_text("// bin\n")
            (root / "tsconfig.json").write_text("{}\n")
            ws = Path(tmp) / "ws"
            ws.mkdir()
            iso = Path(tmp) / "iso"
            argv, env, cwd = self.r.launch(
                "dsh",
                "deepseek/deepseek-v4-flash",
                ws,
                "the task",
                iso,
                dsh_root=root,
            )
            patch = Path(env["DSH_HOME"]) / "patch.yml"
            self.assertTrue(patch.is_file())
            text = patch.read_text()
        self.assertEqual(cwd, ws.resolve())
        self.assertTrue(cwd.is_absolute())
        self.assertEqual(Path(env["DSH_HOME"]), (iso / "dsh-home").resolve())
        self.assertEqual(env["DSH_PERMISSION_MODE"], "danger-full-access")
        self.assertEqual(Path(env["TSX_TSCONFIG_PATH"]), (root / "tsconfig.json").resolve())
        self.assertIn("baseURL: https://taotoken.net/api/v1", text)
        self.assertIn("apiKeyEnv: DEEPSEEK_API_KEY", text)
        self.assertIn("thinkingFormat: deepseek", text)
        self.assertIn("id: deepseek-v4-flash", text)
        self.assertNotIn("systemPrompt", text)
        self.assertNotIn("subagent", text.lower())
        self.assertIn("--profile", argv)
        self.assertIn("headless", argv)
        self.assertEqual(argv[-1], "the task")

    def test_leveler_uses_auto_approve_and_isolated_home(self):
        with tempfile.TemporaryDirectory() as tmp:
            cfg = Path(tmp) / "config.toml"
            cfg.write_text('default_model = "deepseek/deepseek-v4-flash"\n')
            ws = Path(tmp) / "ws"
            ws.mkdir()
            iso = Path(tmp) / "iso"
            bin_path = Path(tmp) / "leveler"
            bin_path.write_text("#!/bin/sh\n")
            argv, env, cwd = self.r.launch(
                "leveler",
                "deepseek/deepseek-v4-flash",
                ws,
                "the task",
                iso,
                leveler_bin=bin_path,
                leveler_config=cfg,
            )
            copied = iso / "leveler-home" / "config.toml"
            self.assertTrue(copied.is_file())
            self.assertEqual(
                copied.read_text(),
                'default_model = "deepseek/deepseek-v4-flash"\n',
            )
        self.assertEqual(argv[0], str(bin_path))
        self.assertEqual(argv[1], "run")
        self.assertEqual(argv[2], "the task")
        self.assertIn("--auto-approve", argv)
        self.assertIn("--repo", argv)
        self.assertTrue(Path(argv[argv.index("--repo") + 1]).is_absolute())
        self.assertEqual(Path(argv[argv.index("--repo") + 1]), ws.resolve())
        self.assertIn("--model", argv)
        self.assertEqual(argv[argv.index("--model") + 1], "deepseek/deepseek-v4-flash")
        self.assertNotIn("--permission", argv)
        self.assertEqual(Path(env["LEVELER_HOME"]), (iso / "leveler-home").resolve())
        self.assertEqual(cwd, ws.resolve())

    def test_redact_text_strips_key_material(self):
        raw = "Authorization: Bearer sk-abcdefghijklmnop\nDEEPSEEK_API_KEY=supersecretvalue\n"
        out = self.r.redact_text(raw, extra_secrets=["supersecretvalue"])
        self.assertNotIn("sk-abcdefghijklmnop", out)
        self.assertNotIn("supersecretvalue", out)
        self.assertIn("<REDACTED>", out)

    def test_claimed_completion_codeleveler_completed(self):
        text = "Modified files\n  a.go\n\nCompleted in 12 round(s).\n"
        claim = self.r.parse_claimed_completion("leveler", text)
        self.assertEqual(claim["claimed_done"], True)
        self.assertEqual(claim["stop_class"], "Completed")

    def test_claimed_completion_codeleveler_blocked_is_honest(self):
        text = "Stopped: the model reported the goal blocked after 8 round(s).\n"
        claim = self.r.parse_claimed_completion("leveler", text)
        self.assertEqual(claim["claimed_done"], False)
        self.assertEqual(claim["stop_class"], "Blocked")

    def test_dsh_patch_writer_rejects_eval_coaching(self):
        with tempfile.TemporaryDirectory() as tmp:
            patch = self.r.write_dsh_patch(Path(tmp), "deepseek-v4-flash")
            text = patch.read_text()
        lowered = text.lower()
        for banned in (
            "use multiple agents",
            "hidden test",
            "acceptance criteria",
            "eval-specific",
            "keep trying",
        ):
            self.assertNotIn(banned, lowered)

    def test_canonicalize_failure_is_infra_not_task_fail(self):
        label = self.r.classify_harness_exit(
            arm="leveler",
            rc=1,
            timed_out=False,
            output="error:\n  workspace error: failed to canonicalize workspace root foo/ws",
            expect_ok=False,
            wall_seconds=0.1,
        )
        self.assertEqual(label, "INFRA_FAILURE")

    def test_hc002_case_is_icg5_and_uses_the_same_rotation(self):
        self.assertEqual(self.r.HC002_CASE, "icg-5-long-task")
        self.assertEqual(self.r.HC002_ORDER, self.r.HC001_ORDER)
        self.assertEqual(self.r.HC002_TIMEOUT_SECONDS, 1800)

    def test_hc002_paid_runs_hold_on_obsolete_baseline(self):
        with self.assertRaises(SystemExit) as ctx:
            self.r.hc002_paid_gate(None)
        self.assertIn("HC002_PAID_RUNS=HOLD", str(ctx.exception))
        with self.assertRaises(SystemExit) as ctx:
            self.r.hc002_paid_gate("3b400357342cef4caa760628531ead3bd9eff333")
        self.assertIn("HC002_PAID_RUNS=HOLD", str(ctx.exception))

    def test_hc002_paid_gate_accepts_a_new_sha(self):
        sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        self.assertEqual(self.r.hc002_paid_gate(sha), sha)

    def test_launch_repo_and_workdir_are_absolute(self):
        """HC-001 adapter bug: relative --repo under cwd=ws failed canonicalize."""
        with tempfile.TemporaryDirectory() as tmp:
            ws = Path(tmp) / "ws"
            ws.mkdir()
            iso = Path(tmp) / "iso"
            iso.mkdir()
            cfg = Path(tmp) / "config.toml"
            cfg.write_text('default_model = "deepseek/deepseek-v4-flash"\n')
            bin_path = Path(tmp) / "leveler"
            bin_path.write_text("#!/bin/sh\n")
            argv, env, cwd = self.r.launch(
                "leveler",
                "deepseek/deepseek-v4-flash",
                Path(os.path.relpath(ws)),
                "the task",
                iso,
                leveler_bin=bin_path,
                leveler_config=cfg,
            )
        self.assertTrue(Path(argv[argv.index("--repo") + 1]).is_absolute())
        self.assertTrue(cwd.is_absolute())
        self.assertTrue(Path(env["LEVELER_HOME"]).is_absolute())


if __name__ == "__main__":
    unittest.main()
