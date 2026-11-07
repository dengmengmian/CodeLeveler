"""Evidence archives must survive builds and never reuse a run directory."""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
import urllib.request
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'append_instruction_dogfood.py'
spec = importlib.util.spec_from_file_location('append_dogfood', SCRIPT)
dog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dog)
phase3_spec = importlib.util.spec_from_file_location('append_phase3', SCRIPT.with_name('append_instruction_phase3.py'))
phase3 = importlib.util.module_from_spec(phase3_spec)
phase3_spec.loader.exec_module(phase3)


class EvidenceArchiveTests(unittest.TestCase):
    def test_proxy_close_waits_for_request_writer_before_sealing(self):
        started = threading.Event()
        release = threading.Event()
        closing_workers = threading.Event()
        closed = threading.Event()
        errors = []

        def handle(proxy, handler, body):
            started.set()
            try:
                if not release.wait(5):
                    raise RuntimeError('test did not release request')
                proxy._write({'kind': 'request_end', 'seq': 1})
                handler.send_response(200)
                handler.send_header('Connection', 'close')
                handler.end_headers()
            except Exception as error:
                errors.append(error)
            finally:
                handler.close_connection = True

        with tempfile.TemporaryDirectory() as root, patch.object(dog.RecordingProxy, 'handle', handle):
            proxy = dog.RecordingProxy(root, 'https://example.invalid/v1')
            original_close = proxy.server.server_close

            def server_close():
                closing_workers.set()
                original_close()

            proxy.server.server_close = server_close

            def request():
                try:
                    with urllib.request.urlopen(urllib.request.Request(proxy.base_url(), data=b'{}'), timeout=5) as response:
                        response.read()
                except Exception as error:
                    errors.append(error)

            def close():
                proxy.close()
                closed.set()

            client = threading.Thread(target=request)
            closer = threading.Thread(target=close)
            client.start()
            try:
                self.assertTrue(started.wait(5))
                closer.start()
                self.assertTrue(closing_workers.wait(5))
                self.assertFalse(closed.wait(0.1), 'proxy closed its evidence writer while a handler was still running')
            finally:
                release.set()
                client.join(5)
                if closer.ident is not None:
                    closer.join(5)
                else:
                    proxy.close()
            self.assertTrue(closed.is_set())
            self.assertEqual(errors, [])
            self.assertEqual(json.loads((Path(root) / 'proxy.jsonl').read_text()), {'kind': 'request_end', 'seq': 1})

    def test_raw_database_snapshot_keeps_committed_wal_without_copying_config(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root) / 'home'
            state = home / 'state' / 'projects' / 'fixture'
            state.mkdir(parents=True)
            (home / 'config.toml').write_text('api_key = "private-key"')
            archive = Path(root) / 'evidence'
            archive.mkdir()
            source = sqlite3.connect(str(state / 'sessions.db'))
            try:
                self.assertEqual(source.execute('PRAGMA journal_mode=WAL').fetchone()[0], 'wal')
                source.execute('CREATE TABLE session_inputs (id TEXT, content TEXT)')
                source.execute('INSERT INTO session_inputs VALUES (?, ?)', ('stable-id', 'saved in WAL'))
                source.commit()
                self.assertTrue((state / 'sessions.db-wal').exists())
                snapshot = dog.snapshot_database(str(home), str(archive))
                with sqlite3.connect(snapshot) as recovered:
                    self.assertEqual(recovered.execute('SELECT * FROM session_inputs').fetchall(), [('stable-id', 'saved in WAL')])
                    self.assertEqual(recovered.execute('PRAGMA integrity_check').fetchone()[0], 'ok')
                self.assertEqual([p.name for p in archive.iterdir()], ['sessions.sqlite3'])
                self.assertNotIn(b'private-key', Path(snapshot).read_bytes())
            finally:
                source.close()

    def test_web_log_redacts_token_without_changing_process_arguments(self):
        with tempfile.TemporaryDirectory() as root:
            case = phase3.Case.__new__(phase3.Case)
            case.dir = root
            case.repo = root
            case.env = {}
            case.token = 'private-daemon-bearer-token'
            case.daemon_port = 11111
            case.log = io.StringIO()
            with patch.object(phase3, 'free_port', return_value=22222), patch.object(phase3, 'wait_until', return_value=True), patch.object(phase3.subprocess, 'Popen') as spawn, patch('builtins.print') as output:
                try:
                    self.assertTrue(case.start_web())
                    argv = spawn.call_args.args[0]
                    self.assertEqual(argv[argv.index('--token') + 1], case.token)
                    self.assertNotIn(case.token, case.log.getvalue())
                    self.assertNotIn(case.token, str(output.call_args_list))
                finally:
                    case.web_log.close()

    def test_unique_runs_preserve_previous_result(self):
        with tempfile.TemporaryDirectory() as root, patch.dict(os.environ, {'LEVELER_APPEND_EVIDENCE_ROOT': root}):
            first = Path(dog.create_evidence_dir('same-case'))
            (first / 'result.json').write_text('original')
            second = Path(dog.create_evidence_dir('same-case'))
            self.assertNotEqual(first, second)
            self.assertEqual(first.parent, Path(root))
            self.assertEqual((first / 'result.json').read_text(), 'original')
            self.assertEqual(first.stat().st_mode & 0o777, 0o700)

    def test_default_archive_is_outside_build_tree(self):
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(dog.evidence_root(), os.path.expanduser('~/CodeLeveler-append-instruction-evidence'))

    def test_manifest_covers_final_artifacts_and_records_safe_model_identity(self):
        with tempfile.TemporaryDirectory() as root:
            run = Path(root) / 'run'
            run.mkdir()
            binary = Path(root) / 'leveler'
            binary.write_bytes(b'binary-under-test')
            (run / 'requests').mkdir()
            (run / 'requests' / '0001.json').write_text(json.dumps({'model': 'real-model', 'reasoning_effort': 'high', 'api_key': 'never-copy-into-metadata'}))
            (run / 'result.json').write_text('{"ok": true}')
            dog.write_evidence_identity(str(run), str(binary))
            dog.finalize_evidence(str(run))
            identity = json.loads((run / 'identity.json').read_text())
            self.assertEqual(identity['binary_sha256'], hashlib.sha256(binary.read_bytes()).hexdigest())
            self.assertEqual(identity['observed_model_configs'], [{'model': 'real-model', 'reasoning_effort': 'high'}])
            self.assertNotIn('never-copy-into-metadata', (run / 'identity.json').read_text())
            manifest = (run / 'SHA256SUMS').read_text().splitlines()
            for line in manifest:
                digest, path = line.split('  ', 1)
                self.assertEqual(hashlib.sha256((run / path).read_bytes()).hexdigest(), digest)
            self.assertTrue(any(line.endswith('  result.json') for line in manifest))
            self.assertTrue(any(line.endswith('  identity.json') for line in manifest))


if __name__ == '__main__':
    unittest.main()
