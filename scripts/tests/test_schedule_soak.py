import datetime as dt
import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "schedule_soak.py"
spec = importlib.util.spec_from_file_location("schedule_soak", SCRIPT)
soak = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules[spec.name] = soak
spec.loader.exec_module(soak)


class TimeTests(unittest.TestCase):
    def test_iso_round_trip(self):
        value = dt.datetime(2026, 11, 6, 1, 2, 3, tzinfo=dt.timezone.utc)
        self.assertEqual(soak.parse_iso(soak.iso(value)), value)

    def test_version_revision_reads_clean_and_dirty_suffixes(self):
        self.assertEqual(soak.version_revision("1.0.23 (a421315cc4ad)"), "a421315cc4ad")
        self.assertEqual(soak.version_revision("1.0.23 (a421315cc4ad-dirty)"), "a421315cc4ad")
        self.assertIsNone(soak.version_revision("1.0.23"))


class IsolationTests(unittest.TestCase):
    def test_real_home_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaises(soak.SoakError):
                soak.assert_isolated_home(Path.home() / ".leveler", Path(root))

    def test_home_outside_root_is_rejected(self):
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as other:
            with self.assertRaises(soak.SoakError):
                soak.assert_isolated_home(Path(other) / "home", Path(root))

    def test_tmp_home_is_accepted(self):
        root = Path(tempfile.mkdtemp(dir="/tmp", prefix="lvs-test-"))
        try:
            home = root / "homes" / "os"
            home.mkdir(parents=True)
            soak.assert_isolated_home(home, root)
        finally:
            import shutil

            shutil.rmtree(root, ignore_errors=True)


class StatisticsTests(unittest.TestCase):
    def test_percentile_uses_nearest_rank(self):
        values = [1, 2, 3, 4, 5]
        self.assertEqual(soak.percentile(values, 0.5), 3)
        self.assertEqual(soak.percentile(values, 0.95), 5)
        self.assertIsNone(soak.percentile([], 0.5))

    def test_latency_stats_empty(self):
        self.assertEqual(soak.latency_stats([]), {"count": 0, "p50": None, "p95": None, "p99": None, "max": None})


class TriggerTests(unittest.TestCase):
    def test_every_slots_include_the_anchor(self):
        start = dt.datetime(2026, 11, 6, tzinfo=dt.timezone.utc)
        deadline = start + dt.timedelta(seconds=180)
        spec = soak.DefSpec("t", "name", "shell", ["--every", "60"])
        self.assertEqual(soak._expected_slots(spec, start, deadline), 4)

    def test_once_is_one_slot(self):
        start = dt.datetime(2026, 11, 6, tzinfo=dt.timezone.utc)
        spec = soak.DefSpec("o", "name", "shell", ["--at", soak.iso(start)])
        self.assertEqual(soak._expected_slots(spec, start, start), 1)

    def test_cron_is_not_estimated(self):
        start = dt.datetime(2026, 11, 6, tzinfo=dt.timezone.utc)
        spec = soak.DefSpec("c", "name", "shell", ["--cron", "*/5 * * * *"])
        self.assertIsNone(soak._expected_slots(spec, start, start))


class S4Tests(unittest.TestCase):
    def test_allow_execution_is_a_gating_check(self):
        self.assertIn("S4_allow_execution_1", soak.required_checks("dry-run"))
        required = soak.required_checks("soak")
        for suffix in ("1", "2", "3"):
            self.assertIn("S4_allow_execution_%s" % suffix, required)

    def test_the_allow_lane_runs_one_fresh_session_per_run(self):
        for mode in ("dry-run", "soak"):
            lanes = soak.lane_specs(mode, "deepseek/test")
            allow = next(spec for spec in lanes["overlap"] if spec.key == "allow")
            self.assertEqual(allow.overlap, "allow-concurrent", mode)
            self.assertEqual(allow.session_policy, "fresh", mode)

    def test_a_shell_definition_carries_its_session_policy(self):
        spec = soak.DefSpec(
            "allow",
            "n",
            "shell",
            ["--every", "60"],
            command="true",
            overlap="allow-concurrent",
            session_policy="fresh",
        )
        args = soak._add_args({"repo": "/tmp/workspace"}, spec)
        self.assertIn("--session-policy", args)
        self.assertEqual(args[args.index("--session-policy") + 1], "fresh")


class EffectAccountingTests(unittest.TestCase):
    def test_a_spec_key_resolves_to_the_created_definition_id(self):
        defs = {"queue": {"id": "def-1", "name": "soak-overlap-queue", "spec": {}}}
        self.assertEqual(soak.definition_id_for(defs, "queue"), "def-1")
        self.assertEqual(soak.effect_success_count({"def-1": 3}, defs, "queue"), 3)

    def test_a_legacy_string_mapping_still_resolves(self):
        self.assertEqual(soak.definition_id_for({"queue": "def-1"}, "queue"), "def-1")
        self.assertEqual(soak.effect_success_count({"def-1": 2}, {"queue": "def-1"}, "queue"), 2)

    def test_a_missing_definition_is_zero_successes(self):
        self.assertIsNone(soak.definition_id_for({}, "queue"))
        self.assertEqual(soak.effect_success_count({"def-1": 2}, {}, "queue"), 0)


class NaturalOverlapTests(unittest.TestCase):
    def _snapshot(self, runs, occurrences):
        return {"definitions": [], "runs": runs, "occurrences": occurrences}

    def _run(self, run_id, state, started, finished=None):
        return {
            "id": run_id,
            "definition_id": "def",
            "state": state,
            "started_at": started,
            "finished_at": finished,
        }

    def test_a_skipped_slot_due_inside_an_active_run_is_observed(self):
        runs = [self._run("a", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:02:00Z")]
        occurrences = [
            {
                "id": "occ",
                "definition_id": "def",
                "scheduled_for": "2026-11-06T00:01:00Z",
                "state": "skipped",
                "runs": [self._run("s", "skipped", "2026-11-06T00:02:05Z", "2026-11-06T00:02:05Z")],
            }
        ]
        found = soak.natural_overlap_observation(
            self._snapshot(runs, occurrences), {"skip": "def"}, soak.parse_iso("2026-11-06T00:00:30Z")
        )["skip"]
        self.assertFalse(found.get("missing"), found)
        self.assertTrue(found["terminal"])
        self.assertTrue(found["policy_ok"])

    def test_a_queue_run_starts_after_the_active_run_finishes(self):
        runs = [self._run("a", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:02:00Z")]
        occurrences = [
            {
                "id": "occ",
                "definition_id": "def",
                "scheduled_for": "2026-11-06T00:01:00Z",
                "state": "completed",
                "runs": [
                    self._run("q", "succeeded", "2026-11-06T00:02:01Z", "2026-11-06T00:03:00Z")
                ],
            }
        ]
        found = soak.natural_overlap_observation(
            self._snapshot(runs, occurrences), {"queue": "def"}, soak.parse_iso("2026-11-06T00:00:30Z")
        )["queue"]
        self.assertTrue(found["policy_ok"], found)
        self.assertTrue(found["terminal"])

    def test_a_queue_run_that_started_early_is_not_queue_one(self):
        runs = [self._run("a", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:02:00Z")]
        occurrences = [
            {
                "id": "occ",
                "definition_id": "def",
                "scheduled_for": "2026-11-06T00:01:00Z",
                "state": "completed",
                "runs": [
                    self._run("q", "succeeded", "2026-11-06T00:01:30Z", "2026-11-06T00:02:30Z")
                ],
            }
        ]
        found = soak.natural_overlap_observation(
            self._snapshot(runs, occurrences), {"queue": "def"}, soak.parse_iso("2026-11-06T00:00:30Z")
        )["queue"]
        self.assertFalse(found["policy_ok"], found)

    def test_a_second_allow_run_started_while_the_first_is_active(self):
        runs = [
            self._run("a", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:02:00Z"),
            self._run("b", "succeeded", "2026-11-06T00:01:00Z", "2026-11-06T00:03:00Z"),
        ]
        found = soak.natural_overlap_observation(
            self._snapshot(runs, []), {"allow": "def"}, soak.parse_iso("2026-11-06T00:00:30Z")
        )["allow"]
        self.assertTrue(found["policy_ok"], found)
        self.assertTrue(found["terminal"])

    def test_a_second_allow_run_after_the_first_finished_is_not_overlap(self):
        runs = [
            self._run("a", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:02:00Z"),
            self._run("b", "succeeded", "2026-11-06T00:02:30Z", "2026-11-06T00:03:30Z"),
        ]
        found = soak.natural_overlap_observation(
            self._snapshot(runs, []), {"allow": "def"}, soak.parse_iso("2026-11-06T00:00:30Z")
        )["allow"]
        self.assertFalse(found["policy_ok"], found)


class FaultRecoveryTests(unittest.TestCase):
    def _snapshot(self, runs, occurrences=None):
        return {"definitions": [], "runs": runs, "occurrences": occurrences or []}

    def _run(self, run_id, state, started, finished=None, occurrence="o"):
        return {
            "id": run_id,
            "definition_id": "fault-def",
            "occurrence_id": occurrence,
            "state": state,
            "started_at": started,
            "finished_at": finished,
        }

    def test_a_dispatcher_kill_that_recovers_the_outcome_is_a_pass(self):
        runs = [
            self._run("inflight", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:05:00Z"),
            self._run("later", "succeeded", "2026-11-06T00:06:00Z", "2026-11-06T00:06:30Z", "o2"),
        ]
        facts = soak.fault_recovery_facts(
            self._snapshot(runs),
            "fault-def",
            soak.parse_iso("2026-11-06T00:00:30Z"),
            ["inflight"],
        )
        self.assertEqual(facts["unknown"], [])
        self.assertEqual(facts["recovered"], ["inflight"])
        self.assertTrue(facts["passed"], facts)

    def test_a_run_that_never_settles_fails_the_recovery(self):
        runs = [self._run("inflight", "running", "2026-11-06T00:00:00Z")]
        facts = soak.fault_recovery_facts(
            self._snapshot(runs),
            "fault-def",
            soak.parse_iso("2026-11-06T00:00:30Z"),
            ["inflight"],
        )
        self.assertEqual(facts["stuck"], ["inflight"])
        self.assertFalse(facts["passed"], facts)

    def test_a_duplicate_occurrence_run_fails_the_recovery(self):
        runs = [
            self._run("inflight", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:05:00Z"),
            self._run("again", "succeeded", "2026-11-06T00:00:40Z", "2026-11-06T00:01:00Z"),
            self._run("later", "succeeded", "2026-11-06T00:06:00Z", "2026-11-06T00:06:30Z", "o2"),
        ]
        facts = soak.fault_recovery_facts(
            self._snapshot(runs),
            "fault-def",
            soak.parse_iso("2026-11-06T00:00:30Z"),
            ["inflight"],
        )
        self.assertEqual(facts["duplicate_occurrences"], ["o"])
        self.assertFalse(facts["passed"], facts)

    def test_no_later_success_means_dispatch_did_not_resume(self):
        runs = [self._run("inflight", "succeeded", "2026-11-06T00:00:00Z", "2026-11-06T00:05:00Z")]
        facts = soak.fault_recovery_facts(
            self._snapshot(runs),
            "fault-def",
            soak.parse_iso("2026-11-06T00:00:30Z"),
            ["inflight"],
        )
        self.assertEqual(facts["later_success"], 0)
        self.assertFalse(facts["passed"], facts)


class MisfireTests(unittest.TestCase):
    def _snapshot(self):
        def run(run_id, state, started, finished=None):
            return {
                "id": run_id,
                "definition_id": "runonce-def",
                "state": state,
                "started_at": started,
                "finished_at": finished,
            }

        return {
            "definitions": [],
            "runs": [
                run("recovered", "succeeded", "2026-11-06T00:02:50Z", "2026-11-06T00:03:55Z"),
                run("next", "running", "2026-11-06T00:03:55Z"),
            ],
            "occurrences": [
                {
                    "id": "offline",
                    "definition_id": "runonce-def",
                    "scheduled_for": "2026-11-06T00:00:53Z",
                    "state": "completed",
                    "runs": [
                        run("recovered", "succeeded", "2026-11-06T00:02:50Z", "2026-11-06T00:03:55Z")
                    ],
                },
                {
                    "id": "after-on",
                    "definition_id": "runonce-def",
                    "scheduled_for": "2026-11-06T00:03:53Z",
                    "state": "claimed",
                    "runs": [run("next", "running", "2026-11-06T00:03:55Z")],
                },
                {
                    "id": "skip-offline",
                    "definition_id": "skip-def",
                    "scheduled_for": "2026-11-06T00:00:53Z",
                    "state": "missed",
                    "runs": [],
                },
            ],
        }

    def test_only_the_offline_window_recovery_run_counts(self):
        facts = soak.misfire_facts(
            self._snapshot(),
            {"skip": "skip-def", "runonce": "runonce-def"},
            soak.parse_iso("2026-11-06T00:00:50Z"),
            soak.parse_iso("2026-11-06T00:02:50Z"),
        )
        self.assertEqual(facts["skip_missed"], 1)
        self.assertEqual(len(facts["recovered"]), 1)
        self.assertEqual(facts["recovered"][0]["id"], "recovered")
        self.assertTrue(facts["passed"], facts)

    def test_an_offline_run_started_before_on_is_not_a_recovery(self):
        snapshot = self._snapshot()
        snapshot["occurrences"][0]["runs"][0]["started_at"] = "2026-11-06T00:00:10Z"
        facts = soak.misfire_facts(
            snapshot,
            {"skip": "skip-def", "runonce": "runonce-def"},
            soak.parse_iso("2026-11-06T00:00:50Z"),
            soak.parse_iso("2026-11-06T00:02:50Z"),
        )
        self.assertEqual(facts["recovered"], [])
        self.assertFalse(facts["passed"], facts)


class RecoveryTests(unittest.TestCase):
    def test_only_lanes_with_active_runs_are_reconciled(self):
        rows = [
            {"lane": "fault", "run": "a"},
            {"lane": "os", "run": "b"},
            {"lane": "fault", "run": "c"},
        ]
        self.assertEqual(soak.lanes_with_active_runs(rows), ["fault", "os"])
        self.assertEqual(soak.lanes_with_active_runs([]), [])


class ResourceTests(unittest.TestCase):
    def test_a_process_sample_feeds_the_resource_summary(self):
        import shutil

        root = Path(tempfile.mkdtemp(dir="/tmp", prefix="lvs-res-test-"))
        try:
            evidence = root / "evidence"
            evidence.mkdir()
            paths = soak.RunPaths.for_evidence(evidence, root)
            recorder = soak.Recorder(paths)
            sample = soak.sample_processes(paths, {}, recorder)
            summary = soak.resource_summary([sample])
            self.assertEqual(summary["cpu_pct"]["count"], 1)
            self.assertEqual(summary["db_bytes_growth"], 0.0)
        finally:
            shutil.rmtree(root, ignore_errors=True)

    def test_a_sample_names_the_rss_of_each_kind_of_process(self):
        import shutil
        from unittest import mock

        root = Path(tempfile.mkdtemp(dir="/tmp", prefix="lvs-res-test-"))
        try:
            evidence = root / "evidence"
            evidence.mkdir()
            paths = soak.RunPaths.for_evidence(evidence, root)
            recorder = soak.Recorder(paths)
            rows = [
                {
                    "pid": 11,
                    "ppid": 1,
                    "rss_kb": 100,
                    "cpu": 1.0,
                    "command": str(root) + "/bin/leveler schedule serve --poll-secs 5",
                },
                {
                    "pid": 22,
                    "ppid": 11,
                    "rss_kb": 200,
                    "cpu": 2.0,
                    "command": str(root) + "/bin/leveler serve --ready-json",
                },
            ]
            with mock.patch.object(soak, "matching_processes", return_value=rows):
                sample = soak.sample_processes(paths, {}, recorder)
            by_pid = {row["pid"]: row for row in sample["process_rss"]}
            self.assertEqual(by_pid[11]["kind"], "dispatcher")
            self.assertEqual(by_pid[22]["kind"], "runtime")
            self.assertEqual(by_pid[11]["rss_kb"], 100)
            self.assertEqual(by_pid[22]["rss_kb"], 200)
            self.assertEqual(sample["rss_kb"], 300)
        finally:
            shutil.rmtree(root, ignore_errors=True)


class VerdictTests(unittest.TestCase):
    def _checks(self, names):
        return {name: {"passed": True} for name in names}

    def test_dry_run_passes_when_all_required_checks_pass(self):
        checks = self._checks(soak.required_checks("dry-run"))
        plan = {"mode": "dry-run", "duration_secs": 420}
        verdict, reasons = soak.compute_verdict(plan, {"duration_secs": 420}, checks)
        self.assertEqual(verdict, "PASS", reasons)

    def test_a_missing_check_fails(self):
        names = soak.required_checks("dry-run")
        checks = self._checks(names[1:])
        plan = {"mode": "dry-run", "duration_secs": 420}
        verdict, reasons = soak.compute_verdict(plan, {"duration_secs": 420}, checks)
        self.assertEqual(verdict, "FAIL")
        self.assertIn("missing checks", reasons[0])

    def test_a_failed_check_fails(self):
        checks = self._checks(soak.required_checks("dry-run"))
        checks[soak.required_checks("dry-run")[0]]["passed"] = False
        plan = {"mode": "dry-run", "duration_secs": 420}
        verdict, reasons = soak.compute_verdict(plan, {"duration_secs": 420}, checks)
        self.assertEqual(verdict, "FAIL")
        self.assertIn("failed checks", reasons[0])

    def test_a_claimed_but_unexecuted_allow_slot_fails_the_verdict(self):
        checks = self._checks(soak.required_checks("dry-run"))
        checks["S4_allow_execution_1"]["passed"] = False
        plan = {"mode": "dry-run", "duration_secs": 420}
        verdict, reasons = soak.compute_verdict(plan, {"duration_secs": 420}, checks)
        self.assertEqual(verdict, "FAIL")
        self.assertIn("S4_allow_execution_1", reasons[0])

    def test_budget_stop_is_not_a_pass(self):
        plan = {"mode": "soak", "duration_secs": 86400, "budget_stopped": True}
        verdict, _ = soak.compute_verdict(plan, {"duration_secs": 86400, "budget_stopped": True}, {})
        self.assertEqual(verdict, "BUDGET_STOPPED")

    def test_signal_stop_is_not_a_pass(self):
        plan = {"mode": "soak", "duration_secs": 86400}
        verdict, _ = soak.compute_verdict(plan, {"duration_secs": 10, "stop_reason": "signal"}, {})
        self.assertEqual(verdict, "STOPPED")

    def test_short_soak_fails_even_with_green_checks(self):
        checks = self._checks(soak.required_checks("soak"))
        plan = {"mode": "soak", "duration_secs": 86400}
        verdict, reasons = soak.compute_verdict(plan, {"duration_secs": 100}, checks)
        self.assertEqual(verdict, "FAIL")
        self.assertTrue(any("planned deadline" in reason for reason in reasons))


class DiskGuardTests(unittest.TestCase):
    def test_free_space_below_the_floor_stops_the_run(self):
        self.assertEqual(soak.disk_stop_reason(1, 2), "disk-low")

    def test_free_space_at_or_above_the_floor_keeps_running(self):
        self.assertIsNone(soak.disk_stop_reason(2, 2))
        self.assertIsNone(soak.disk_stop_reason(3, 2))


class CleanupFallbackTests(unittest.TestCase):
    def _recorder(self, root):
        evidence = Path(root) / "evidence"
        evidence.mkdir()
        paths = soak.RunPaths.for_evidence(evidence, Path(root))
        return soak.Recorder(paths)

    def test_ownership_requires_the_run_live_root_in_the_plist(self):
        self.assertTrue(
            soak.plist_owned_by_run(
                "<string>/private/tmp/lvs-run/homes/os</string>", Path("/private/tmp/lvs-run")
            )
        )
        self.assertFalse(
            soak.plist_owned_by_run(
                "<string>/private/tmp/lvs-other/homes/os</string>", Path("/private/tmp/lvs-run")
            )
        )

    def test_unregister_refuses_a_plist_owned_by_another_run(self):
        import shutil
        from unittest import mock

        root = tempfile.mkdtemp(dir="/tmp", prefix="lvs-clean-test-")
        try:
            live = Path(root) / "lvs-run"
            live.mkdir()
            plist = Path(root) / "foreign.plist"
            plist.write_text("<string>/private/tmp/lvs-other/homes/os</string>")
            calls = []
            with (
                mock.patch.object(soak, "plist_for", return_value=plist),
                mock.patch.object(
                    soak,
                    "run_command",
                    side_effect=lambda args, **kw: calls.append(list(args))
                    or subprocess.CompletedProcess(args, 0, "", ""),
                ),
            ):
                row = soak.unregister_lane_fallback(
                    {"name": "os", "label": "com.leveler.schedule.test"},
                    live,
                    self._recorder(root),
                )
            self.assertEqual(row["action"], "refused-foreign-plist")
            self.assertEqual(calls, [])
            self.assertTrue(plist.is_file())
        finally:
            shutil.rmtree(root, ignore_errors=True)

    def test_unregister_boots_out_and_removes_an_owned_plist(self):
        import shutil
        from unittest import mock

        root = tempfile.mkdtemp(dir="/tmp", prefix="lvs-clean-test-")
        try:
            live = Path(root) / "lvs-run"
            (live / "homes" / "os").mkdir(parents=True)
            plist = Path(root) / "owned.plist"
            plist.write_text("<string>%s</string>" % (live / "homes" / "os"))
            calls = []
            with (
                mock.patch.object(soak, "plist_for", return_value=plist),
                mock.patch.object(
                    soak,
                    "run_command",
                    side_effect=lambda args, **kw: calls.append(list(args))
                    or subprocess.CompletedProcess(args, 0, "", ""),
                ),
            ):
                row = soak.unregister_lane_fallback(
                    {"name": "os", "label": "com.leveler.schedule.test"},
                    live,
                    self._recorder(root),
                )
            self.assertEqual(row["action"], "booted-out")
            self.assertEqual(
                calls,
                [["launchctl", "bootout", "gui/%d/com.leveler.schedule.test" % os.getuid()]],
            )
            self.assertFalse(plist.is_file())
        finally:
            shutil.rmtree(root, ignore_errors=True)


class PreflightIdentityTests(unittest.TestCase):
    def test_preflight_accepts_the_candidate_identity(self):
        args = soak.build_parser().parse_args(
            [
                "preflight",
                "--binary",
                "/tmp/candidate/leveler",
                "--commit",
                "abc123",
                "--source-tree",
                "/tmp/candidate-src",
            ]
        )
        self.assertEqual(args.binary, "/tmp/candidate/leveler")
        self.assertEqual(args.commit, "abc123")
        self.assertEqual(args.source_tree, "/tmp/candidate-src")
        create = soak.build_parser().parse_args(
            ["dry-run", "--source-tree", "/tmp/candidate-src"]
        )
        self.assertEqual(create.source_tree, "/tmp/candidate-src")


class ReportTests(unittest.TestCase):
    def test_report_identity_names_the_terminal_reason_and_duration(self):
        report = {
            "run_id": "run",
            "verdict": "FAIL",
            "stop_reason": "dead-runner",
            "duration_secs": 12,
            "binary": {},
            "verdict_reasons": [],
        }
        text = soak.render_report(report)
        self.assertIn("| stop_reason | dead-runner |", text)
        self.assertIn("| duration_secs | 12 |", text)


class PlanTests(unittest.TestCase):
    def test_soak_mode_schedules_the_rule_check(self):
        names = [action["name"] for action in soak.initial_actions("soak", 0.0)]
        self.assertIn("rules_check", names)

    def test_plan_modes_have_distinct_lane_sets(self):
        dry = soak.lane_specs("dry-run", "deepseek/test")
        soak_lanes = soak.lane_specs("soak", "deepseek/test")
        self.assertIn("agent_short", [spec.key for spec in dry["agent"]])
        self.assertIn("agent_continue", [spec.key for spec in soak_lanes["agent"]])
        self.assertGreater(len(dry["overlap"]), 0)
        self.assertGreater(len(soak_lanes["os"]), 0)

    def test_agent_frequency_is_below_shell_frequency(self):
        lanes = soak.lane_specs("soak", "deepseek/test")
        agent = next(spec for spec in lanes["agent"] if spec.key == "agent_continue")
        self.assertEqual(agent.trigger[0], "--every")
        self.assertGreaterEqual(int(agent.trigger[1]), 3600)


class GateTests(unittest.TestCase):
    def _plan(self):
        return {
            "mode": "dry-run",
            "duration_secs": 420,
            "lanes": {"agent": [{"kind": "agent", "name": "agent", "key": "agent"}]},
            "thresholds": dict(soak.THRESHOLDS),
        }

    def _cleanup(self, **residue):
        base = {"processes": [], "labels": [], "plists": [], "missing_user_labels": []}
        base.update(residue)
        return base

    def _evidence(self):
        return {
            "totals": {"run_states": {"succeeded": 2}, "occurrence_states": {}},
            "duplicates": [],
            "stranded": [],
            "unsettled": [],
            "classified_without_run": [],
            "failures": [],
            "usage": {"rows_without_cost": 0},
            "latency_secs": {"claim": {"p95": 1.0}, "completion": {"p95": 1.0}},
            "samples": [
                {"rss_kb": 1000, "cpu_pct": 1, "db_bytes": 10, "live_root_bytes": 10},
                {"rss_kb": 1100, "cpu_pct": 1, "db_bytes": 12, "live_root_bytes": 12},
            ],
            "lanes": {
                "os": {
                    "run_states": {"succeeded": 1},
                    "wakeups": {"max_gap_secs": 60},
                    "effects": {
                        "shell": {"lines": 1, "succeeded_runs": 1, "effect_matches_success": True}
                    },
                },
                "agent": {
                    "run_states": {"succeeded": 1},
                    "wakeups": {"max_gap_secs": 60},
                    "effects": {},
                },
            },
        }

    def _checks(self, evidence=None, cleanup=None):
        return soak.derive_checks(evidence or self._evidence(), {}, self._plan(), cleanup or self._cleanup())

    def test_empty_skip_and_queue_runs_are_not_a_pass(self):
        self.assertEqual(soak.overlap_observation_status(False, False, 10), "OPEN")
        self.assertEqual(soak.overlap_observation_status(False, False, soak.OVERLAP_OBSERVE_SECS), "UNKNOWN")
        self.assertEqual(soak.overlap_probe_status([(False, False)], soak.OVERLAP_OBSERVE_SECS), "UNKNOWN")
        self.assertNotEqual(soak.overlap_probe_status([(False, False)], soak.OVERLAP_OBSERVE_SECS), "PASS")

    def test_a_terminal_policy_violation_fails_without_waiting(self):
        self.assertEqual(soak.overlap_probe_status([(True, False), (False, False)], 1), "FAIL")

    def test_a_non_fault_shell_failure_fails_the_shell_gate(self):
        evidence = self._evidence()
        evidence["failures"] = [
            {"lane": "os", "state": "failed", "run": "shell-1"},
            {"lane": "fault", "state": "unknown", "run": "fault-1"},
            {"lane": "os", "state": "cancelled", "run": "shell-2"},
        ]
        checks = self._checks(evidence)
        self.assertFalse(checks["S_shell_outcomes"]["passed"])
        self.assertEqual(len(checks["S_shell_outcomes"]["detail"]["failures"]), 1)
        self.assertTrue(checks["S2_agent"]["passed"])
        self.assertIn("S_shell_outcomes", soak.required_checks("dry-run"))
        self.assertIn("S_shell_outcomes", soak.required_checks("soak"))
        verdict, reasons = soak.compute_verdict(
            self._plan(),
            {"duration_secs": 420},
            {name: {"passed": True} for name in soak.required_checks("dry-run")}
            | {"S_shell_outcomes": checks["S_shell_outcomes"]},
        )
        self.assertEqual(verdict, "FAIL")
        self.assertIn("S_shell_outcomes", reasons[0])

    def test_effect_mismatch_and_silent_skip_fail(self):
        evidence = self._evidence()
        evidence["lanes"]["os"]["effects"]["shell"] = {
            "lines": 0,
            "succeeded_runs": 1,
            "effect_matches_success": False,
        }
        evidence["classified_without_run"] = [
            {"state": "skipped", "occurrence": "occ-skip"},
            {"state": "missed", "occurrence": "occ-miss"},
        ]
        checks = self._checks(evidence)
        self.assertFalse(checks["S_effect_match"]["passed"])
        self.assertFalse(checks["S1_occurrences"]["passed"])
        self.assertEqual(len(checks["S1_occurrences"]["detail"]["silent_losses"]), 1)
        self.assertIn("S_effect_match", soak.required_checks("soak"))

    def test_duplicate_marker_lines_count_against_the_frozen_threshold(self):
        evidence = self._evidence()
        evidence["lanes"]["os"]["effects"]["shell"] = {
            "lines": 2,
            "succeeded_runs": 1,
            "effect_matches_success": False,
        }
        checks = self._checks(evidence)
        self.assertGreater(checks["S1_occurrences"]["detail"]["duplicate_side_effects"], 0)
        self.assertFalse(checks["S1_occurrences"]["passed"])

    def test_a_fault_unknown_line_within_the_allowance_is_not_a_duplicate(self):
        evidence = self._evidence()
        evidence["lanes"]["fault"] = {
            "run_states": {"succeeded": 4, "unknown": 1},
            "wakeups": {"max_gap_secs": 60},
            "effects": {
                "fault": {"lines": 5, "succeeded_runs": 4, "effect_matches_success": True}
            },
        }
        checks = self._checks(evidence)
        self.assertEqual(checks["S1_occurrences"]["detail"]["duplicate_side_effects"], 0)
        self.assertTrue(checks["S_effect_match"]["passed"], checks["S_effect_match"])

    def test_wake_gap_agent_ratio_rss_growth_and_orphans_use_frozen_thresholds(self):
        healthy = self._checks()
        self.assertTrue(healthy["S10_performance"]["passed"], healthy["S10_performance"])
        self.assertTrue(healthy["S2_agent"]["passed"], healthy["S2_agent"])
        self.assertTrue(healthy["S9_residue"]["passed"], healthy["S9_residue"])

        gapped = self._evidence()
        gapped["lanes"]["os"]["wakeups"]["max_gap_secs"] = soak.THRESHOLDS["max_unexplained_wake_gap_secs"] + 1
        self.assertFalse(self._checks(gapped)["S10_performance"]["passed"])

        unmeasured = self._evidence()
        unmeasured["lanes"]["os"]["wakeups"] = {}
        unmeasured["lanes"]["agent"]["wakeups"] = {}
        self.assertFalse(self._checks(unmeasured)["S10_performance"]["passed"])

        ratio = self._evidence()
        ratio["lanes"]["agent"]["run_states"] = {"succeeded": 1, "failed": 1}
        ratio_checks = self._checks(ratio)
        self.assertFalse(ratio_checks["S2_agent"]["passed"])
        self.assertLess(ratio_checks["S2_agent"]["detail"]["agent_success_ratio"], 1)

        # Absolute RSS above the growth budget, growth well under it.
        high = self._evidence()
        high["samples"] = [
            {"rss_kb": 590_000, "cpu_pct": 1, "db_bytes": 10, "live_root_bytes": 10},
            {"rss_kb": 600_000, "cpu_pct": 1, "db_bytes": 12, "live_root_bytes": 12},
        ]
        high_checks = self._checks(high)
        self.assertTrue(high_checks["S10_performance"]["passed"], high_checks["S10_performance"])
        self.assertLess(
            high_checks["S10_performance"]["detail"]["rss_growth_bytes"],
            soak.THRESHOLDS["max_rss_growth_bytes"],
        )

        grown = self._evidence()
        grown["samples"] = [
            {"rss_kb": 1_000, "cpu_pct": 1, "db_bytes": 10, "live_root_bytes": 10},
            {"rss_kb": 601_000, "cpu_pct": 1, "db_bytes": 12, "live_root_bytes": 12},
        ]
        self.assertFalse(self._checks(grown)["S10_performance"]["passed"])

        orphans = self._checks(cleanup=self._cleanup(processes=[{"pid": 9, "command": "sleep"}]))
        self.assertFalse(orphans["S9_residue"]["passed"])
        self.assertEqual(orphans["S9_residue"]["detail"]["orphan_processes"], 1)

    def test_runner_errors_fail_apart_from_product_checks(self):
        checks = {name: {"passed": True} for name in soak.required_checks("dry-run")}
        state = {"duration_secs": 420}
        soak.record_runner_error(state, "s4_verify_1", "disk full")
        verdict, reasons = soak.compute_verdict(self._plan(), state, checks)
        self.assertEqual(verdict, "FAIL")
        self.assertTrue(any(reason.startswith("runner errors:") for reason in reasons))
        self.assertFalse(any(reason.startswith("failed checks:") for reason in reasons))

    def test_a_runner_exception_is_not_recorded_as_a_product_check(self):
        import shutil

        root = Path(tempfile.mkdtemp(dir="/tmp", prefix="lvs-gate-test-"))
        try:
            evidence = root / "evidence"
            evidence.mkdir()
            paths = soak.RunPaths.for_evidence(evidence, root)
            plan = {
                "mode": "dry-run",
                "start_at": "2026-11-06T00:00:00Z",
                "deadline_at": "2026-11-06T00:07:00Z",
                "duration_secs": 420,
                "lanes": {},
            }
            state = {"checks": {}}
            supervisor = soak.Supervisor(paths, plan, state, soak.Recorder(paths))

            def explode(_name):
                raise RuntimeError("runner blew up")

            supervisor.action_handler = lambda _name: explode  # type: ignore[method-assign]
            supervisor.execute_action({"name": "s4_verify_1", "status": "pending", "attempts": 0})
            self.assertEqual(state["runner_errors"][0]["source"], "runner")
            self.assertNotIn("s4_verify_1", supervisor.checks)
            self.assertNotIn("S4_1", supervisor.checks)
        finally:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
