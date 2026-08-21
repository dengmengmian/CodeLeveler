"""Load eval experiment configs. Parameters live in YAML, not in runner code.

Supports a restricted YAML subset (maps, lists, scalars). No PyYAML dependency.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

REQUIRED = ("suite", "experiment", "model", "runs", "metrics")


def _parse_scalar(raw: str) -> Any:
    text = raw.strip()
    if text in ("", "~", "null", "Null", "NULL"):
        return None
    if text in ("true", "True", "yes"):
        return True
    if text in ("false", "False", "no"):
        return False
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "\"'":
        return text[1:-1]
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        if not inner:
            return []
        return [_parse_scalar(part) for part in inner.split(",")]
    try:
        if "." in text:
            return float(text)
        return int(text)
    except ValueError:
        return text


def _parse_restricted_yaml(text: str) -> dict[str, Any]:
    lines: list[tuple[int, str]] = []
    for raw in text.splitlines():
        if "#" in raw:
            raw = raw[: raw.index("#")]
        if not raw.strip():
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        lines.append((indent, raw.strip()))

    def parse_block(index: int, indent: int) -> tuple[Any, int]:
        mapping: dict[str, Any] = {}
        sequence: list[Any] | None = None
        while index < len(lines):
            current_indent, content = lines[index]
            if current_indent < indent:
                break
            if current_indent > indent:
                raise ValueError(f"unexpected indent at {content!r}")
            if content.startswith("- "):
                if mapping:
                    raise ValueError("cannot mix map and list at the same indent")
                if sequence is None:
                    sequence = []
                item = content[2:].strip()
                if item:
                    sequence.append(_parse_scalar(item))
                    index += 1
                else:
                    value, index = parse_block(index + 1, indent + 2)
                    sequence.append(value)
                continue
            if sequence is not None:
                raise ValueError("cannot mix map and list at the same indent")
            key, _, rest = content.partition(":")
            key = key.strip()
            rest = rest.strip()
            index += 1
            if rest:
                mapping[key] = _parse_scalar(rest)
            else:
                if index < len(lines) and lines[index][0] > current_indent:
                    value, index = parse_block(index, lines[index][0])
                    mapping[key] = value
                else:
                    mapping[key] = None
        if sequence is not None:
            return sequence, index
        return mapping, index

    doc, _end = parse_block(0, 0)
    if not isinstance(doc, dict):
        raise ValueError("experiment config must be a mapping")
    return doc


def load_experiment(path: Path) -> dict[str, Any]:
    text = path.read_text(encoding="utf-8")
    cfg = _parse_restricted_yaml(text)
    missing = [k for k in REQUIRED if k not in cfg]
    if missing:
        raise ValueError(f"{path}: missing {', '.join(missing)}")
    if not isinstance(cfg["runs"], int) or cfg["runs"] < 1:
        raise ValueError(f"{path}: runs must be a positive integer")
    if not isinstance(cfg["metrics"], list) or not cfg["metrics"]:
        raise ValueError(f"{path}: metrics must be a non-empty list")
    cfg.setdefault("provider", None)
    cfg.setdefault("binary", "leveler")
    cfg.setdefault("tasks", [])
    cfg.setdefault("shape", None)
    cfg.setdefault("timeout_seconds", 1200)
    cfg.setdefault("arm", "control")
    cfg.setdefault("output", f"eval/reports/{cfg['suite']}/{cfg['experiment']}")
    cfg.setdefault("exclude", [])
    cfg.setdefault("population", "model_initiated_only")
    cfg.setdefault("changes_runtime", False)
    cfg["_path"] = str(path)
    return cfg


def resolve_experiment(eval_root: Path, suite: str, experiment: str) -> Path:
    path = eval_root / "configs" / suite / f"{experiment}.yaml"
    if not path.is_file():
        raise FileNotFoundError(f"experiment config not found: {path}")
    return path


def apply_overrides(
    cfg: dict[str, Any],
    *,
    model: str | None = None,
    provider: str | None = None,
    runs: int | None = None,
    output: str | None = None,
    binary: str | None = None,
    task: str | None = None,
    shape: str | None = None,
) -> dict[str, Any]:
    out = dict(cfg)
    if model:
        out["model"] = model
    if provider:
        out["provider"] = provider
    if runs is not None:
        out["runs"] = runs
    if output:
        out["output"] = output
    if binary:
        out["binary"] = binary
    if task:
        out["tasks"] = [task]
    if shape:
        out["shape"] = shape
    return out


def model_ref(cfg: dict[str, Any]) -> str:
    model = cfg.get("model") or ""
    provider = cfg.get("provider")
    if provider and "/" not in str(model):
        return f"{provider}/{model}"
    return str(model)
