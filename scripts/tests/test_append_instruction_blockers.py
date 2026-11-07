"""Contracts for diagnostic request changes and independent HTTP acceptance."""

import copy
import importlib.util
import json
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "append_instruction_blockers.py"
spec = importlib.util.spec_from_file_location("append_blockers", SCRIPT)
blockers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(blockers)

PHASE3 = Path(__file__).resolve().parents[1] / "append_instruction_phase3.py"
phase3_spec = importlib.util.spec_from_file_location("append_phase3", PHASE3)
phase3 = importlib.util.module_from_spec(phase3_spec)
phase3_spec.loader.exec_module(phase3)


class BlockerExperimentTests(unittest.TestCase):
    def test_counterfactual_removes_only_exact_runtime_pin_copy(self):
        pin = "[Active objective — host-pinned]\n<objective>\nOriginal task\n</objective>\nThis is the active request for this turn."
        request = {"model": "real-model", "messages": [
            {"role": "system", "content": "system policy"},
            {"role": "user", "content": pin},
            {"role": "user", "content": "Original task"},
            {"role": "assistant", "content": "in progress", "tool_calls": [{"id": "keep"}]},
            {"role": "user", "content": "New user requirement", "images": ["preserve"]},
        ]}
        before = copy.deepcopy(request)
        changed, removed = blockers.without_original_pin(request, "Original task")
        self.assertEqual(removed, [1])
        self.assertEqual(changed["messages"], [before["messages"][i] for i in [0, 2, 3, 4]])
        self.assertEqual(changed["model"], "real-model")
        self.assertEqual(request, before)

    def test_counterfactual_preserves_similar_user_text_and_other_roles(self):
        exact = "[Active objective — host-pinned]\n<objective>\nOriginal task\n</objective>\nThis is the active request for this turn."
        messages = [
            {"role": "system", "content": exact},
            {"role": "user", "content": exact + " This is my quotation."},
            {"role": "user", "content": exact.replace("Original task", "Different task")},
        ]
        request = {"messages": messages}
        changed, removed = blockers.without_original_pin(request, "Original task")
        self.assertEqual(changed, request)
        self.assertEqual(removed, [])

    def test_ambiguous_duplicate_pins_fail_instead_of_dropping_user_rows(self):
        exact = "[Active objective — host-pinned]\n<objective>\nOriginal task\n</objective>\nThis is the active request for this turn."
        with self.assertRaises(ValueError):
            blockers.without_original_pin({"messages": [
                {"role": "user", "content": exact},
                {"role": "user", "content": exact},
            ]}, "Original task")

    def test_health_acceptance_requires_actual_http_json_success(self):
        self.assertTrue(blockers.valid_health_response({
            "status": 200, "type": "application/json; charset=utf-8", "body": '{"status":"ok"}',
        }))
        for page in [
            {"status": 404, "type": "application/json", "body": '{"status":"ok"}'},
            {"status": 200, "type": "text/html", "body": '<p>{"status":"ok"}</p>'},
            {"status": 200, "type": "application/json", "body": '"status ok"'},
            {"status": 200, "type": "application/json", "body": '{"status":"failed"}'},
            {"status": 200, "type": "application/json", "body": 'not JSON'},
        ]:
            with self.subTest(page=page):
                self.assertFalse(blockers.valid_health_response(page))


class StructuredTranscriptRowTests(unittest.TestCase):
    TEXT = '引号 " 换行\n反斜杠 \\ 与 Unicode ✅'

    def row(self, text, role="user", input_id="journal-1", mid="message-1"):
        payload = json.dumps({
            "role": role,
            "content": [{"type": "text", "text": text}],
            "origin": {"kind": "user_input"},
        }, ensure_ascii=False)
        return {"id": mid, "ordinal": 3, "turn_id": "turn-1",
                "input_id": input_id, "payload": payload}

    def test_decoded_text_matches_even_when_raw_payload_cannot(self):
        row = self.row(self.TEXT)
        # The escaped payload is exactly why a raw substring probe failed.
        self.assertNotIn(self.TEXT, row["payload"])
        rows = phase3.user_message_rows_in([row], self.TEXT)
        self.assertEqual([(r["id"], r["input_id"]) for r in rows],
                         [("message-1", "journal-1")])

    def test_role_and_exact_text_decide_the_match(self):
        assistant = self.row(self.TEXT, role="assistant", mid="assistant")
        near_miss = self.row(self.TEXT + " ", mid="near-miss")
        self.assertEqual(phase3.user_message_rows_in([assistant, near_miss], self.TEXT), [])

    def test_string_content_is_still_structured(self):
        payload = json.dumps({"role": "user", "content": self.TEXT}, ensure_ascii=False)
        row = {"id": "message-2", "ordinal": 4, "turn_id": "turn-1",
               "input_id": "journal-2", "payload": payload}
        self.assertEqual(phase3.user_message_rows_in([row], self.TEXT), [row])

    def test_broken_or_non_text_payloads_are_not_guessed(self):
        broken = {"id": "broken", "payload": "{not json"}
        image_only = {"id": "image", "payload": json.dumps({
            "role": "user", "content": [{"type": "image", "text": self.TEXT}],
        })}
        self.assertEqual(phase3.user_message_rows_in([broken, image_only], self.TEXT), [])


if __name__ == "__main__":
    unittest.main()
