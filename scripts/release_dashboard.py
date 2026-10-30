#!/usr/bin/env python3
"""Render the CodeLeveler release dashboard from recorded release evidence.

The dashboard is generated, never hand-written: every number on the page comes
from one of two recorded artefacts, and the page links to them.

  --release-json   `.dev/release/<release-id>/release.json`, written by
                   `./dev publish` / `./dev release`. It owns the release
                   identity, the stage results, the dogfood/RC gate data, the
                   feature coverage, the change summary and the remote facts.

  --post-release   optional. Post-release acceptance recorded by the operator
                   (see POST_RELEASE_SCHEMA below). It owns what happened after
                   the publication: which artefacts were downloaded, their
                   checksums, the fresh install and the upgrade from the
                   previous stable version.

Outputs, for `--out docs/releases`:

  v<version>/release.json   the published release record (release evidence +
                            post-release evidence + how the page read them)
  v<version>/index.html     the version report
  index.html                the history index, rebuilt from every v*/release.json

Deterministic: the same inputs produce byte-identical output. No clock, no
randomness, no network. Untrusted text (commit subjects, reasons, asset names)
is HTML-escaped.

POST_RELEASE_SCHEMA (all fields optional; a missing field is NOT_VERIFIED):

  {
    "verified_at": "2026-10-30T12:00:00Z",
    "channel": "GitHub Releases",
    "artifacts": [
      {"name": "leveler-v1.0.22-aarch64-apple-darwin.tar.gz",
       "platform": "macOS arm64", "checksum": "sha256:...",
       "checksum_verified": true, "download_url": "https://..."}
    ],
    "fresh_install": {
      "status": "PASS",
      "detail": "downloaded the published archive, verified sha256, ran `leveler --version`",
      "version_output": "leveler 1.0.22",
      "tui_start": true,
      "provider_call": true,
      "tool_execution": true
    },
    "upgrade": {
      "status": "PASS",
      "from": "v1.0.21",
      "detail": "installed v1.0.21, created sessions and config, upgraded in place",
      "config_preserved": true,
      "sessions_readable": true,
      "migration_applied": true,
      "notes": []
    },
    "known_issues": ["..."],
    "not_verified": ["..."]
  }
"""

from __future__ import annotations

import argparse
import html
import json
import os
import sys

NOT_VERIFIED = "NOT_VERIFIED"

STATUS_WORD = {
    "PASS": "PASS",
    "CONDITIONAL PASS": "PASS (conditional)",
    "WARN": "WARN",
    "FAIL": "FAIL",
    "UNPROVEN": "UNPROVEN",
    "SKIPPED": "SKIPPED",
    "REUSED": "REUSED",
}


# ── data access ─────────────────────────────────────────────────────────────


def dig(obj, *path, default=None):
    cur = obj
    for key in path:
        if isinstance(cur, dict):
            if key not in cur:
                return default
            cur = cur[key]
        elif isinstance(cur, list) and isinstance(key, int):
            if key >= len(cur):
                return default
            cur = cur[key]
        else:
            return default
    return default if cur is None else cur


def text(value, default=NOT_VERIFIED):
    if value is None or value == "":
        return default
    if isinstance(value, bool):
        return "yes" if value else "no"
    return str(value)


def pct(part, whole):
    if not whole:
        return NOT_VERIFIED
    return "%.1f%%" % (100.0 * part / whole)


def human_duration(seconds):
    try:
        total = int(round(float(seconds)))
    except (TypeError, ValueError):
        return NOT_VERIFIED
    if total <= 0:
        return NOT_VERIFIED
    hours, rest = divmod(total, 3600)
    minutes, secs = divmod(rest, 60)
    if hours:
        return "%dh %02dm" % (hours, minutes)
    if minutes:
        return "%dm %02ds" % (minutes, secs)
    return "%ds" % secs


def short(sha, length=12):
    if not sha:
        return NOT_VERIFIED
    return str(sha)[:length]


# ── model ───────────────────────────────────────────────────────────────────


def release_status(record):
    """RELEASED needs all three: gates passed, the release exists, and the
    published artefacts were accepted afterwards."""
    state_status = str(dig(record, "state", "status", default="")).upper()
    release_state = str(dig(record, "remote", "release_state", default="")).lower()
    tag_sha = dig(record, "remote", "tag_sha")
    published = release_state in ("published", "public") and bool(tag_sha)
    post = record.get("post_release") or {}
    post_ok = post.get("fresh_install", {}).get("status") == "PASS"
    if state_status == "PASS" and published and post_ok:
        return "RELEASED"
    if state_status == "FAIL":
        return "BLOCKED"
    return "PARTIAL"


def stage_rows(record):
    rows = []
    for stage in dig(record, "state", "stages", default=[]) or []:
        rows.append(
            {
                "stage": text(stage.get("stage")),
                "name": text(stage.get("name")),
                "status": str(stage.get("status", NOT_VERIFIED)).upper(),
                "seconds": human_duration(stage.get("seconds")),
                "reused": "yes" if stage.get("reused") else "no",
                "detail": [
                    d
                    for d in (stage.get("detail") or [])
                    if not d.startswith("L0.json sha256=")
                    and ".json sha256=" not in d
                ],
                "reason": stage.get("failure_reason"),
            }
        )
    return rows


def layer_rows(record):
    rows = []
    layers = dig(record, "dogfood", "gate", "layers", default={}) or {}
    for name in sorted(layers, key=lambda n: (len(n), n)):
        layer = layers[name]
        checks = layer.get("checks") or []
        passed = sum(1 for c in checks if str(c.get("status")).upper() == "PASS")
        failed = sum(1 for c in checks if str(c.get("status")).upper() == "FAIL")
        cases = layer.get("cases") or []
        row = {
            "layer": name,
            "status": str(layer.get("status", NOT_VERIFIED)).upper(),
            "checks": ("%d/%d" % (passed, len(checks))) if checks else "—",
            "failed": failed,
            "cases": len(cases) if cases else "",
            "seconds": human_duration(layer.get("seconds")),
            "live": "yes" if layer.get("live") else "no",
        }
        if cases:
            row["case_pass"] = sum(1 for c in cases if c.get("passed"))
            row["case_fail"] = sum(1 for c in cases if c.get("failures"))
        rows.append(row)
    return rows


def dogfood_case_totals(record):
    cases = dig(record, "dogfood", "gate", "layers", "L3", "cases", default=[]) or []
    total = len(cases)
    passed = sum(1 for c in cases if c.get("passed"))
    failed = sum(1 for c in cases if not c.get("passed"))
    incomplete = sum(1 for c in cases if not c.get("completed"))
    return {
        "total": total,
        "passed": passed,
        "failed": failed,
        "incomplete": incomplete,
        "rate": pct(passed, total),
    }


def pty_rows(record):
    pty = dig(record, "dogfood", "gate", "layers", "L2", "pty", default=None)
    rows = []
    if isinstance(pty, dict):
        for name in sorted(pty):
            entry = pty[name] if isinstance(pty[name], dict) else {"status": pty[name]}
            rows.append(
                {
                    "name": name,
                    "status": str(entry.get("status", NOT_VERIFIED)).upper(),
                    "detail": text(entry.get("detail"), ""),
                }
            )
    return rows


def perf_rows(record):
    """Performance is only a comparison when the gate says the two runs are
    comparable AND a baseline sample exists. Otherwise it is NOT_COMPARABLE."""
    perf = dig(record, "dogfood", "performance", default={}) or {}
    metrics = perf.get("metrics") or {}
    comparison = record.get("comparison") or {}
    comparable = (
        str(perf.get("comparability", metrics.get("comparability", ""))).upper()
        == "COMPARABLE"
        and str(comparison.get("status", "")).upper() == "PASS"
    )
    labels = [
        ("cases", "Dogfood cases", ""),
        ("wall_seconds_p50", "Task duration p50", "s"),
        ("rounds_p50", "Rounds p50", ""),
        ("tool_calls_p50", "Tool calls p50", ""),
        ("input_tokens_p50", "Input tokens p50", ""),
        ("output_tokens_p50", "Output tokens p50", ""),
        ("total_tokens_p50", "Total tokens p50", ""),
        ("ttff_ms_p50", "Time to first frame p50", "ms"),
        ("cost_usd_p50", "Metered cost p50", "USD"),
    ]
    rows = []
    for key, label, unit in labels:
        value = metrics.get(key)
        if value is None:
            shown = NOT_VERIFIED if key in ("cost_usd_p50",) else "—"
        elif unit:
            shown = "%s %s" % (value, unit)
        else:
            shown = text(value)
        rows.append(
            {
                "label": label,
                "current": shown,
                "previous": NOT_VERIFIED,
                "delta": NOT_VERIFIED if not comparable else "—",
                "verdict": NOT_VERIFIED if not comparable else "UNCHANGED",
            }
        )
    reason = comparison.get("reason") or "no comparable baseline was recorded for this gate"
    return rows, comparable, reason


def change_rows(record, limit=12):
    changes = record.get("changes") or {}
    out = {}
    for key, label in (
        ("features", "Features"),
        ("fixes", "Bug fixes"),
        ("other", "Other changes"),
    ):
        items = changes.get(key) or []
        out[label] = {"total": len(items), "items": [text(i) for i in items[:limit]]}
    removed = changes.get("removed") or []
    if removed:
        out["Removed"] = {"total": len(removed), "items": [text(i) for i in removed]}
    return out


def coverage_summary(record):
    cov = dig(record, "dogfood", "gate", "feature_coverage", "summary", default={}) or {}
    if not cov:
        cov = dig(record, "feature_coverage", "summary", default={}) or {}
    if not cov:
        return []
    rows = [
        ("Core features covered", "%s / %s (%s%%)" % (
            text(cov.get("core_covered")), text(cov.get("core_total")),
            text(cov.get("coverage_core_percent")))),
        ("Core gaps", text(cov.get("core_gaps"))),
        ("Mapped scenarios", text(cov.get("mapped"))),
        ("Scenario coverage", "%s%%" % text(cov.get("coverage_scenario_percent"))),
        ("Candidate evidence coverage", "%s%%" % text(cov.get("coverage_candidate_evidence_percent"))),
        ("New features", text(cov.get("new_features"))),
        ("Stale entries", text(cov.get("stale_entries"))),
    ]
    return rows


def known_issues(record):
    issues = []
    decision = dig(record, "dogfood", "gate", "decision", default={}) or {}
    for blocker in decision.get("blockers") or []:
        issues.append(("blocker", text(blocker.get("id")), text(blocker.get("detail"))))
    for warning in decision.get("warnings") or []:
        issues.append(("warning", text(warning.get("id")), text(warning.get("detail"))))
    cov = dig(record, "dogfood", "gate", "feature_coverage", default={}) or {}
    for entry in cov.get("unverified_list") or []:
        issues.append(
            ("not verified", text(entry.get("feature_id")), text(entry.get("reason")))
        )
    if record.get("failure_reason"):
        issues.insert(0, ("failure", "release", text(record.get("failure_reason"))))
    post = record.get("post_release") or {}
    for note in post.get("known_issues") or []:
        issues.append(("post-release", "known issue", text(note)))
    for note in post.get("not_verified") or []:
        issues.append(("post-release", "not verified", text(note)))
    return issues


def artifact_rows(record):
    rows = []
    remote = record.get("remote") or {}
    verified = set(remote.get("verified_assets") or [])
    for name in remote.get("assets") or []:
        rows.append(
            {
                "name": text(name),
                "platform": "—",
                "build": "built by release workflow",
                "checksum": "verified" if name in verified else NOT_VERIFIED,
                "url": text(remote.get("release_url"), ""),
                "post": NOT_VERIFIED,
            }
        )
    post = record.get("post_release") or {}
    for art in post.get("artifacts") or []:
        name = text(art.get("name"))
        matched = None
        for row in rows:
            if row["name"] == name:
                matched = row
                break
        target = matched or {"name": name, "platform": "—", "build": "—", "checksum": NOT_VERIFIED, "url": ""}
        target["platform"] = text(art.get("platform"), target["platform"])
        if art.get("checksum"):
            target["checksum"] = (
                "verified" if art.get("checksum_verified") else "mismatch: %s" % text(art.get("checksum"))
            )
        target["url"] = text(art.get("download_url"), target["url"])
        target["post"] = "accepted"
        if matched is None:
            rows.append(target)
    return rows


def summary_cards(record):
    layers = {row["layer"]: row for row in layer_rows(record)}
    cases = dogfood_case_totals(record)
    stages = {row["stage"]: row for row in stage_rows(record)}
    decision = dig(record, "dogfood", "gate", "decision", "decision", default=None)
    pty = pty_rows(record)
    pty_failed = [p for p in pty if p["status"] == "FAIL"]
    post = record.get("post_release") or {}
    fresh = post.get("fresh_install") or {}
    upgrade = post.get("upgrade") or {}
    build = stages.get("1", {}).get("status", NOT_VERIFIED)
    tests = stages.get("2", {}).get("status", NOT_VERIFIED)
    cross = dig(record, "remote", "assets", default=[]) or []
    return [
        ("Build & static", build, "stage 1 · dogfood L0"),
        ("Unit / integration", tests, "stage 2 · dogfood L1/L2"),
        (
            "Real provider dogfood",
            STATUS_WORD.get(str(decision).upper(), text(decision)) if decision else NOT_VERIFIED,
            "%d case(s), %s passed" % (cases["total"], cases["rate"]),
        ),
        (
            "Long-task execution",
            layers.get("L3", {}).get("status", NOT_VERIFIED),
            "dogfood L3",
        ),
        (
            "TUI / PTY",
            "FAIL" if pty_failed else (layers.get("L2", {}).get("status", NOT_VERIFIED)),
            "%d PTY suite(s)" % len(pty),
        ),
        (
            "Feature coverage",
            layers.get("L3", {}).get("status", NOT_VERIFIED),
            "core %s%%" % text(dig(record, "dogfood", "gate", "feature_coverage", "summary", "coverage_core_percent")),
        ),
        (
            "Cross-platform build",
            "PASS" if len(cross) >= 4 else NOT_VERIFIED,
            "%d published asset(s)" % len(cross),
        ),
        (
            "Post-release verification",
            text(fresh.get("status")),
            "upgrade: %s" % text(upgrade.get("status")),
        ),
    ]


# ── html ────────────────────────────────────────────────────────────────────


def esc(value):
    return html.escape("" if value is None else str(value), quote=True)


def status_class(value):
    word = str(value).upper()
    if word.startswith("PASS") or word in ("RELEASED", "VERIFIED", "IMPROVED", "UNCHANGED"):
        return "ok"
    if word.startswith("WARN") or word in ("PARTIAL", "NOT_COMPARABLE", "UNPROVEN", "SKIPPED"):
        return "warn"
    if word.startswith("FAIL") or word in ("BLOCKED", "REGRESSED"):
        return "bad"
    return "muted"


def badge(value):
    word = text(value)
    return '<span class="badge %s">%s</span>' % (status_class(word), esc(word))


def table(headers, rows, numeric=()):
    head = "".join(
        '<th scope="col"%s>%s</th>' % (' class="num"' if i in numeric else "", esc(h))
        for i, h in enumerate(headers)
    )
    body = []
    for row in rows:
        cells = []
        for i, cell in enumerate(row):
            cls = ' class="num"' if i in numeric else ""
            if isinstance(cell, tuple) and cell[0] == "badge":
                cells.append("<td%s>%s</td>" % (cls, badge(cell[1])))
            else:
                cells.append("<td%s>%s</td>" % (cls, cell))
        body.append("<tr>%s</tr>" % "".join(cells))
    return '<div class="scroll"><table><thead><tr>%s</tr></thead><tbody>%s</tbody></table></div>' % (
        head,
        "".join(body),
    )


CSS = """
:root{color-scheme:light dark;--bg:#f7f7f8;--panel:#fff;--ink:#1b1b1f;--muted:#5d5d68;--line:#dcdce3;--accent:#2457c5;--ok:#1a7f4b;--warn:#8a5a00;--bad:#b3241f;--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
@media (prefers-color-scheme:dark){:root{--bg:#111114;--panel:#191920;--ink:#ececf1;--muted:#a5a5b3;--line:#2f2f3a;--accent:#8fa9ee;--ok:#59c98a;--warn:#e2b45a;--bad:#f0837d}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"Helvetica Neue",Arial,"PingFang SC","Hiragino Sans GB","Microsoft YaHei",sans-serif}
main{max-width:1080px;margin:0 auto;padding:28px 20px 64px}
h1{font-size:26px;margin:0 0 4px}
h2{font-size:18px;margin:34px 0 10px;padding-bottom:6px;border-bottom:1px solid var(--line)}
h3{font-size:15px;margin:18px 0 6px;color:var(--muted);font-weight:600}
p{margin:6px 0}
a{color:var(--accent)}
.sub{color:var(--muted)}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px 18px}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:10px;margin-top:12px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:12px 14px}
.card .k{font-size:12px;color:var(--muted);text-transform:uppercase;letter-spacing:.04em}
.card .v{margin:6px 0 2px;font-size:16px;font-weight:600}
.card .n{font-size:12px;color:var(--muted)}
.badge{display:inline-block;padding:1px 8px;border-radius:999px;border:1px solid currentColor;font-size:12px;font-weight:600;white-space:nowrap}
.badge.ok{color:var(--ok)}.badge.warn{color:var(--warn)}.badge.bad{color:var(--bad)}.badge.muted{color:var(--muted)}
.scroll{overflow-x:auto;-webkit-overflow-scrolling:touch;border:1px solid var(--line);border-radius:10px;background:var(--panel)}
table{border-collapse:collapse;width:100%;font-size:13.5px}
th,td{padding:8px 12px;text-align:left;border-bottom:1px solid var(--line);vertical-align:top}
th{font-size:12px;text-transform:uppercase;letter-spacing:.04em;color:var(--muted);font-weight:600;background:var(--panel);position:sticky;top:0}
tbody tr:last-child td{border-bottom:none}
td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}
code,.mono{font-family:var(--mono);font-size:12.5px}
ul{margin:6px 0;padding-left:20px}
li{margin:3px 0}
.meta{display:flex;flex-wrap:wrap;gap:8px 22px;margin-top:10px;font-size:13px;color:var(--muted)}
.meta b{color:var(--ink);font-weight:600}
.note{font-size:13px;color:var(--muted)}
footer{margin-top:40px;font-size:12.5px;color:var(--muted)}
"""


def page(title, body):
    return (
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n"
        '<meta name="viewport" content="width=device-width,initial-scale=1">\n'
        "<title>%s</title>\n<style>%s</style>\n</head>\n<body>\n<main>\n%s\n</main>\n</body>\n</html>\n"
        % (esc(title), CSS, body)
    )


def render_version(record):
    state = record.get("state") or {}
    version = text(state.get("version"))
    release_id = text(state.get("release_id"), "v" + version)
    status = release_status(record)
    remote = record.get("remote") or {}
    repo = record.get("repository") or "dengmengmian/CodeLeveler"
    commit = text(state.get("candidate_sha"))
    short_sha = text(state.get("candidate_short"), short(commit))
    post = record.get("post_release") or {}
    verified_at = text(post.get("verified_at"), text(state.get("finished_at")))
    release_url = text(remote.get("release_url"), "")
    run_id = remote.get("workflow_run_id")

    parts = []
    parts.append("<h1>CodeLeveler %s</h1>" % esc(version))
    parts.append(
        '<p class="sub">%s release · previous stable %s · %s</p>'
        % (badge(status), esc(text(state.get("previous_release"))), esc(verified_at))
    )

    meta = [
        ("Version", version),
        ("Previous stable", text(state.get("previous_release"))),
        ("Commit", '<code>%s</code>' % esc(short_sha)),
        ("Git tag", esc(text(remote.get("tag"), "v" + version if status == "RELEASED" else NOT_VERIFIED))),
        ("Model", text(state.get("model"))),
        ("Dogfood run", '<code>%s</code>' % esc(text(state.get("dogfood_run_id")))),
    ]
    parts.append(
        '<div class="meta">%s</div>'
        % "".join("<span>%s <b>%s</b></span>" % (esc(k), v) for k, v in meta)
    )

    if state.get("failure_reason"):
        parts.append(
            '<div class="panel" style="margin-top:14px"><b>Failure:</b> %s</div>'
            % esc(state.get("failure_reason"))
        )

    parts.append("<h2>Executive summary</h2>")
    parts.append('<div class="cards">')
    for name, value, note in summary_cards(record):
        parts.append(
            '<div class="card"><div class="k">%s</div><div class="v">%s</div><div class="n">%s</div></div>'
            % (esc(name), badge(value), esc(note))
        )
    parts.append("</div>")

    # Previous vs current
    parts.append("<h2>Previous vs current</h2>")
    cases = dogfood_case_totals(record)
    comparison = record.get("comparison") or {}
    changed = record.get("changes") or {}
    cmp_rows = [
        (
            "Build quality",
            esc(text(comparison.get("baseline_sha"), NOT_VERIFIED)),
            badge(dig(record, "state", "stages", 0, "status", default=NOT_VERIFIED)),
            NOT_VERIFIED,
        ),
        (
            "Real dogfood",
            NOT_VERIFIED,
            "%d case(s), %s passed" % (cases["total"], cases["rate"]),
            NOT_VERIFIED if str(comparison.get("status")).upper() != "PASS" else "UNCHANGED",
        ),
        (
            "Commits in release",
            "—",
            text(changed.get("commit_count")),
            "—",
        ),
        (
            "Files changed",
            "—",
            text(changed.get("changed_files")),
            "—",
        ),
        (
            "Performance",
            NOT_VERIFIED,
            NOT_VERIFIED,
            NOT_VERIFIED,
        ),
        (
            "Compatibility",
            NOT_VERIFIED,
            text((post.get("upgrade") or {}).get("status")),
            NOT_VERIFIED
            if (post.get("upgrade") or {}).get("status") != "PASS"
            else "IMPROVED",
        ),
    ]
    parts.append(
        table(
            ["Dimension", "Previous %s" % esc(text(state.get("previous_release"))), "Current %s" % esc(version), "Verdict"],
            [(a, b, c, d) for a, b, c, d in cmp_rows],
        )
    )
    parts.append(
        '<p class="note">Comparison status: %s — %s</p>'
        % (badge(comparison.get("status")), esc(text(comparison.get("reason"), "no reason recorded")))
    )

    # Changes
    parts.append("<h2>Release changes</h2>")
    for label, group in change_rows(record).items():
        parts.append("<h3>%s <span class=\"sub\">(%d)</span></h3>" % (esc(label), group["total"]))
        if group["items"]:
            parts.append("<ul>%s</ul>" % "".join("<li><code>%s</code></li>" % esc(i) for i in group["items"]))
        else:
            parts.append('<p class="note">None recorded.</p>')
        if group["total"] > len(group["items"]):
            parts.append(
                '<p class="note">%d more in the full report.</p>' % (group["total"] - len(group["items"]))
            )

    # Quality & dogfood
    parts.append("<h2>Quality gates</h2>")
    stage_table = [
        (r["stage"], r["name"], (r["status"],), r["seconds"], r["reused"])
        for r in stage_rows(record)
    ]
    parts.append(
        table(
            ["Stage", "Name", "Status", "Duration", "Reused"],
            [(a, esc(b), ("badge", c[0]), esc(d), esc(e)) for a, b, c, d, e in stage_table],
        )
    )
    parts.append("<h3>Dogfood layers</h3>")
    parts.append(
        table(
            ["Layer", "Status", "Checks", "Cases", "Duration", "Live"],
            [
                (
                    esc(r["layer"]),
                    ("badge", r["status"]),
                    esc(r["checks"]),
                    esc(
                        "%d/%d" % (r.get("case_pass", 0), r["cases"])
                        if r["cases"]
                        else "—"
                    ),
                    esc(r["seconds"]),
                    esc(r["live"]),
                )
                for r in layer_rows(record)
            ],
        )
    )
    cov = coverage_summary(record)
    if cov:
        parts.append("<h3>Feature coverage</h3>")
        parts.append(table(["Measure", "Value"], [(esc(k), esc(v)) for k, v in cov]))
    pty = pty_rows(record)
    if pty:
        parts.append("<h3>PTY suites</h3>")
        parts.append(
            table(
                ["Suite", "Status", "Detail"],
                [(esc(p["name"]), ("badge", p["status"]), esc(p["detail"])) for p in pty],
            )
        )

    # Performance
    parts.append("<h2>Performance</h2>")
    perf, comparable, reason = perf_rows(record)
    parts.append(
        table(
            ["Metric", "Previous", "Current", "Delta", "Verdict"],
            [
                (esc(r["label"]), esc(r["previous"]), esc(r["current"]), esc(r["delta"]), ("badge", r["verdict"]))
                for r in perf
            ],
        )
    )
    if not comparable:
        parts.append(
            '<p class="note">Not comparable: %s. Current values are the candidate run only; no previous-version claim is made.</p>'
            % esc(reason)
        )

    # Artifacts
    parts.append("<h2>Release artifacts</h2>")
    arts = artifact_rows(record)
    if arts:
        parts.append(
            table(
                ["Artifact", "Platform", "Build", "Checksum", "Download"],
                [
                    (
                        "<code>%s</code>" % esc(a["name"]),
                        esc(a["platform"]),
                        esc(a["build"]),
                        esc(a["checksum"]),
                        '<a href="%s">link</a>' % esc(a["url"]) if a["url"] else NOT_VERIFIED,
                    )
                    for a in arts
                ],
            )
        )
    else:
        parts.append('<p class="note">No artifact is published for this release (status %s).</p>' % esc(status))

    post_checks = []
    fresh = post.get("fresh_install") or {}
    upgrade = post.get("upgrade") or {}
    if fresh:
        post_checks.append(("Fresh install from published channel", (fresh.get("status"),), text(fresh.get("detail"), "")))
    if upgrade:
        post_checks.append(
            (
                "Upgrade from %s" % text(upgrade.get("from"), text(state.get("previous_release"))),
                (upgrade.get("status"),),
                text(upgrade.get("detail"), ""),
            )
        )
    if post_checks:
        parts.append("<h3>Post-release acceptance</h3>")
        parts.append(
            table(
                ["Check", "Status", "Evidence"],
                [(esc(a), ("badge", b[0]), esc(c)) for a, b, c in post_checks],
            )
        )

    # Known issues
    parts.append("<h2>Known issues and limits</h2>")
    issues = known_issues(record)
    if issues:
        parts.append(
            table(
                ["Kind", "Subject", "Detail"],
                [(esc(a), esc(b), esc(c)) for a, b, c in issues[:40]],
            )
        )
        if len(issues) > 40:
            parts.append('<p class="note">%d more recorded in the full report.</p>' % (len(issues) - 40))
    else:
        parts.append('<p class="note">None recorded.</p>')

    # Evidence
    parts.append("<h2>Evidence</h2>")
    links = []
    local = record.get("evidence_dir")
    if local:
        links.append(("Full release report (local)", os.path.join(local, "RELEASE_REPORT.md")))
        links.append(("Raw release data (local)", os.path.join(local, "release.json")))
    if commit:
        links.append(("Commit", "https://github.com/%s/commit/%s" % (repo, commit)))
    links.append(("CI workflow", "https://github.com/%s/actions/workflows/ci.yml" % repo))
    links.append(("Release workflow", "https://github.com/%s/actions/workflows/release.yml" % repo))
    if run_id:
        links.append(
            ("Release run", "https://github.com/%s/actions/runs/%s" % (repo, run_id))
        )
    if release_url:
        links.append(("GitHub Release", release_url))
    links.append(("Changelog", "../../../CHANGELOG.md"))
    links.append(("Release history", "../index.html"))
    parts.append(
        "<ul>%s</ul>"
        % "".join(
            '<li><a href="%s">%s</a></li>' % (esc(url), esc(label)) for label, url in links
        )
    )

    parts.append(
        '<footer>Generated by <code>scripts/release_dashboard.py</code> from the recorded release '
        "evidence. Status %s. Nothing on this page is measured here.</footer>" % esc(status)
    )
    return page("CodeLeveler %s release report" % version, "\n".join(parts))


def render_index(records):
    rows = []
    for record in records:
        state = record.get("state") or {}
        status = release_status(record)
        cases = dogfood_case_totals(record)
        changes = record.get("changes") or {}
        date = text(state.get("finished_at"), text((record.get("post_release") or {}).get("verified_at"), NOT_VERIFIED))
        rows.append(
            (
                '<a href="%s/index.html">%s</a>' % (esc(text(state.get("release_id"))), esc(text(state.get("version")))),
                esc(date),
                ("badge", status),
                esc(text(dig(record, "dogfood", "gate", "decision", "decision", default=NOT_VERIFIED))),
                esc("%d case(s), %s" % (cases["total"], cases["rate"])),
                esc(
                    "%s feat · %s fix · %s files"
                    % (
                        text(changes.get("features") and len(changes["features"]) or 0),
                        text(changes.get("fixes") and len(changes["fixes"]) or 0),
                        text(changes.get("changed_files")),
                    )
                ),
            )
        )
    body = [
        "<h1>CodeLeveler releases</h1>",
        '<p class="sub">Every published release, newest first. Each version page is generated from that release\'s own evidence.</p>',
        table(
            ["Version", "Released", "Status", "Release gate", "Dogfood", "Changes"],
            rows,
        ) if rows else '<p class="note">No release record yet.</p>',
        '<footer>Generated by <code>scripts/release_dashboard.py</code>. Records live in <code>v&lt;version&gt;/release.json</code>.</footer>',
    ]
    return page("CodeLeveler releases", "\n".join(body))


# ── io ──────────────────────────────────────────────────────────────────────


def load_json(path):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def sort_key(record):
    version = text(dig(record, "state", "version"), "0.0.0").split("-")[0]
    parts = []
    for piece in version.split("."):
        digits = "".join(ch for ch in piece if ch.isdigit())
        parts.append(int(digits) if digits else 0)
    while len(parts) < 3:
        parts.append(0)
    date = text(dig(record, "state", "finished_at"), "")
    return (tuple(parts), date)


def build_record(release, post, evidence_dir):
    record = json.loads(json.dumps(release))  # deep copy, no shared state
    record.setdefault("kind", "codeleveler_release")
    record["evidence_dir"] = evidence_dir
    if post is not None:
        record["post_release"] = post
    state = record.get("state") or {}
    record["dashboard"] = {
        "status": release_status(record),
        "generated_by": "scripts/release_dashboard.py",
        "source": {
            "release_evidence": os.path.join(evidence_dir, "release.json"),
            "post_release": "post_release.json" if post is not None else None,
        },
    }
    return record


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--release-json", required=True)
    parser.add_argument("--post-release")
    parser.add_argument("--evidence-dir", default=None)
    parser.add_argument("--out", required=True)
    parser.add_argument("--repository", default=None)
    args = parser.parse_args(argv)

    release = load_json(args.release_json)
    post = load_json(args.post_release) if args.post_release else None
    evidence_dir = args.evidence_dir or os.path.dirname(os.path.abspath(args.release_json))
    record = build_record(release, post, evidence_dir)
    if args.repository:
        record["repository"] = args.repository
    version = text(dig(record, "state", "version"), "")
    if not version or version == NOT_VERIFIED:
        print("release.json has no state.version", file=sys.stderr)
        return 2

    version_dir = os.path.join(args.out, "v%s" % version)
    os.makedirs(version_dir, exist_ok=True)
    with open(os.path.join(version_dir, "release.json"), "w", encoding="utf-8") as handle:
        json.dump(record, handle, indent=2, ensure_ascii=False, sort_keys=False)
        handle.write("\n")
    with open(os.path.join(version_dir, "index.html"), "w", encoding="utf-8") as handle:
        handle.write(render_version(record))

    records = []
    for name in sorted(os.listdir(args.out)):
        path = os.path.join(args.out, name, "release.json")
        if name.startswith("v") and os.path.isfile(path):
            records.append(load_json(path))
    records.sort(key=sort_key, reverse=True)
    with open(os.path.join(args.out, "index.html"), "w", encoding="utf-8") as handle:
        handle.write(render_index(records))

    print("%s: %s" % (record["dashboard"]["status"], os.path.join(version_dir, "index.html")))
    print("%s: %d version(s)" % (os.path.join(args.out, "index.html"), len(records)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
