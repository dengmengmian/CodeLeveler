"""Metrics-only catalog for micro tasks. Never rendered into the model prompt."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

FORBIDDEN_IN_TASK = (
    "spawn_agent",
    "delegate",
    "delegation",
    "multi-agent",
    "multi agent",
    "subagent",
    "sub-agent",
    "keep-vs-delegate",
    "expected_disposition",
)


def load_catalog(path: Path) -> dict[str, Any]:
    text = path.read_text(encoding="utf-8")
    return json.loads(text)


def _entry(catalog: dict[str, Any], task_id: str) -> dict[str, Any]:
    tasks = catalog.get("tasks") or {}
    entry = tasks.get(task_id) or {}
    return entry if isinstance(entry, dict) else {}


def expected_disposition(catalog: dict[str, Any], task_id: str) -> str | None:
    return _entry(catalog, task_id).get("expected_disposition")


def task_shape(catalog: dict[str, Any], task_id: str) -> str | None:
    return _entry(catalog, task_id).get("shape")


def assert_task_text_clean(task_yaml: str) -> list[str]:
    lower = task_yaml.lower()
    return [token for token in FORBIDDEN_IN_TASK if token in lower]
