#!/usr/bin/env python3
"""Real-PTY acceptance for the Skills surface.

Drives the REAL `leveler` binary on a real pseudo-terminal (in-process runtime,
real reducer, real renderer) with an isolated LEVELER_HOME and a fixture repo.
The provider is the local OpenAI-wire mock from `pty_dogfood.py`, so the exact
request the runtime sends is inspectable: a loaded Skill's procedure body must
be in the model request, not merely on screen.

Usage:
    python3 scripts/skills_pty_acceptance.py --only page
    python3 scripts/skills_pty_acceptance.py            # every mock scenario
    python3 scripts/skills_pty_acceptance.py --only real # real provider, one call
Evidence: target/skills-pty/.

A check passes only when the observable behavior is present on the terminal or
in the provider request. A refusal that is not on screen is a failure; a body
that only appears in the UI but not in the request is a failure.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts"))

import pty_dogfood as harness  # noqa: E402

BIN = harness.BIN
EVIDENCE = os.path.join(ROOT, "target", "skills-pty")
harness.TRANSPORT = "in-process"
harness.EVIDENCE = EVIDENCE

DEPLOY_BODY = "deploy-procedure-body-marker"
DEPLOY_TEXT = DEPLOY_BODY + "\n1. run the deploy checklist"
NO_USER_BODY = "no-user-procedure-body-marker"
NO_MODEL_BODY = "no-model-procedure-body-marker"
REAL_BODY = "real-model-procedure-body-marker"
REAL_ANSWER = "REAL-SKILL-MARKER"


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def skill_md(name: str, description: str, body: str, extra: str = "") -> str:
    return f"---\nname: {name}\ndescription: {description}\n{extra}---\n\n{body}\n"


def write_skill(root: str, name: str, contents: str, scope: str = "project") -> str:
    if scope == "project":
        directory = os.path.join(root, ".leveler", "skills", name)
    else:
        directory = os.path.join(root, "skills", name)
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, "SKILL.md")
    with open(path, "w") as fh:
        fh.write(contents)
    return path


def trust_allow(env: dict[str, str], repo: str):
    proc = subprocess.run(
        [BIN, "--repo", repo, "trust", "allow", "--yes"],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if proc.returncode != 0:
        raise SystemExit(f"trust allow failed: {proc.stderr}\n{proc.stdout}")


def load_skill_calls(home: str, name: str) -> list[str] | None:
    """Durable `load_skill` rows for `name` from the session event store.

    The rendered screen is not evidence by itself: the Skills guidance text
    mentions `load_skill`, so any string match can be a false positive. The
    event row records the tool the runtime actually executed and its
    arguments. Returns `None` when the store cannot be read at all.
    """
    projects = os.path.join(home, "state", "projects")
    if not os.path.isdir(projects):
        return None
    rows: list[str] = []
    try:
        for entry in os.listdir(projects):
            db = os.path.join(projects, entry, "sessions.db")
            if not os.path.isfile(db):
                continue
            proc = subprocess.run(
                [
                    "sqlite3",
                    db,
                    "SELECT payload FROM events WHERE type = 'tool_call_started'",
                ],
                capture_output=True,
                text=True,
                timeout=60,
            )
            if proc.returncode != 0:
                return None
            for line in proc.stdout.splitlines():
                try:
                    body = json.loads(line).get("payload", {})
                    if body.get("name") != "load_skill":
                        continue
                    if json.loads(body.get("arguments", "{}")).get("name") == name:
                        rows.append(line)
                except (json.JSONDecodeError, AttributeError):
                    continue
    except FileNotFoundError:
        return None
    return rows


def tool_event_rows(home: str, event_type: str, needle: str) -> list[str] | None:
    """Durable event payloads of one type whose text contains `needle`.

    Used as evidence for the real-provider scenario, where the rendered screen
    cannot distinguish a tool result from prose. `None` means the store could
    not be read at all.
    """
    projects = os.path.join(home, "state", "projects")
    if not os.path.isdir(projects):
        return None
    rows: list[str] = []
    try:
        for entry in os.listdir(projects):
            db = os.path.join(projects, entry, "sessions.db")
            if not os.path.isfile(db):
                continue
            proc = subprocess.run(
                [
                    "sqlite3",
                    db,
                    f"SELECT payload FROM events WHERE type = '{event_type}'",
                ],
                capture_output=True,
                text=True,
                timeout=60,
            )
            if proc.returncode != 0:
                return None
            rows.extend(line for line in proc.stdout.splitlines() if needle in line)
    except FileNotFoundError:
        return None
    return rows


def isolate(env: dict[str, str], root: str) -> dict[str, str]:
    """Keep the fixture's own user-scope skills out of the page."""
    user = os.path.join(root, "user")
    os.makedirs(user, exist_ok=True)
    env["HOME"] = user
    return env


def fixture(mock: harness.MockProvider, with_untrusted: bool = True):
    """One isolated home + repo, project skills approved, then one new
    unapproved package so the untrusted state is real, not simulated."""
    root, home, repo, env = harness.isolated_env("mock", mock)
    env = isolate(env, root)
    write_skill(
        repo,
        "deploy",
        skill_md(
            "deploy",
            "把当前分支部署到目标环境。",
            DEPLOY_TEXT,
            extra="argument-hint: <environment>\n",
        ),
    )
    write_skill(
        repo,
        "release-notes",
        skill_md("release-notes", "Draft release notes.", "release-notes-body"),
    )
    write_skill(
        repo,
        "no-user",
        skill_md(
            "no-user",
            "Model-only procedure.",
            NO_USER_BODY,
            extra="user-invocable: false\n",
        ),
    )
    write_skill(
        repo,
        "no-model",
        skill_md(
            "no-model",
            "Manual-only procedure.",
            NO_MODEL_BODY,
            extra="disable-model-invocation: true\n",
        ),
    )
    write_skill(
        repo,
        "help",
        skill_md("help", "A skill that shares the builtin name.", "help-skill-body"),
    )
    trust_allow(env, repo)
    if with_untrusted:
        write_skill(
            repo,
            "pending",
            skill_md("pending", "Not approved yet.", "pending-body"),
        )
    return root, home, repo, env


def second_repo(root: str, env: dict[str, str]) -> str:
    """A sibling project with the same-named skill, approved on its own."""
    repo = os.path.join(root, "repo-b")
    os.makedirs(os.path.join(repo, ".leveler"), exist_ok=True)
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "commit",
         "-q", "--allow-empty", "-m", "seed"],
        cwd=repo,
        check=False,
    )
    write_skill(
        repo,
        "deploy",
        skill_md("deploy", "Ship repo B safely.", "repo-b-body"),
    )
    trust_allow(env, repo)
    return repo


def disable_project_skill(home: str, repo: str, name: str):
    """Seed a project-scope disable the way the page writes it."""
    key = os.path.realpath(repo)
    path = os.path.join(home, "state", "skills.toml")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(f'[projects]\n"{key}" = ["{name}"]\n')


# ---------------------------------------------------------------------------
# Driving
# ---------------------------------------------------------------------------


def launch(env, repo, cols=120, rows=44, extra=None, permission="full-access") -> harness.Pty:
    argv = [
        BIN,
        "--repo",
        repo,
        "tui",
        "--auto-approve",
        "--in-process",
        "--permission",
        permission,
    ] + (extra or [])
    return harness.Pty(argv, env, cols, rows)


def wait_ready(pty: harness.Pty, timeout=90) -> bool:
    if pty.wait_for("Shift+Tab", timeout=timeout):
        pty.wait_stable(0.8, 20)
        return True
    return False


def screen(pty: harness.Pty) -> str:
    return pty.screen.text()


def rows(pty: harness.Pty) -> list[str]:
    return pty.screen.lines()


def row_index(pty: harness.Pty, needle: str):
    for index, line in enumerate(rows(pty)):
        if needle in line:
            return index
    return None


def has_user_message(pty: harness.Pty, message: str) -> bool:
    pattern = re.compile(r" *❯ ?" + re.escape(message) + r" *")
    return any(pattern.fullmatch(line) for line in rows(pty))


def row_count(pty: harness.Pty, needle: str) -> int:
    return sum(1 for line in rows(pty) if needle in line)


def row_text_col(pty: harness.Pty, row: int, needle: str) -> int:
    """Pointer column of `needle` on a painted row.

    A hosted page draws its list inside the card's content inset, so the
    card's border and left padding are not row hit areas. Aim the pointer at
    the row's own text instead of a fixed column that only matched one
    page-shell geometry.
    """
    line = rows(pty)[row]
    col = line.find(needle)
    return col if col >= 0 else 0


def clear(pty: harness.Pty):
    pty.key("\x15")
    pty.wait_stable(0.2, 5)


def send(pty: harness.Pty, text: str):
    pty.write(text)
    time.sleep(0.25)
    pty.key("\r")


def wait_turn(pty: harness.Pty, needle: str, timeout=90) -> bool:
    found = pty.wait_for(needle, timeout=timeout)
    pty.wait_stable(1.0, 30)
    return found


def open_page(pty: harness.Pty):
    clear(pty)
    send(pty, "/skills")
    pty.wait_stable(1.0, 30)


def evidence(report: harness.Report, name: str, pty: harness.Pty):
    report.dump(name, pty)
    with open(os.path.join(EVIDENCE, name + ".screen.txt"), "w") as fh:
        fh.write("\n".join(rows(pty)))
        fh.write("\n")


def requests(mock: harness.MockProvider) -> list[dict]:
    with mock.lock:
        bodies = list(mock.requests)
    out: list[dict] = []
    for body in bodies:
        try:
            data = json.loads(body)
        except Exception:
            continue
        if data.get("tools"):
            out.append(data)
    return out


def request_text(data: dict) -> str:
    return json.dumps(data.get("messages", []), ensure_ascii=False)


def last_request(mock: harness.MockProvider):
    found = requests(mock)
    return found[-1] if found else None


def slow_turn(text: str = "WORKING", frames: int = 40) -> list[str]:
    """A turn long enough for the PTY to type a second message while busy."""
    script = [
        json.dumps({"choices": [{"delta": {"content": text[i % len(text)]}}]})
        for i in range(frames)
    ]
    script.append(json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}]}))
    return script


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


def scenario_page(report: harness.Report):
    layer = "page"
    mock = harness.MockProvider()
    mock.assist_text = ""
    root, home, repo, env = fixture(mock)
    disable_project_skill(home, repo, "release-notes")
    # A directory the registry must reject, so the scan section and its
    # reason have a real subject.
    unusable = os.path.join(repo, ".leveler", "skills", "bad name")
    os.makedirs(unusable, exist_ok=True)
    with open(os.path.join(unusable, "SKILL.md"), "w") as fh:
        fh.write(skill_md("bad", "Unusable directory name.", "bad-body"))
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            evidence(report, "page-boot-failure", pty)
            return
        open_page(pty)
        evidence(report, "page-open", pty)

        text = screen(pty)
        report.check(layer, "page opens on /skills", "技能详情" not in text and "筛选" in text, text[:200])
        for needle in (
            "当前项目:",
            "/deploy",
            "/project:help",
            "/no-model",
            "/no-user",
            "/release-notes",
            "/pending",
            "/skill-creator",
        ):
            report.check(layer, f"lists {needle}", needle in text)
        report.check(layer, "model-only package is marked", "（仅模型可调用）" in text)
        report.check(layer, "untrusted package has a next step", "→ /trust" in text)
        report.check(layer, "argument hint is metadata-visible", "<environment>" in text)
        for section in ("可用", "已禁用", "未信任", "扫描问题"):
            report.check(layer, f"section {section}", section in text)
        report.check(
            layer,
            "an untrusted package is listed once, in its own section",
            row_count(pty, "pending") == 1,
            f"pending rows: {row_count(pty, 'pending')}",
        )
        disabled = row_index(pty, "已禁用")
        disabled_row = row_index(pty, "/release-notes")
        untrusted = row_index(pty, "未信任")
        untrusted_row = row_index(pty, "/pending")
        report.check(
            layer,
            "disabled skill sits under 已禁用",
            disabled is not None and disabled_row is not None and disabled < disabled_row,
            f"section={disabled} row={disabled_row}",
        )
        report.check(
            layer,
            "untrusted skill sits under 未信任",
            untrusted is not None and untrusted_row is not None and untrusted < untrusted_row,
            f"section={untrusted} row={untrusted_row}",
        )

        # A scan problem keeps its reason in the detail view (the absolute
        # path already fills the list row).
        pty.key("s")
        pty.write("bad name")
        pty.key("\r")
        pty.wait_stable(0.4, 10)
        # The scan section can sit below the fold at this height; the search
        # result is the layout-independent proof that the row exists.
        report.check(
            layer,
            "the unusable directory is a scan row",
            "bad name" in screen(pty),
            screen(pty)[:600],
        )
        pty.key("\r")
        pty.wait_stable(0.6, 10)
        evidence(report, "page-detail-scan-problem", pty)
        report.check(
            layer,
            "a scan problem explains its reason in detail",
            "contains whitespace" in screen(pty),
            screen(pty)[:600],
        )
        pty.key("\x1b", "\x1b")
        pty.wait_stable(0.4, 10)
        open_page(pty)

        # Detail of the first row (deploy).
        pty.key("\r")
        pty.wait_stable(0.8, 20)
        evidence(report, "page-detail-deploy", pty)
        detail = screen(pty)
        for needle in (
            "技能详情",
            "标识: project:deploy",
            "斜杠调用: /deploy",
            "内联提及: $deploy",
            "参数提示: <environment>",
            "用户可显式调用: 是",
            "模型可自动使用: 是",
        ):
            report.check(layer, f"deploy detail shows {needle!r}", needle in detail)

        # no-model's detail says the model may not load it.
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        pty.key("\x1b[B", "\x1b[B", "\r")
        pty.wait_stable(0.8, 20)
        evidence(report, "page-detail-no-model", pty)
        report.check(layer, "no-model detail shows 模型可自动使用: 否", "模型可自动使用: 否" in screen(pty))

        # no-user's detail says the person may not invoke it.
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        pty.key("\x1b[B", "\r")
        pty.wait_stable(0.8, 20)
        evidence(report, "page-detail-no-user", pty)
        report.check(layer, "no-user detail shows 用户可显式调用: 否", "用户可显式调用: 否" in screen(pty))

        # Filter cycles: All -> Available -> Disabled.
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        pty.key("f")
        pty.wait_stable(0.5, 10)
        report.check(layer, "filter 可用 hides the disabled skill", "筛选: 可用" in screen(pty) and "/release-notes" not in screen(pty))
        pty.key("f")
        pty.wait_stable(0.5, 10)
        evidence(report, "page-filter-disabled", pty)
        report.check(
            layer,
            "filter 已禁用 keeps the disabled skill and hides /deploy",
            "筛选: 已禁用" in screen(pty)
            and "/release-notes" in screen(pty)
            and "/deploy" not in screen(pty),
            screen(pty)[:300],
        )

        # The filter is a durable view preference (open() keeps it), so return
        # it to 全部 before searching for a name outside the current section.
        for _ in range(3):
            pty.key("f")
        pty.wait_stable(0.5, 10)

        # Search: reopen fresh, then query a name.
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        open_page(pty)
        pty.key("s")
        pty.write("no-model")
        pty.key("\r")
        pty.wait_stable(0.6, 10)
        evidence(report, "page-search", pty)
        search = screen(pty)
        report.check(
            layer,
            "search narrows to /no-model",
            "搜索: no-model" in search and "/no-model" in search and "/deploy" not in search,
            search[:300],
        )

        # Toggle: a durable write, visible after reopening the page.
        pty.key("\x1b", "\x1b")
        pty.wait_stable(0.4, 10)
        open_page(pty)
        pty.key(" ")
        pty.wait_stable(1.0, 30)
        evidence(report, "page-toggle-disabled", pty)
        toggled = screen(pty)
        preferences = os.path.join(home, "state", "skills.toml")
        written = open(preferences).read() if os.path.exists(preferences) else ""
        report.check(layer, "space disables /deploy with a notice", "已禁用 /deploy" in toggled, toggled[:300])
        report.check(
            layer,
            "the preference names this project's canonical root",
            leveler_project_key(repo) in written and '"deploy"' in written,
            written,
        )
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        open_page(pty)
        reopened = screen(pty)
        report.check(
            layer,
            "reopening the page shows the persisted disable",
            row_index(pty, "已禁用") is not None
            and row_index(pty, "/deploy") is not None
            and row_index(pty, "已禁用") < row_index(pty, "/deploy"),
            reopened[:400],
        )
        # Re-enable the row that was just disabled. Reopening the page resets
        # the cursor to the first row, and the disabled section now starts the
        # list, so filter to 已禁用 first instead of assuming /deploy is first.
        for _ in range(5):
            if "筛选: 已禁用" in screen(pty):
                break
            pty.key("f")
            pty.wait_stable(0.4, 10)
        report.check(layer, "the disabled filter leads with /deploy", "筛选: 已禁用" in screen(pty) and "/deploy" in screen(pty))
        pty.key(" ")
        pty.wait_stable(1.0, 30)
        written = open(preferences).read() if os.path.exists(preferences) else ""
        report.check(layer, "space again re-enables /deploy", "已启用 /deploy" in screen(pty), screen(pty)[:300])
        report.check(layer, "re-enable removes the disable", '"deploy"' not in written, written)

        # The filter is a durable view preference. Return it to 全部, otherwise
        # the pending search below is filtered out before it can be shown.
        for _ in range(5):
            if "筛选: 全部" in screen(pty):
                break
            pty.key("f")
            pty.wait_stable(0.4, 10)

        # An untrusted row's toggle explains /trust and writes nothing.
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        clear(pty)
        send(pty, "/skills pending")
        pty.wait_stable(0.8, 20)
        pty.key(" ")
        pty.wait_stable(0.6, 10)
        evidence(report, "page-toggle-untrusted", pty)
        report.check(
            layer,
            "an untrusted toggle points at /trust",
            "未信任的包不会被加载" in screen(pty) and "/trust" in screen(pty),
            screen(pty)[:300],
        )
        report.check(
            layer,
            "an untrusted toggle writes no preference",
            '"pending"' not in (open(preferences).read() if os.path.exists(preferences) else ""),
        )
    finally:
        pty.close()


def leveler_project_key(repo: str) -> str:
    return os.path.realpath(repo)


def scenario_isolation(report: harness.Report):
    layer = "isolation"
    mock = harness.MockProvider()
    mock.assist_text = ""
    root, home, repo_a, env = fixture(mock, with_untrusted=False)
    repo_b = second_repo(root, env)
    disable_project_skill(home, repo_a, "deploy")

    for label, repo, expected_state in (("repo-b", repo_b, "available"),):
        pty = launch(env, repo, cols=110, rows=36)
        try:
            if not wait_ready(pty):
                report.block(layer, f"{label} boot", "footer never appeared")
                return
            open_page(pty)
            evidence(report, f"isolation-{label}", pty)
            text = screen(pty)
            check = "/deploy" in text and "已禁用" not in text
            report.check(
                layer,
                "a same-named skill in another project is still available",
                check,
                text[:400],
            )
        finally:
            pty.close()

    preferences = open(os.path.join(home, "state", "skills.toml")).read()
    report.check(
        layer,
        "the preference names repo A, not repo B",
        leveler_project_key(repo_a) in preferences
        and leveler_project_key(repo_b) not in preferences,
        preferences,
    )


def scenario_completion(report: harness.Report):
    layer = "completion"
    mock = harness.MockProvider()
    mock.assist_text = ""
    root, home, repo, env = fixture(mock, with_untrusted=False)
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        pty.write("/dep")
        pty.wait_stable(0.8, 20)
        evidence(report, "completion-skill-prefix", pty)
        text = screen(pty)
        report.check(layer, "/dep lists /deploy", "/deploy" in text)
        report.check(layer, "the skill row names its source", "项目·CodeLeveler" in text, text[-900:])
        report.check(layer, "the skill row shows the argument hint", "参数提示: <environment>" in text)

        pty.key("\t")
        pty.wait_stable(0.6, 10)
        evidence(report, "completion-tab", pty)
        after_tab = screen(pty)
        report.check(layer, "Tab completes the skill name", "/deploy" in after_tab)
        report.check(layer, "Tab closes the skill popup", "参数提示: <environment>" not in after_tab)

        clear(pty)
        pty.write("/help")
        pty.wait_stable(0.7, 10)
        evidence(report, "completion-help-both", pty)
        text = screen(pty)
        builtin = row_index(pty, "/help")
        project = row_index(pty, "/project:help")
        report.check(layer, "/help lists the built-in and the project skill", builtin is not None and project is not None, text[-900:])
        report.check(
            layer,
            "commands come before skills on the shared surface",
            builtin is not None and project is not None and builtin < project,
            f"builtin={builtin} project={project}",
        )

        clear(pty)
        pty.write("/")
        pty.wait_stable(0.7, 10)
        evidence(report, "completion-bare-slash", pty)
        text = screen(pty)
        report.check(layer, "bare / keeps the built-in quick list", "/model" in text and "/goal" in text, text[-700:])
        report.check(
            layer,
            "bare / lists callable Skills in the same menu",
            "/deploy" in text and "项目·CodeLeveler" in text,
            text[-700:],
        )
        report.check(
            layer,
            "bare / keeps a model-only Skill out of the user menu",
            "/no-user" not in text,
            "bare slash offered a user-invocation-disabled skill",
        )
        selected_before = row_index(pty, "│❯/")
        pty.key("\x1b[B")
        pty.wait_stable(0.3, 10)
        selected_down = row_index(pty, "│❯/")
        report.check(
            layer,
            "Down moves the completion highlight",
            selected_before is not None
            and selected_down is not None
            and selected_down == selected_before + 1,
            f"before={selected_before} after={selected_down}",
        )
        pty.key("\x1b[A")
        pty.wait_stable(0.3, 10)
        report.check(
            layer,
            "Up returns the completion highlight",
            row_index(pty, "│❯/") == selected_before,
            f"before={selected_before} back={row_index(pty, '│❯/')}",
        )

        clear(pty)
        pty.write("/dpl")
        pty.wait_stable(0.7, 10)
        evidence(report, "completion-fuzzy-skill", pty)
        report.check(
            layer,
            "a fuzzy query finds the Skill by subsequence",
            "/deploy" in screen(pty),
            screen(pty)[-600:],
        )

        clear(pty)
        pty.write("/mdl")
        pty.wait_stable(0.7, 10)
        evidence(report, "completion-fuzzy-command", pty)
        report.check(
            layer,
            "a fuzzy query finds a built-in command",
            "/model" in screen(pty),
            screen(pty)[-600:],
        )

        clear(pty)
        pty.write("/pro")
        pty.wait_stable(0.7, 10)
        evidence(report, "completion-reserved", pty)
        report.check(
            layer,
            "a reserved name is advertised as scope:name",
            "/project:help" in screen(pty),
            screen(pty)[-600:],
        )
    finally:
        pty.close()


def scenario_invoke(report: harness.Report):
    layer = "invoke"
    mock = harness.MockProvider()
    mock.assist_text = ""
    mock.turns = [
        harness.text_turn("DEPLOY-OK"),
        harness.text_turn("NOMODEL-OK"),
        harness.text_turn("MENTION-OK"),
    ]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        send(pty, "/deploy staging")
        report.check(layer, "the explicit turn settles", wait_turn(pty, "DEPLOY-OK"))
        evidence(report, "invoke-deploy", pty)
        text = screen(pty)
        report.check(layer, "the user message stays verbatim", has_user_message(pty, "/deploy staging"), text[:400])
        report.check(layer, "the loaded skill is reported", "技能 /deploy 已加载" in text)
        requests_so_far = requests(mock)
        report.check(layer, "exactly one agent request ran", len(requests_so_far) == 1, f"{len(requests_so_far)}")
        report.check(
            layer,
            "the procedure body reached the model request",
            bool(requests_so_far) and DEPLOY_BODY in request_text(requests_so_far[0]),
        )
        report.check(
            layer,
            "the model saw the user's original message",
            bool(requests_so_far) and "/deploy staging" in request_text(requests_so_far[0]),
        )

        send(pty, "/no-model")
        report.check(layer, "the model-only turn settles", wait_turn(pty, "NOMODEL-OK"))
        evidence(report, "invoke-no-model", pty)
        latest = last_request(mock)
        report.check(
            layer,
            "an explicit call loads a disable-model-invocation skill",
            latest is not None and NO_MODEL_BODY in request_text(latest),
        )
        report.check(layer, "the load is reported", "技能 /no-model 已加载" in screen(pty))

        send(pty, "$deploy now")
        report.check(layer, "the mention turn settles", wait_turn(pty, "MENTION-OK"))
        evidence(report, "invoke-mention", pty)
        latest = last_request(mock)
        report.check(
            layer,
            "a $mention loads the procedure",
            latest is not None and DEPLOY_BODY in request_text(latest),
        )
        report.check(
            layer,
            "a successful mention adds no extra note",
            "技能 $deploy 已加载" not in screen(pty),
            screen(pty)[-500:],
        )
        report.check(layer, "three explicit inputs made three agent requests", len(requests(mock)) == 3, f"{len(requests(mock))}")
    finally:
        pty.close()
        mock.stop()


def scroll_until(pty, needle: str, timeout: float = 30.0) -> bool:
    """Poll the live screen for a transient row (a notice, a dialog) instead
    of judging the settled screen after it has been replaced."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in screen(pty):
            return True
        if not pty.alive():
            return False
        time.sleep(0.1)
    return False


def scenario_trust(report: harness.Report):
    """Assisted mode asks about the exact bytes; the answer is the runtime's
    structured decision, never a text hint. Full is covered by `refusal`."""
    layer = "trust"
    mock = harness.MockProvider()
    mock.assist_text = ""
    mock.turns = [harness.text_turn("TRUST-DONE"), harness.text_turn("DENY-NOT-SENT")]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    write_skill(
        repo,
        "pending",
        skill_md("pending", "Not approved yet.", "pending-body-marker"),
    )
    write_skill(
        repo,
        "pending-deny",
        skill_md("pending-deny", "Also not approved.", "pending-deny-body"),
    )
    pty = launch(env, repo, permission="assisted")
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        send(pty, "/pending")
        report.check(
            layer,
            "an untrusted explicit skill opens the authorization dialog",
            scroll_until(pty, "Skill 授权"),
            screen(pty)[-900:],
        )
        dialog = screen(pty)
        evidence(report, "trust-request", pty)
        report.check(layer, "the dialog names the package", "pending" in dialog, dialog[-900:])
        report.check(
            layer,
            "the dialog states the body is withheld",
            "批准前不会加载正文" in dialog,
            dialog[-900:],
        )
        report.check(
            layer,
            "the narrowest grant is offered",
            "仅本次允许" in dialog and "信任当前内容版本" in dialog and "拒绝使用" in dialog,
            dialog[-900:],
        )
        pty.key("\r")
        report.check(layer, "the approved turn settles", wait_turn(pty, "TRUST-DONE"))
        evidence(report, "trust-allowed", pty)
        found = requests(mock)
        report.check(layer, "approval starts the model turn once", len(found) == 1, f"{len(found)}")
        report.check(
            layer,
            "the approved procedure reaches the model",
            bool(found) and "pending-body-marker" in request_text(found[-1]),
        )
        report.check(
            layer,
            "the user's literal message is preserved",
            bool(found) and "/pending" in request_text(found[-1]),
        )

        clear(pty)
        send(pty, "/pending-deny")
        report.check(
            layer,
            "a second untrusted skill asks again",
            scroll_until(pty, "Skill 授权"),
            screen(pty)[-700:],
        )
        pty.key("\x1b")
        report.check(
            layer,
            "denial returns the draft",
            scroll_until(pty, "› /pending-deny"),
            screen(pty)[-700:],
        )
        evidence(report, "trust-denied", pty)
        text = screen(pty)
        report.check(
            layer,
            "denial says the message was not accepted",
            "消息未被接受" in text,
            text[-700:],
        )
        found = requests(mock)
        report.check(layer, "denial opened no model turn", len(found) == 1, f"{len(found)}")
        report.check(
            layer,
            "the denied body was never sent",
            all("pending-deny-body" not in request_text(data) for data in found),
        )
    finally:
        pty.close()
        mock.stop()


def scenario_refusal(report: harness.Report):
    """Full mode never adds an approval dialog. Requests that cannot load
    locally keep the draft and open no turn; an unapproved package created
    after the session baseline still starts its turn, but its body is refused
    and the runtime says why."""
    layer = "refusal"
    mock = harness.MockProvider()
    mock.assist_text = ""
    mock.turns = [harness.text_turn("PENDING-DONE")]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    disable_project_skill(home, repo, "release-notes")
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        cases = [
            ("/release-notes", "已禁用", "page-disabled skill"),
            ("/no-user", "仅模型可调用", "user-invocation-disabled skill"),
            ("/nonexistent", "未知命令", "unknown slash command"),
        ]
        for command, reason, label in cases:
            clear(pty)
            before = len(requests(mock))
            send(pty, command)
            pty.wait_stable(0.7, 15)
            evidence(report, "refusal-" + command.strip("/"), pty)
            text = screen(pty)
            report.check(layer, f"{label} is refused with a reason", reason in text, text[-600:])
            report.check(layer, f"{label} keeps the draft", f"› {command}" in text, text[-400:])
            report.check(
                layer,
                f"{label} opened no model turn",
                len(requests(mock)) == before,
                f"{len(requests(mock)) - before} agent requests",
            )

        # A package written after the session started is outside the Full
        # baseline, so it is not adopted. Full still asks nothing: the turn
        # runs and the delivery audit names the refusal.
        write_skill(
            repo,
            "pending",
            skill_md("pending", "Not approved yet.", "pending-body-marker"),
        )
        clear(pty)
        pty.write("/")
        pty.wait_stable(0.4, 10)
        pty.key("\x15")
        pty.wait_stable(0.2, 5)
        note_seen = False
        send(pty, "/pending")
        deadline = time.time() + 90
        while time.time() < deadline:
            text = screen(pty)
            if "技能 /pending 未加载" in text and "未信任" in text:
                note_seen = True
            if "PENDING-DONE" in text:
                break
            if not pty.alive():
                break
            time.sleep(0.1)
        pty.wait_stable(1.0, 30)
        evidence(report, "refusal-pending", pty)
        text = screen(pty)
        report.check(
            layer,
            "Full shows no trust dialog for an untrusted skill",
            "Skill 授权" not in text and "仅本次允许" not in text,
            text[-700:],
        )
        report.check(
            layer,
            "the untrusted procedure is reported as not loaded",
            note_seen and "未信任" in text,
            text[-900:],
        )
        found = requests(mock)
        report.check(layer, "only the untrusted turn reached the model", len(found) == 1, f"{len(found)}")
        report.check(
            layer,
            "the untrusted body was never sent",
            all("pending-body-marker" not in request_text(data) for data in found),
        )
    finally:
        pty.close()
        mock.stop()


def scenario_mid_turn(report: harness.Report):
    layer = "mid-turn"
    mock = harness.MockProvider()
    mock.assist_text = ""
    mock.stream_delay = 0.12
    mock.turns = [slow_turn()]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        send(pty, "start a long turn")
        time.sleep(1.2)
        send(pty, "/deploy staging")
        pty.wait_stable(0.6, 10)
        evidence(report, "mid-turn-refusal", pty)
        text = screen(pty)
        report.check(
            layer,
            "a mid-turn skill request is refused explicitly",
            "需要加载技能指令" in text and "草稿已保留" in text,
            text[-700:],
        )
        report.check(layer, "the draft is kept in the composer", "/deploy staging" in text, text[-400:])

        report.check(layer, "the long turn settles", wait_turn(pty, "WORKING", timeout=120))
        found = requests(mock)
        report.check(layer, "only the first turn reached the model", len(found) == 1, f"{len(found)}")
        report.check(
            layer,
            "the refused procedure was never delivered",
            all(DEPLOY_BODY not in request_text(data) for data in found),
        )
    finally:
        pty.close()
        mock.stop()


def scenario_sizes(report: harness.Report):
    layer = "sizes"
    mock = harness.MockProvider()
    mock.assist_text = ""
    root, home, repo, env = fixture(mock, with_untrusted=False)
    pty = launch(env, repo, cols=60, rows=30)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        open_page(pty)
        evidence(report, "sizes-60x30-page", pty)
        text = screen(pty)
        report.check(layer, "60x30: the page still renders", "筛选" in text and "/deploy" in text, text[:400])
        pty.key("\r")
        pty.wait_stable(0.7, 10)
        evidence(report, "sizes-60x30-detail", pty)
        report.check(layer, "60x30: detail keeps the full hint", "参数提示: <environment>" in screen(pty))
        pty.key("\x1b", "\x1b")
        pty.wait_stable(0.4, 10)

        pty.write("/dep")
        pty.wait_stable(0.7, 10)
        evidence(report, "sizes-60x30-completion", pty)
        text = screen(pty)
        report.check(layer, "60x30: completion keeps the name and hint label", "/deploy" in text and "参数提示:" in text, text[-600:])

        pty.resize(120, 44)
        pty.wait_stable(0.8, 20)
        open_page(pty)
        evidence(report, "sizes-120x44-page", pty)
        report.check(
            layer,
            "wider terminal keeps the CJK description",
            "把当前分支部署到目标环境。" in screen(pty),
            screen(pty)[:800],
        )

        pty.resize(80, 24)
        pty.wait_stable(0.8, 20)
        evidence(report, "sizes-80x24-page", pty)
        text = screen(pty)
        report.check(layer, "80x24 keeps the page usable", "筛选" in text and "/deploy" in text, text[:400])
        report.check(layer, "no escape sequence leaked into the screen", "\x1b" not in text)
    finally:
        pty.close()
        mock.stop()


def scenario_interactions(report: harness.Report):
    """Requirement 3: the page's real mouse, wheel, detail, resize and
    duplicate-source behaviour on a live PTY."""
    layer = "page-interactions"
    mock = harness.MockProvider()
    mock.assist_text = ""
    mock.turns = [harness.text_turn("INTERACTION-OK")]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    long_description = (
        "这是一段很长的中文技能描述，用来验证窄终端里的换行、详情中的完整显示，"
        "以及列表不会被描述挤爆。" * 3
    )
    write_skill(repo, "chinese-long", skill_md("chinese-long", long_description, "chinese-long-body"))
    # A lower-priority user definition with the same name as the project
    # /deploy: once approved it is a real shadowed source for the detail.
    user_deploy = os.path.join(home, "skills", "deploy")
    os.makedirs(user_deploy, exist_ok=True)
    with open(os.path.join(user_deploy, "SKILL.md"), "w") as fh:
        fh.write(skill_md("deploy", "User-scope duplicate.", "user-deploy-body"))
    trust_allow(env, repo)

    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        open_page(pty)
        text = screen(pty)
        report.check(layer, "the long CJK row renders", "/chinese-long" in text, text[:600])
        report.check(
            layer,
            "a duplicate source keeps exactly one winning row",
            row_count(pty, "/deploy") == 1,
            f"/deploy rows: {row_count(pty, '/deploy')}",
        )

        # A click selects the row under the pointer.
        target = row_index(pty, "/deploy")
        report.check(layer, "the deploy row is on screen", target is not None)
        if target is None:
            return
        pty.click(target, row_text_col(pty, target, "/deploy"))
        pty.wait_stable(0.4, 10)
        evidence(report, "interactions-click-select", pty)
        clicked = rows(pty)[target]
        report.check(
            layer,
            "a click moves the selection marker to that row",
            "›" in clicked and "/deploy" in clicked,
            clicked,
        )

        pty.key("\r")
        pty.wait_stable(0.6, 15)
        evidence(report, "interactions-duplicate-detail", pty)
        detail = screen(pty)
        report.check(layer, "the clicked row opens its detail", "标识: project:deploy" in detail, detail[:700])
        report.check(
            layer,
            "the detail names the shadowed user definition",
            "被遮蔽的定义" in detail and "user:deploy" in detail,
            detail[:900],
        )

        # The wheel is the list's selection movement.
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        before = row_index(pty, "›")
        wheel_col = row_text_col(pty, before if before is not None else 8, "›")
        pty.wheel(False, row=(before or 8) + 1, col=wheel_col)
        pty.wait_stable(0.4, 10)
        after = row_index(pty, "›")
        report.check(
            layer,
            "one wheel notch moves the selection",
            after is not None and after != before,
            f"before={before} after={after}",
        )
        pty.wheel(True, row=(after or 8) + 1, col=wheel_col)
        pty.wait_stable(0.4, 10)
        report.check(
            layer,
            "wheel up returns the selection",
            row_index(pty, "›") == before,
            f"before={before} back={row_index(pty, '›')}",
        )

        # Long CJK description: full in the detail, then survive two resizes.
        chinese = row_index(pty, "/chinese-long")
        report.check(layer, "the long-description row is on screen", chinese is not None)
        if chinese is None:
            return
        pty.click(chinese, row_text_col(pty, chinese, "/chinese-long"))
        pty.wait_stable(0.3, 10)
        pty.key("\r")
        pty.wait_stable(0.6, 15)
        evidence(report, "interactions-long-cjk-detail", pty)
        detail = screen(pty)
        report.check(
            layer,
            "the detail keeps the long CJK description",
            "标识: project:chinese-long" in detail and "很长的中文技能描述" in detail,
            detail[:900],
        )
        pty.resize(60, 30)
        pty.wait_stable(0.8, 20)
        evidence(report, "interactions-narrow-detail", pty)
        report.check(
            layer,
            "a narrow terminal keeps the detail readable",
            "技能详情" in screen(pty) and "chinese-long" in screen(pty),
            screen(pty)[:500],
        )
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)
        evidence(report, "interactions-narrow-list", pty)
        report.check(
            layer,
            "the narrow list still names the skills",
            "/deploy" in screen(pty) and "/chinese-long" in screen(pty),
            screen(pty)[:500],
        )
        pty.resize(120, 44)
        pty.wait_stable(0.8, 20)
        evidence(report, "interactions-widened-list", pty)
        report.check(layer, "widening the terminal keeps the page", "/deploy" in screen(pty), screen(pty)[:500])
        pty.key("\x1b")
        pty.wait_stable(0.4, 10)

        # A turn's delivery fact appears once; it does not become page rows
        # or crowd the page chrome.
        send(pty, "/deploy staging")
        report.check(layer, "the delivery turn settles", wait_turn(pty, "INTERACTION-OK"))
        found = requests(mock)
        report.check(
            layer,
            "an explicit name resolves to the higher-priority package",
            bool(found)
            and "deploy-procedure-body-marker" in request_text(found[-1])
            and "user-deploy-body" not in request_text(found[-1]),
            "the request used the shadowed user copy",
        )
        evidence(report, "interactions-delivery-note", pty)
        report.check(
            layer,
            "the delivery is reported once",
            row_count(pty, "技能 /deploy 已加载") == 1,
            f"notes: {row_count(pty, '技能 /deploy 已加载')}",
        )
        clear(pty)
        send(pty, "/skills")
        pty.wait_stable(0.8, 20)
        evidence(report, "interactions-delivery-on-page", pty)
        page_text = screen(pty)
        report.check(
            layer,
            "the delivery fact is not repeated as page rows",
            row_count(pty, "技能 /deploy 已加载") <= 1,
            f"notes on page: {row_count(pty, '技能 /deploy 已加载')}",
        )
        report.check(
            layer,
            "the page keeps its own footer hint",
            "Esc 返回" in page_text,
            page_text[-300:],
        )
    finally:
        pty.close()
        mock.stop()


def list_sessions(env, repo) -> list[dict]:
    proc = subprocess.run(
        [BIN, "--repo", repo, "sessions", "list"],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if proc.returncode != 0:
        raise SystemExit(f"sessions list failed: {proc.stderr}\n{proc.stdout}")
    found = []
    for line in proc.stdout.splitlines():
        match = re.search(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})", line)
        if match:
            found.append({"id": match.group(1), "line": line})
    return found


def scenario_resume(report: harness.Report):
    layer = "resume"
    mock = harness.MockProvider()
    mock.assist_text = ""
    mock.turns = [harness.text_turn("BEFORE-RESUME"), harness.text_turn("AFTER-RESUME")]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        send(pty, "/deploy staging")
        report.check(layer, "the first session settles", wait_turn(pty, "BEFORE-RESUME"))
        evidence(report, "resume-before", pty)
    finally:
        pty.close()

    sessions = list_sessions(env, repo)
    if not sessions:
        report.block(layer, "session id", "sessions list named no session")
        return
    session_id = sessions[0]["id"]
    with open(os.path.join(EVIDENCE, "resume-sessions.txt"), "w") as fh:
        fh.write("\n".join(s["line"] for s in sessions))

    pty = launch(env, repo, extra=["--session", session_id])
    try:
        if not wait_ready(pty):
            report.block(layer, "resumed TUI boot", "footer never appeared")
            return
        replayed = pty.wait_for("/deploy staging", timeout=30)
        pty.wait_stable(1.0, 20)
        evidence(report, "resume-reopened", pty)
        report.check(layer, "the resumed session replays the user's message", replayed and has_user_message(pty, "/deploy staging"), screen(pty)[:600])
        report.check(layer, "the resumed session replays the answer", "BEFORE-RESUME" in screen(pty))

        send(pty, "/deploy again")
        report.check(layer, "the resumed session accepts a new turn", wait_turn(pty, "AFTER-RESUME"))
        evidence(report, "resume-after-turn", pty)
        latest = last_request(mock)
        report.check(
            layer,
            "skill delivery still works after resume",
            latest is not None and DEPLOY_BODY in request_text(latest),
            "no body in the resumed request",
        )
        report.check(layer, "resume did not duplicate the first turn", len(requests(mock)) == 2, f"{len(requests(mock))}")
    finally:
        pty.close()
        mock.stop()


def scenario_compact(report: harness.Report):
    layer = "compact"
    mock = harness.MockProvider()
    # Compaction is a tool-less non-streaming call; the mock's assist lane is
    # the only shape that answers it. The summary text is the observable.
    mock.assist_text = "COMPACT-SUMMARY-SENTINEL"
    mock.turns = [
        harness.text_turn("ONE"),
        harness.text_turn("TWO"),
        harness.text_turn("AFTER-COMPACT"),
    ]
    root, home, repo, env = fixture(mock, with_untrusted=False)
    pty = launch(env, repo)
    try:
        if not wait_ready(pty):
            report.block(layer, "TUI boot", "footer never appeared")
            return
        send(pty, "first message")
        report.check(layer, "the first turn settles", wait_turn(pty, "ONE"))
        send(pty, "second message")
        report.check(layer, "the second turn settles", wait_turn(pty, "TWO"))

        send(pty, "/compact")
        pty.wait_for("上下文已压缩", timeout=30)
        pty.wait_stable(1.0, 30)
        evidence(report, "compact-result", pty)
        text = screen(pty)
        if "对话较短" in text:
            report.block(layer, "compaction", "two turns produced too few messages to compact")
            return
        report.check(
            layer,
            "compaction reports the new context size",
            "上下文已压缩" in text,
            text[-600:],
        )

        send(pty, "/deploy after compact")
        report.check(layer, "a turn after compaction settles", wait_turn(pty, "AFTER-COMPACT"))
        evidence(report, "compact-after-turn", pty)
        latest = last_request(mock)
        report.check(
            layer,
            "skill delivery still works after compaction",
            latest is not None and DEPLOY_BODY in request_text(latest),
            "the post-compaction request carried no procedure body",
        )
    finally:
        pty.close()
        mock.stop()


def scenario_real_discovery(report: harness.Report):
    """One real provider call chain where the MODEL finds a Skill from its
    description and opens it with `load_skill`.

    The proof of the load is the structured tool row in the transcript and the
    provider's follow-up request, not the model's prose. The final marker is
    the task completing after the procedure was loaded; a missing tool row is
    reported as not verified, never as a pass.
    """
    layer = "real-discovery"
    source = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(source):
        report.block(layer, "real provider", "~/.leveler/config.toml is absent")
        return
    root, home, repo, env = harness.isolated_env("real", None)
    env = isolate(env, root)
    write_skill(
        repo,
        "discover-me",
        skill_md(
            "discover-me",
            "Use when the user asks for the DISCOVERY-MARKER verification.",
            "DISCOVERY-BODY-SENTINEL\n\nTo complete the task, reply with exactly "
            "DISCOVERY-OK and do not call any more tools.",
        ),
    )
    trust_allow(env, repo)

    pty = launch(env, repo, cols=110, rows=36)
    try:
        if not wait_ready(pty):
            report.block(layer, "real TUI boot", "footer never appeared")
            return
        send(
            pty,
            "Enable the Skills capability if you need it, find the project Skill "
            "whose description matches the DISCOVERY-MARKER verification, load it, "
            "and follow its procedure.",
        )
        answered = False
        deadline = time.time() + 300
        while time.time() < deadline:
            text = screen(pty)
            if "DISCOVERY-OK" in text:
                answered = True
                break
            if not pty.alive():
                break
            time.sleep(0.2)
        pty.wait_stable(1.5, 60)
        evidence(report, "real-discovery", pty)
        report.check(
            layer,
            "the task completed after the procedure was loaded",
            answered,
            screen(pty)[-900:],
        )
    finally:
        pty.close()
    calls = load_skill_calls(home, "discover-me")
    finished = tool_event_rows(home, "tool_call_finished", "DISCOVERY-BODY-SENTINEL")
    if calls is not None:
        with open(os.path.join(EVIDENCE, "real-discovery-load-skill.jsonl"), "w") as fh:
            fh.write("\n".join(calls) + "\n")
    if finished is not None:
        with open(os.path.join(EVIDENCE, "real-discovery-load-skill-finished.jsonl"), "w") as fh:
            fh.write("\n".join(finished) + "\n")
    if calls is None:
        report.block(
            layer,
            "durable load_skill evidence",
            "the session event store could not be read",
        )
        return
    report.check(
        layer,
        "the model called load_skill for the discovered Skill",
        bool(calls),
        "no load_skill event row named discover-me",
    )


def scenario_real(report: harness.Report):
    """One real provider call: the Skill's instruction must shape the answer.

    This is a delivered-procedure signal, not a claim that the model follows
    every procedure; the audit still records `agent_use: NotVerifiable`.
    """
    layer = "real-model"
    source = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(source):
        report.block(layer, "real provider", "~/.leveler/config.toml is absent")
        return
    root, home, repo, env = harness.isolated_env("real", None)
    env = isolate(env, root)
    write_skill(
        repo,
        "marker",
        skill_md(
            "marker",
            "Answer the marker.",
            f"{REAL_BODY}\n\nFor any request in this turn, reply with exactly "
            f"{REAL_ANSWER} and do not call tools.",
        ),
    )
    trust_allow(env, repo)

    pty = launch(env, repo, cols=110, rows=36)
    try:
        if not wait_ready(pty):
            report.block(layer, "real TUI boot", "footer never appeared")
            return
        send(pty, "/marker")
        # The load note is transient; poll while the real turn runs instead of
        # judging the final screen after the recap lane has replaced it.
        note_seen = False
        answered = False
        deadline = time.time() + 180
        while time.time() < deadline:
            if "技能 /marker 已加载" in screen(pty):
                note_seen = True
            if REAL_ANSWER in screen(pty):
                answered = True
                break
            if not pty.alive():
                break
            time.sleep(0.1)
        pty.wait_stable(1.5, 60)
        evidence(report, "real-invoke", pty)
        report.check(
            layer,
            "the real provider answered from the loaded procedure",
            answered and REAL_ANSWER in screen(pty),
            screen(pty)[-1200:],
        )
        report.check(layer, "the real turn reported the load", note_seen, "load note never appeared")
        report.check(layer, "the real user message stays verbatim", has_user_message(pty, "/marker"))
    finally:
        pty.close()


SCENARIOS = {
    "page": scenario_page,
    "isolation": scenario_isolation,
    "completion": scenario_completion,
    "invoke": scenario_invoke,
    "trust": scenario_trust,
    "refusal": scenario_refusal,
    "mid-turn": scenario_mid_turn,
    "sizes": scenario_sizes,
    "interactions": scenario_interactions,
    "resume": scenario_resume,
    "compact": scenario_compact,
    "real-discovery": scenario_real_discovery,
    "real": scenario_real,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--only",
        action="append",
        choices=sorted(SCENARIOS) + ["all"],
        help="run one scenario (repeatable); default: every mock scenario",
    )
    args = parser.parse_args()
    os.makedirs(EVIDENCE, exist_ok=True)
    chosen = args.only or [name for name in SCENARIOS if not name.startswith("real")]
    if "all" in chosen:
        chosen = sorted(SCENARIOS)
    report = harness.Report("skills-pty")
    for name in chosen:
        print(f"\n=== {name} ===")
        SCENARIOS[name](report)
    path = report.save()
    print(f"\nreport: {path}")
    print(f"failures={report.failures} blocked={report.blocked}")
    return 1 if report.failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
