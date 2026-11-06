#!/usr/bin/env python3
"""Real-PTY visual acceptance for the CodeLeveler diff surface.

This drives the REAL `leveler` binary on a REAL pseudo-terminal and reads the
frame back twice:

  * the project's own VT-100 model (text / layout), and
  * a pyte terminal emulator fed the exact same bytes (SGR colours), so the
    screenshot PNGs below are rendered from what the terminal actually
    received. Nothing here hand-paints an "expected" diff.

Cases (the product scenarios from the change request):

  ts-add          TypeScript file: consecutive added lines.
  readme-replace  README: one removed and one added version of a line.
  inline-edit     one line whose middle characters changed.
  ts-long         ~90-line TypeScript ledger diff: inline change + long hunk.
  go-long         40+ line Go test added under a removed placeholder.
  rust-add        Rust struct/function with a string and a comment.
  python-add      Python dataclass and a comprehension.
  js-add          JavaScript helpers with an arrow callback.
  json-add        JSON object: string keys/values and numbers.
  shell-add       Bash script with a shebang, test and comment.
  sql-add         SQL select with a function, string and number.
  jsx-add         React component: `.jsx` must resolve to JavaScript, never
                  the generic Bash fallback. JSX tags/attributes have no
                  dedicated grammar (recorded limit), so the sample asserts
                  keyword/function/string/comment/number ink only.
  unknown-extension
                  `.xyz` file containing code: Bash fallback must still paint
                  semantic syntax inks instead of flat text.

Colour modes:

  truecolor, ansi256, ansi16, none (NO_COLOR).

Usage:
    python3 scripts/diff_visual_acceptance.py --out target/diff-visual/before --expect before
    python3 scripts/diff_visual_acceptance.py --out target/diff-visual/after  --expect after

`--expect before` records what the old build paints and never fails on the
new visual contract; `--expect after` asserts it.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts"))

import pty_dogfood as dog  # noqa: E402

import pyte  # noqa: E402
from PIL import Image, ImageDraw, ImageFont  # noqa: E402

# --- the three product scenarios -------------------------------------------

TS_CONTENT = """\
export interface Money {
  amount: number;
  currency: string;
}

export function add(a: Money, b: Money): Money {
  if (a.currency !== b.currency) {
    throw new Error("currency mismatch");
  }
  return { amount: a.amount + b.amount, currency: a.currency };
}
"""

README_BEFORE = """\
# CodeLeveler

A terminal coding agent.
"""

INLINE_BEFORE = """\
export function total(price: number, quantity: number): number {
  return price * quantity;
}
"""


def patch_update(path: str, context: list[str], removed: list[str], added: list[str]) -> str:
    body = "*** Begin Patch\n*** Update File: %s\n@@\n" % path
    body += "".join(" " + line + "\n" for line in context)
    body += "".join("-" + line + "\n" for line in removed)
    body += "".join("+" + line + "\n" for line in added)
    body += "*** End Patch\n"
    return body


def case_ts_add(env):
    os.makedirs(os.path.join(env.repo, "src"), exist_ok=True)
    return [
        dog.tool_turn(
            "write_file",
            json.dumps({"path": "src/money.ts", "content": TS_CONTENT}),
        ),
        dog.text_turn("已创建 src/money.ts。"),
    ], "create src/money.ts"


def case_readme_replace(env):
    with open(os.path.join(env.repo, "README.md"), "w") as fh:
        fh.write(README_BEFORE)
    patch = patch_update(
        "README.md",
        ["# CodeLeveler", ""],
        ["A terminal coding agent."],
        ["A terminal coding agent that shows every change."],
    )
    return [
        dog.tool_turn("apply_patch", json.dumps({"patch": patch})),
        dog.text_turn("已更新 README.md。"),
    ], "update the README description"


def case_inline_edit(env):
    os.makedirs(os.path.join(env.repo, "src"), exist_ok=True)
    with open(os.path.join(env.repo, "src", "total.ts"), "w") as fh:
        fh.write(INLINE_BEFORE)
    patch = patch_update(
        "src/total.ts",
        [],
        ["  return price * quantity;"],
        ["  return price * count;"],
    )
    return [
        dog.tool_turn("apply_patch", json.dumps({"patch": patch})),
        dog.text_turn("已更新 src/total.ts。"),
    ], "change the variable used by total"


TS_LONG_BASE = """\
// Settlement ledger.
export const LEDGER_VERSION = 1;
export const LEDGER_STAGE = "draft";

export interface LedgerState {
  readonly entries: number;
}

export function summarize(state: LedgerState): string {
  return `${state.entries} entries`;
}
"""

TS_LONG_ADDED = """\
// Ledger posting rules for the settlement pipeline.
// Rules stay pure: one event maps to zero or more entries.
export type CurrencyCode = "USD" | "EUR" | "GBP" | "JPY";

export interface LedgerEntry {
  readonly account: string;
  readonly amount: number;
  readonly currency: CurrencyCode;
  readonly memo: string;
}

export interface PostingContext {
  readonly eventId: string;
  readonly occurredAt: string;
  readonly actor: string;
}

// Round to the smallest unit the ledger can represent.
export function toMinorUnits(amount: number, currency: CurrencyCode): number {
  const scale = currency === "JPY" ? 1 : 100;
  return Math.round(amount * scale);
}

// Build the debit and credit sides of a double-entry posting.
export function buildEntries(
  amount: number,
  currency: CurrencyCode,
  context: PostingContext,
): LedgerEntry[] {
  const minor = toMinorUnits(amount, currency);
  if (minor <= 0) {
    throw new Error(`invalid amount for ${context.eventId}`);
  }
  const debit: LedgerEntry = {
    account: "assets:receivable",
    amount: minor,
    currency,
    memo: `debit ${context.eventId}`,
  };
  const credit: LedgerEntry = {
    account: "income:settlement",
    amount: -minor,
    currency,
    memo: `credit ${context.eventId}`,
  };
  return [debit, credit];
}

// Apply a posting and return the new balance.
export function applyPosting(
  balance: number,
  entries: LedgerEntry[],
): number {
  let next = balance;
  for (const entry of entries) {
    next += entry.amount;
  }
  return next;
}

// Reject postings whose currency does not match the account.
export function assertCurrency(
  entry: LedgerEntry,
  expected: CurrencyCode,
): void {
  if (entry.currency !== expected) {
    throw new Error(`currency mismatch: ${entry.currency}`);
  }
}

// Format one entry for the audit trail.
export function formatEntry(entry: LedgerEntry): string {
  const sign = entry.amount < 0 ? "-" : "+";
  const value = Math.abs(entry.amount) / 100;
  return `${sign}${value.toFixed(2)} ${entry.currency} ${entry.account}`;
}

// Fold every entry into a per-account total.
export function totalsByAccount(entries: LedgerEntry[]): Map<string, number> {
  const totals = new Map<string, number>();
  for (const entry of entries) {
    const current = totals.get(entry.account) ?? 0;
    totals.set(entry.account, current + entry.amount);
  }
  return totals;
}
"""


def case_ts_long(env):
    os.makedirs(os.path.join(env.repo, "src"), exist_ok=True)
    with open(os.path.join(env.repo, "src", "ledger.ts"), "w") as fh:
        fh.write(TS_LONG_BASE)
    # Two hunks: a one-for-one inline change, then a long addition. Both live
    # in one tool cell so added, removed and context rows share one frame.
    body = "*** Begin Patch\n*** Update File: src/ledger.ts\n@@\n"
    body += " // Settlement ledger.\n"
    body += " export const LEDGER_VERSION = 1;\n"
    body += '-export const LEDGER_STAGE = "draft";\n'
    body += '+export const LEDGER_STAGE = "stable";\n'
    body += "@@\n"
    body += " export function summarize(state: LedgerState): string {\n"
    body += "   return `${state.entries} entries`;\n"
    body += " }\n"
    body += "".join("+" + line + "\n" for line in TS_LONG_ADDED.splitlines())
    body += "*** End Patch\n"
    return [
        dog.tool_turn("apply_patch", json.dumps({"patch": body})),
        dog.text_turn("已扩展 ledger 模块。"),
    ], "add ledger posting rules"


GO_LONG_BASE = """\
package cookie

import "testing"

// Entry is one ledger posting.
type Entry struct {
\tAccount  string
\tAmount   int
\tCurrency string
}

type LedgerState struct {
\tEntries int
}

func ToMinorUnits(amount int, currency string) int {
\tif currency == "JPY" {
\t\treturn amount
\t}
\treturn amount * 100
}

// TODO: add the ledger test suite.
"""

GO_LONG_TEST = """\
// TestLedgerTotals checks that postings balance.
func TestLedgerTotals(t *testing.T) {
\tentry := Entry{
\t\tAccount:  "assets:receivable",
\t\tAmount:   1250,
\t\tCurrency: "USD",
\t}
\t// Minor units keep the ledger exact.
\tminor := ToMinorUnits(entry.Amount, entry.Currency)
\tif minor != 1250 {
\t\tt.Fatalf("unexpected minor units: %d", minor)
\t}
\tlabel := "settlement"
\tswitch label {
\tcase "settlement":
\t\tt.Logf("settled %s", label)
\tdefault:
\t\tt.Errorf("unknown label %q", label)
\t}
\tcount := 0
\tfor i := 0; i < 10; i++ {
\t\tif i%2 == 0 {
\t\t\tcount += i
\t\t}
\t}
\tif count != 20 {
\t\tt.Errorf("expected 20, got %d", count)
\t}
\tstate := &LedgerState{Entries: count}
\tif state.Entries == 0 {
\t\tt.Fatal("empty ledger")
\t}
\tif entry.Currency != "USD" {
\t\tt.Fatalf("unexpected currency %q", entry.Currency)
\t}
\tfor _, e := range []Entry{entry} {
\t\tif e.Amount <= 0 {
\t\t\tt.Errorf("non-positive amount: %d", e.Amount)
\t\t}
\t}
\tt.Log("ledger test complete")
}
"""


def case_go_long(env):
    os.makedirs(os.path.join(env.repo, "internal", "cookie"), exist_ok=True)
    with open(os.path.join(env.repo, "internal", "cookie", "cookie_test.go"), "w") as fh:
        fh.write(GO_LONG_BASE)
    # One context row, one removed placeholder, then a 40+ line addition: the
    # shape of a real Claude Code Go diff.
    body = "*** Begin Patch\n*** Update File: internal/cookie/cookie_test.go\n@@\n"
    body += " }\n"
    body += " \n"
    body += "-// TODO: add the ledger test suite.\n"
    body += "".join("+" + line + "\n" for line in GO_LONG_TEST.splitlines())
    body += "*** End Patch\n"
    return [
        dog.tool_turn("apply_patch", json.dumps({"patch": body})),
        dog.text_turn("已补充 cookie 账本测试。"),
    ], "add the ledger test suite"


RUST_CONTENT = """\
/// Settlement entry.
struct Ledger {
    amount: i64,
    memo: String,
}

pub fn totals(entries: &[Ledger]) -> i64 {
    let mut sum = 0; // accumulator
    let label = "settled";
    for entry in entries {
        sum += entry.amount;
    }
    sum
}
"""

PYTHON_CONTENT = """\
from dataclasses import dataclass

# One posting in the ledger.
@dataclass
class Entry:
    account: str = "cash"
    amount: float = 1250

def total(entries: list[Entry]) -> float:
    return sum(entry.amount for entry in entries if entry.amount > 0)
"""

JS_CONTENT = """\
// Ledger helpers.
export function total(entries) {
  const settled = entries.filter((entry) => entry.amount > 0);
  const note = "ledger-ready";
  return settled.reduce((sum, entry) => sum + entry.amount, 1250);
}
"""

JSON_CONTENT = """\
{
  "ledger": "settlement",
  "version": 2,
  "entries": [
    { "account": "cash", "amount": 1250 }
  ]
}
"""

SHELL_CONTENT = """\
#!/usr/bin/env bash
set -euo pipefail

# Fetch the ledger once.
if [ -f "$HOME/ledger.json" ]; then
  echo "ready"  # probe
fi
"""

SQL_CONTENT = """\
-- One row per settled posting.
SELECT account, SUM(amount) AS total
FROM postings
WHERE settled = TRUE AND account = 'cash' AND amount > 1250
GROUP BY account;
"""

# React JSX. The bundled fancy-regex syntax set has no JSX grammar, so `.jsx`
# resolves to JavaScript: keywords, strings, comments, numbers and calls keep
# their semantic inks. JSX tags and attributes are NOT a dedicated grammar
# context and may read as identifiers/operators (recorded as a real limit).
JSX_CONTENT = """\
import React, { useState } from "react";

// Ledger badge rendered in the header.
export function LedgerBadge({ total, currency = "USD" }) {
  const [open, setOpen] = useState(false);
  const label = `${currency} ${total.toFixed(2)}`;
  return (
    <button className="badge" onClick={() => setOpen(!open)}>
      {open ? <span title="Settled">{label}</span> : null}
    </button>
  );
}
"""


def added_file_case(env, path, content, note):
    return [
        dog.tool_turn("write_file", json.dumps({"path": path, "content": content})),
        dog.text_turn(note),
    ], "create " + path


def case_rust(env):
    return added_file_case(env, "src/ledger.rs", RUST_CONTENT, "已创建 src/ledger.rs。")


def case_python(env):
    return added_file_case(env, "src/ledger.py", PYTHON_CONTENT, "已创建 src/ledger.py。")


def case_js(env):
    return added_file_case(env, "src/ledger.js", JS_CONTENT, "已创建 src/ledger.js。")


def case_json(env):
    return added_file_case(env, "config/ledger.json", JSON_CONTENT, "已创建 config/ledger.json。")


def case_shell(env):
    return added_file_case(
        env, "scripts/fetch-ledger.sh", SHELL_CONTENT, "已创建 scripts/fetch-ledger.sh。"
    )


def case_sql(env):
    return added_file_case(env, "queries/totals.sql", SQL_CONTENT, "已创建 queries/totals.sql。")


def case_jsx(env):
    return added_file_case(
        env, "src/LedgerBadge.jsx", JSX_CONTENT, "已创建 src/LedgerBadge.jsx。"
    )


def case_unknown_extension(env):
    # Definitely code (a shell script), but `.xyz` resolves to no bundled
    # grammar: the diff must still read as code via the Bash fallback.
    return added_file_case(
        env, "scripts/deploy.xyz", SHELL_CONTENT, "已创建 scripts/deploy.xyz。"
    )


CASES = {
    "ts-add": case_ts_add,
    "readme-replace": case_readme_replace,
    "inline-edit": case_inline_edit,
    "ts-long": case_ts_long,
    "go-long": case_go_long,
    "rust-add": case_rust,
    "python-add": case_python,
    "js-add": case_js,
    "json-add": case_json,
    "shell-add": case_shell,
    "sql-add": case_sql,
    "jsx-add": case_jsx,
    "unknown-extension": case_unknown_extension,
}

# Each case's marker text, the code row we look for, and the fragment that
# should be marked as changed on that row (inline case only).
EXPECT = {
    "ts-add": {
        "head": "src/money.ts",
        "needle": "currency: a.currency",
        "kind": "add",
    },
    "readme-replace": {
        "head": "README.md",
        "needle": "A terminal coding agent that shows every change.",
        "kind": "add",
        "removed_needle": "A terminal coding agent.",
    },
    "inline-edit": {
        "head": "src/total.ts",
        "needle": "return price * count;",
        "kind": "add",
        "changed": "count",
        "changed_role": "variable",
        "unchanged": "price * ",
        "removed_needle": "return price * quantity;",
        "removed_changed": "quantity",
    },
    "ts-long": {
        "head": "src/ledger.ts",
        "needle": "return totals;",
        "kind": "add",
        # Visible only after scrolling up from the bottom of the long block.
        "scroll_needle": "Rules stay pure",
        "context_needle": "export function summarize",
        "changed": "stable",
        "changed_role": "string",
        "unchanged": "export const LEDGER_STAGE = ",
        "changed_row_needle": '= "stable";',
        "removed_needle": 'export const LEDGER_STAGE = "draft";',
        "removed_changed": "draft",
    },
    "go-long": {
        "head": "internal/cookie/cookie_test.go",
        "needle": 't.Fatal("empty ledger")',
        "kind": "add",
        "scroll_needle": "// TestLedgerTotals checks",
        "removed_needle": "// TODO: add the ledger test suite.",
    },
    "rust-add": {
        "head": "src/ledger.rs",
        "needle": "sum += entry.amount;",
        "kind": "add",
    },
    "python-add": {
        "head": "src/ledger.py",
        "needle": "return sum(entry.amount",
        "kind": "add",
    },
    "js-add": {
        "head": "src/ledger.js",
        "needle": "settled.reduce",
        "kind": "add",
    },
    "json-add": {
        "head": "config/ledger.json",
        "needle": '"amount": 1250',
        "kind": "add",
        # JSON has strings and numbers only; demanding seven semantic roles
        # would be a made-up requirement rather than a grammar fact.
        "min_inks": 2,
    },
    "shell-add": {
        "head": "scripts/fetch-ledger.sh",
        "needle": 'echo "ready"',
        "kind": "add",
    },
    "sql-add": {
        "head": "queries/totals.sql",
        "needle": "SELECT account",
        "kind": "add",
    },
    "jsx-add": {
        "head": "src/LedgerBadge.jsx",
        "needle": 'className="badge"',
        "kind": "add",
    },
    "unknown-extension": {
        "head": "scripts/deploy.xyz",
        "needle": 'echo "ready"',
        "kind": "add",
    },
}


# Band colours resolved by theme.rs; the PTY carries these exact RGB values.
BANDS = {
    "dark": {
        "add": "14301c",
        "remove": "3a181a",
        "add_inline": "1e4a2d",
        "remove_inline": "522527",
    },
    "light": {
        "add": "def0e4",
        "remove": "f8dada",
        "add_inline": "a7dcbb",
        "remove_inline": "f2aeae",
    },
    "high-contrast": {
        "add": "002200",
        "remove": "330000",
        "add_inline": "004400",
        "remove_inline": "660000",
    },
}


# 256-colour strategy A: the xterm cube has no soft dark green/red, so the row
# band is the neutral step of the same polarity (#262626 on dark, #E4E4E4 on
# light) with a directional marker; the inline token keeps a saturated step.
BANDS_256 = {
    "dark": {
        "add": "262626",
        "remove": "262626",
        "add_inline": "008700",
        "remove_inline": "870000",
    },
    "light": {
        "add": "e4e4e4",
        "remove": "e4e4e4",
        "add_inline": "d7ffd7",
        "remove_inline": "ffd7d7",
    },
    "high-contrast": {
        "add": "262626",
        "remove": "262626",
        "add_inline": "008700",
        "remove_inline": "870000",
    },
}


# The semantic syntax inks the TUI paints in truecolor, per theme. These are
# the colours the user reads, independent of the syntect anchor palette.
INK = {
    "dark": {
        "keyword": "e39ad6",
        "string": "e6c07b",
        "type": "7fd3e8",
        "function": "8fb8f0",
        "variable": "e7e9ec",
        "comment": "8b93a1",
        "number": "e0a06e",
    },
    "light": {
        "keyword": "a02666",
        "string": "7a5200",
        "type": "0b6070",
        "function": "24509e",
        "variable": "20242a",
        "comment": "59616c",
        "number": "8a4b16",
    },
    "high-contrast": {
        "keyword": "ff9bd2",
        "string": "ffd166",
        "type": "7fe8ff",
        "function": "9fc4ff",
        "variable": "ffffff",
        "comment": "b8c0cc",
        "number": "ffb27a",
    },
}

# (needle, semantic role) sampled from a real frame. The column of the needle's
# first character decides the ink, so every needle starts inside one token.
TOKENS = {
    "go-long": [
        ("func TestLedgerTotals", "keyword"),
        ("TestLedgerTotals(t", "function"),
        ("minor :=", "variable"),
        ("settlement", "string"),
        ("// TestLedgerTotals checks", "comment"),
        ("1250", "number"),
        ("T) {", "type"),
    ],
    "ts-long": [
        ("export type", "keyword"),
        ("CurrencyCode = ", "type"),
        ("toMinorUnits(amount", "function"),
        ("minor =", "variable"),
        ("JPY", "string"),
        ("// Rules stay pure", "comment"),
        ("100", "number"),
    ],
    "rust-add": [
        ("pub fn", "keyword"),
        ("Ledger {", "type"),
        ("totals(entries", "function"),
        ("entries: &[Ledger]", "variable"),
        ("// accumulator", "comment"),
        ("settled", "string"),
        ("0;", "number"),
    ],
    "python-add": [
        ("def total", "keyword"),
        ("Entry:", "type"),
        ("total(entries", "function"),
        ("entries: list", "variable"),
        ("# One posting", "comment"),
        ("cash", "string"),
        ("1250", "number"),
    ],
    "js-add": [
        ("export function", "keyword"),
        ("total(entries", "function"),
        ("settled =", "variable"),
        ("// Ledger helpers", "comment"),
        ("ledger-ready", "string"),
        ("1250", "number"),
    ],
    "json-add": [
        ("ledger", "string"),
        ("settlement", "string"),
        ("1250", "number"),
    ],
    "shell-add": [
        ("if [", "keyword"),
        ("echo", "function"),
        ("ready", "string"),
        ("# Fetch", "comment"),
        ("HOME", "variable"),
    ],
    "sql-add": [
        ("SELECT", "keyword"),
        ("SUM", "function"),
        ("-- One row", "comment"),
        ("cash", "string"),
        ("1250", "number"),
    ],
    # `jsx` is deliberately absent from the sample: no JSX grammar is bundled,
    # so tag-name/attribute ink is not a contract this build can meet.
    "jsx-add": [
        ("import React", "keyword"),
        ("LedgerBadge", "function"),
        ("USD", "string"),
        ("// Ledger badge", "comment"),
        ("false", "number"),
    ],
    "unknown-extension": [
        ("if [", "keyword"),
        ("echo", "function"),
        ("ready", "string"),
        ("# Fetch", "comment"),
        ("HOME", "variable"),
    ],
}


def find_code_cell(grid, needle: str):
    """(row, col) of `needle` on a diff code row, never on UI chrome."""
    for y in range(len(grid)):
        text = "".join(cell.data for cell in grid[y])
        if "│" not in text:
            continue
        col = text.find(needle)
        if col >= 0:
            return y, col
    return None


def token_facts(grid, tokens) -> dict:
    out = {}
    for needle, role in tokens:
        found = find_code_cell(grid, needle)
        if found is None:
            out[needle] = {"role": role, "found": False}
            continue
        y, col = found
        out[needle] = {
            "role": role,
            "found": True,
            "fg": grid[y][col].fg,
            "bg": grid[y][col].bg,
        }
    return out


def channel_distance(a: str, b: str) -> int:
    """Squared RGB distance between two pyte hex colours."""
    try:
        ac = [int(a[i : i + 2], 16) for i in (0, 2, 4)]
        bc = [int(b[i : i + 2], 16) for i in (0, 2, 4)]
    except (TypeError, ValueError):
        return -1
    return sum((x - y) ** 2 for x, y in zip(ac, bc))


def _luminance(hexcolor: str) -> float:
    channels = [int(hexcolor[i : i + 2], 16) / 255.0 for i in (0, 2, 4)]

    def linear(c: float) -> float:
        return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4

    r, g, b = (linear(c) for c in channels)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast_ratio(fg: str, bg: str) -> float:
    """WCAG contrast between two pyte hex colours (0.0 when unknown)."""
    try:
        a, b = _luminance(fg), _luminance(bg)
    except (TypeError, ValueError):
        return 0.0
    hi, lo = max(a, b), min(a, b)
    return (hi + 0.05) / (lo + 0.05)


def replay(pty_, cols: int, rows: int) -> pyte.Screen:
    screen = pyte.Screen(cols, rows)
    stream = pyte.Stream(screen)
    stream.feed(pty_.raw.decode("utf-8", "replace"))
    return screen


def cells(screen: pyte.Screen):
    return [
        [screen.buffer[y][x] for x in range(screen.columns)]
        for y in range(screen.lines)
    ]


def row_text(grid, y: int) -> str:
    return "".join(cell.data for cell in grid[y]).rstrip()


def find_cell(grid, needle: str):
    """(row, col) of `needle` in the rendered grid."""
    for y in range(len(grid)):
        text = "".join(cell.data for cell in grid[y])
        col = text.find(needle)
        if col >= 0:
            return y, col
    return None


def find_in_row(grid, y: int, needle: str):
    """(row, col) of `needle` restricted to an already-identified row.

    The removed and added versions of a line share most of their text, so a
    whole-grid search would resolve the wrong row and compare the inline band
    against the neighbouring diff line.
    """
    col = "".join(cell.data for cell in grid[y]).find(needle)
    return (y, col) if col >= 0 else None


def band_leak_rows(grid, band_values):
    """Rows whose band colour has no gutter: stale cells from an old layout.

    A band colour outside a gutter row is a stale cell from the pre-scroll
    layout: the exact ghosting this check exists for. Wrapped continuation
    rows legitimately keep the band without a gutter, and they always follow
    a banded first row.
    """
    banded_rows = [
        any(c.bg in band_values for c in grid[y]) for y in range(len(grid))
    ]
    return [
        y
        for y in range(len(grid))
        if banded_rows[y]
        and "│" not in row_text(grid, y)
        and not (y > 0 and banded_rows[y - 1])
    ]


def scroll_frame_facts(grid, exp, band_values, needle):
    """Backgrounds and stale-band rows for one scrolled frame."""
    out = {}
    row = find_cell(grid, needle)
    out["found"] = row is not None
    if row is not None:
        out["bg"] = grid[row[0]][row[1]].bg
    if exp.get("context_needle"):
        ctx = find_cell(grid, exp["context_needle"])
        if ctx is not None:
            out["context_bg"] = grid[ctx[0]][ctx[1]].bg
    if exp.get("removed_needle"):
        rem = find_cell(grid, exp["removed_needle"])
        if rem is not None:
            out["removed_bg"] = grid[rem[0]][rem[1]].bg
            if exp.get("removed_changed"):
                rc = find_in_row(grid, rem[0], exp["removed_changed"])
                if rc is not None:
                    out["removed_changed_bg"] = grid[rc[0]][rc[1]].bg
                    out["removed_changed_fg"] = grid[rc[0]][rc[1]].fg
    if exp.get("changed") and exp.get("unchanged"):
        changed_row = find_cell(
            grid, exp.get("changed_row_needle") or exp["unchanged"]
        )
        if changed_row is not None:
            ch = find_in_row(grid, changed_row[0], exp["changed"])
            un = find_in_row(grid, changed_row[0], exp["unchanged"])
            if ch is not None and un is not None:
                out["changed_bg"] = grid[ch[0]][ch[1]].bg
                out["unchanged_bg"] = grid[un[0]][un[1]].bg
                out["changed_fg"] = grid[ch[0]][ch[1]].fg
                out["unchanged_fg"] = grid[un[0]][un[1]].fg
    out["leak_rows"] = band_leak_rows(grid, band_values)
    return out


def count_sgr(raw: bytes) -> dict:
    """Colour-capability evidence from the bytes the terminal received.

    crossterm encodes `Color::Indexed(0..=15)` as `38;5;N` too, so a genuine
    16-colour palette shows up as indexed values below 16 -- still a
    degradation to the base palette, just not the 30-37 SGR spelling.
    """
    fg256 = [int(m) for m in re.findall(rb"\x1b\[[0-9;]*38;5;(\d+)", raw)]
    bg256 = [int(m) for m in re.findall(rb"\x1b\[[0-9;]*48;5;(\d+)", raw)]
    return {
        "truecolor_fg": len(re.findall(rb"\x1b\[[0-9;]*38;2;", raw)),
        "truecolor_bg": len(re.findall(rb"\x1b\[[0-9;]*48;2;", raw)),
        "indexed_fg_base16": sum(1 for n in fg256 if n < 16),
        "indexed_bg_base16": sum(1 for n in bg256 if n < 16),
        "indexed_fg_wide": sum(1 for n in fg256 if n >= 16),
        "indexed_bg_wide": sum(1 for n in bg256 if n >= 16),
        "basic_fg": len(re.findall(rb"\x1b\[(?:3[0-7]|9[0-7])m", raw)),
        "basic_bg": len(re.findall(rb"\x1b\[(?:4[0-7]|10[0-7])m", raw)),
        "underline": len(re.findall(rb"\x1b\[(?:\d+;)*4(?:;\d+)*m", raw)),
    }


def color_of(cell) -> tuple[str, str]:
    return (cell.fg, cell.bg)


def _font(size: int, bold: bool = False):
    candidates = [
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
    ]
    for path in candidates:
        if os.path.exists(path):
            try:
                return ImageFont.truetype(path, size)
            except Exception:
                pass
    return ImageFont.load_default()


def _cjk_font(size: int):
    for path in [
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
    ]:
        if os.path.exists(path):
            try:
                return ImageFont.truetype(path, size)
            except Exception:
                pass
    return _font(size)


def _rgb(color: str, default):
    if color in (None, "default"):
        return default
    named = {
        "black": (0, 0, 0),
        "red": (205, 49, 49),
        "green": (13, 188, 121),
        "brown": (229, 229, 16),
        "yellow": (229, 229, 16),
        "blue": (36, 114, 200),
        "magenta": (188, 63, 188),
        "cyan": (17, 168, 205),
        "white": (229, 229, 229),
        "brightblack": (102, 102, 102),
        "brightred": (241, 76, 76),
        "brightgreen": (35, 209, 139),
        "brightyellow": (245, 245, 67),
        "brightblue": (59, 142, 234),
        "brightmagenta": (214, 112, 214),
        "brightcyan": (41, 184, 219),
        "brightwhite": (255, 255, 255),
    }
    if color in named:
        return named[color]
    if re.fullmatch(r"[0-9a-fA-F]{6}", color):
        return tuple(int(color[i : i + 2], 16) for i in (0, 2, 4))
    return default


def screenshot(screen: pyte.Screen, path: str, title: str):
    grid = cells(screen)
    cw, ch = 8, 16
    pad = 12
    width = screen.columns * cw + pad * 2
    height = screen.lines * ch + pad * 2 + 22
    img = Image.new("RGB", (width, height), (0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.text((pad, 4), title, fill=(180, 190, 200), font=_font(11))
    mono = _font(12)
    cjk = _cjk_font(12)
    for y in range(screen.lines):
        for x in range(screen.columns):
            cell = grid[y][x]
            fg = _rgb(cell.fg, (231, 233, 236))
            bg = _rgb(cell.bg, (17, 19, 21))
            px, py = pad + x * cw, pad + 22 + y * ch
            # Wide CJK/emoji glyphs occupy two cells; the continuation cell has
            # empty data but still owns background colour, so paint every cell.
            draw.rectangle([px, py, px + cw - 1, py + ch - 1], fill=bg)
            if cell.data == "":
                continue
            font = cjk if ord(cell.data[0]) > 0x2E80 else mono
            draw.text((px, py), cell.data, fill=fg, font=font)
            if getattr(cell, "underscore", False):
                draw.line([px, py + ch - 2, px + cw - 1, py + ch - 2], fill=fg)
    img.save(path)


# --- one run ----------------------------------------------------------------


def run_case(
    case: str,
    mode: str,
    out_dir: str,
    expect: str,
    bin_path: str,
    theme: str = "dark",
    resize: bool = False,
    scroll: bool = False,
) -> dict:
    mock = dog.MockProvider()
    env = dog.Env("mock", mock)
    try:
        turns, prompt = CASES[case](env)
        mock.turns = turns
        if theme != "dark":
            # The real UI reads [ui] theme from the isolated LEVELER_HOME.
            with open(os.path.join(env.home, "config.toml"), "a") as fh:
                fh.write('\n[ui]\ntheme = "%s"\n' % theme)
        # Terminal identity from the developer's real session must not leak
        # into the capability matrix: TERM_PROGRAM=Apple_Terminal on macOS
        # < 26 downgrades a claimed truecolor terminal to indexed 256.
        env.env.pop("TERM_PROGRAM", None)
        env.env.pop("TERM_PROGRAM_VERSION", None)
        if mode == "ansi256":
            env.env["TERM"] = "xterm-256color"
            env.env.pop("COLORTERM", None)
        elif mode == "ansi16":
            env.env["TERM"] = "xterm"
            env.env.pop("COLORTERM", None)
        elif mode == "none":
            env.env.pop("COLORTERM", None)
            env.env["NO_COLOR"] = "1"
        else:
            env.env["TERM"] = "xterm-256color"
            env.env["COLORTERM"] = "truecolor"
            env.env.pop("NO_COLOR", None)

        pty_ = dog.spawn(env.env, env.repo, 120, 40, [], bin_path or dog.BIN)
        try:
            if not (pty_.wait_for("leveler", timeout=60) or pty_.wait_stable(1.0, 60)):
                raise RuntimeError("TUI did not paint the first frame: " + pty_.screen.text()[:400])
            # The startup banner is a Kitty image and its placeholder cells
            # carry a fixed truecolor tint. Colour-capability evidence starts
            # after it, so the diff surface is judged on its own output.
            sgr_mark = len(pty_.raw)
            dog.ask(pty_, prompt)
            exp = EXPECT[case]
            # Wait on the frame-visible needle. An inline-changed token can sit
            # above the fold in a long diff, so it is only used for facts.
            needle = exp["needle"]
            if not pty_.wait_for(needle, timeout=180):
                raise RuntimeError("diff row never appeared: " + pty_.screen.text()[-1200:])
            pty_.wait_stable(1.2, 60)

            stem = "%s-%s" % (case, mode)
            with open(os.path.join(out_dir, stem + ".ansi"), "wb") as fh:
                fh.write(bytes(pty_.raw))
            screen = replay(pty_, 120, 40)
            grid = cells(screen)
            screenshot(screen, os.path.join(out_dir, stem + ".png"), "CodeLeveler TUI — " + stem)
            text = "\n".join(row_text(grid, y) for y in range(screen.lines))
            with open(os.path.join(out_dir, stem + ".txt"), "w") as fh:
                fh.write(text + "\n")

            expected_bg = None
            expected_removed_bg = None
            bands = BANDS[theme]
            # The diff row band in the current colour mode. An empty set means
            # that mode is not judged on band colours (16-colour/NO_COLOR).
            band_values = (
                {BANDS[theme]["add"], BANDS[theme]["remove"]}
                if mode == "truecolor"
                else {BANDS_256[theme]["add"], BANDS_256[theme]["remove"]}
                if mode == "ansi256"
                else set()
            )
            if mode == "truecolor":
                expected_bg = bands["add"] if exp["kind"] == "add" else bands["remove"]
                expected_removed_bg = bands["remove"]
            row = find_cell(grid, exp["needle"])
            facts = {
                "case": case,
                "mode": mode,
                "theme": theme,
                "screen_found": row is not None,
                "screen_text": text,
            }
            if TOKENS.get(case):
                facts["tokens"] = token_facts(grid, TOKENS[case])
            if row is not None:
                y, col = row
                line = "".join(c.data for c in grid[y])
                band = [grid[y][x] for x in range(0, min(col, 8))]
                facts["code_cell"] = {
                    "row": y,
                    "col": col,
                    "fg": grid[y][col].fg,
                    "bg": grid[y][col].bg,
                }
                facts["band_bg"] = sorted({c.bg for c in band})
                marker = next(
                    (c for c in grid[y][:col] if c.data in ("+", "-")), None
                )
                if marker is not None:
                    facts["marker_cell"] = {"fg": marker.fg, "bg": marker.bg}
                facts["row_bg_values"] = sorted(
                    {grid[y][x].bg for x in range(min(col, len(line)))}
                )
                if exp.get("changed") and exp.get("unchanged"):
                    changed = find_in_row(grid, y, exp["changed"])
                    unchanged = find_in_row(grid, y, exp["unchanged"])
                    if changed and unchanged:
                        facts["changed_bg"] = grid[changed[0]][changed[1]].bg
                        facts["unchanged_bg"] = grid[unchanged[0]][unchanged[1]].bg
                        facts["changed_fg"] = grid[changed[0]][changed[1]].fg
                        facts["unchanged_fg"] = grid[unchanged[0]][unchanged[1]].fg
                if exp.get("removed_needle"):
                    removed = find_cell(grid, exp["removed_needle"])
                    facts["removed_found"] = removed is not None
                    if removed is not None and removed[0] != y:
                        facts["removed_row"] = removed[0]
                        facts["removed_bg"] = grid[removed[0]][removed[1]].bg
                        if exp.get("removed_changed"):
                            rm_changed = find_in_row(
                                grid, removed[0], exp["removed_changed"]
                            )
                            if rm_changed:
                                facts["removed_changed_bg"] = grid[rm_changed[0]][
                                    rm_changed[1]
                                ].bg
                                facts["removed_changed_fg"] = grid[rm_changed[0]][
                                    rm_changed[1]
                                ].fg
                    else:
                        facts["removed_found"] = False
            # Raw SGR evidence distinguishes truecolor from 256 from 16.
            raw = bytes(pty_.raw)
            facts["sgr_scope"] = {
                "from_byte": sgr_mark,
                "to_byte": len(raw),
            }
            facts["sgr"] = count_sgr(raw[sgr_mark:])

            # A live resize must re-lay the band, not leave stale cells at the
            # old width. Ratatui clears and repaints on resize, so the bytes
            # after the SIGWINCH are a complete 90x30 frame on their own.
            if resize:
                resize_mark = len(pty_.raw)
                pty_.resize(90, 30)
                pty_.wait_stable(1.2, 60)
                resized_raw = bytes(pty_.raw)[resize_mark:]
                r_screen = pyte.Screen(90, 30)
                pyte.Stream(r_screen).feed(resized_raw.decode("utf-8", "replace"))
                r_grid = cells(r_screen)
                r_text = "\n".join(row_text(r_grid, y) for y in range(r_screen.lines))
                r_row = find_cell(r_grid, exp["needle"])
                screenshot(
                    r_screen,
                    os.path.join(out_dir, stem + "-resized.png"),
                    "CodeLeveler TUI — " + stem + " (resized 90x30)",
                )
                with open(os.path.join(out_dir, stem + "-resized.ansi"), "wb") as fh:
                    fh.write(resized_raw)
                facts["resize"] = {
                    "cols": 90,
                    "rows": 30,
                    "found": r_row is not None,
                    "text": r_text,
                }
                if r_row is not None:
                    facts["resize"]["bg"] = r_grid[r_row[0]][r_row[1]].bg

            # Real scrolling: PageUp moves the conversation viewport. Replay
            # the whole stream into a fresh screen, so the reconstruction is
            # exactly the terminal's current grid (no partial-diff assumption).
            if scroll and exp.get("scroll_needle"):
                # Page size follows the viewport, so a fixed number of PageUps
                # lands differently after a resize. Scroll until the target
                # row is actually on the frame, with a bounded fallback.
                sc_screen = replay(pty_, pty_.cols, pty_.rows)
                sc_grid = cells(sc_screen)
                sc_row = find_cell(sc_grid, exp["scroll_needle"])
                for _ in range(12):
                    if sc_row is not None:
                        break
                    pty_.key("\x1b[5~")
                    pty_.wait_stable(1.2, 60)
                    sc_screen = replay(pty_, pty_.cols, pty_.rows)
                    sc_grid = cells(sc_screen)
                    sc_row = find_cell(sc_grid, exp["scroll_needle"])
                sc_text = "\n".join(
                    row_text(sc_grid, y) for y in range(sc_screen.lines)
                )
                screenshot(
                    sc_screen,
                    os.path.join(out_dir, stem + "-scrolled.png"),
                    "CodeLeveler TUI — " + stem + " (scrolled)",
                )
                with open(os.path.join(out_dir, stem + "-scrolled.ansi"), "wb") as fh:
                    fh.write(bytes(pty_.raw))
                facts["scroll"] = {
                    "cols": pty_.cols,
                    "rows": pty_.rows,
                    "text": sc_text,
                    **scroll_frame_facts(
                        sc_grid, exp, band_values, exp["scroll_needle"]
                    ),
                }
                if TOKENS.get(case):
                    facts["scroll_tokens"] = token_facts(sc_grid, TOKENS[case])
                # A small viewport cannot hold the hunk-1 inline change and a
                # line from the long added block at once. Scroll on until the
                # inline-change row is visible and keep that frame separately.
                top_needle = exp.get("changed_row_needle") or exp.get("removed_needle")
                sc_top = find_cell(sc_grid, top_needle) if top_needle else None
                top_moves = 0
                for _ in range(12):
                    if sc_top is not None or not top_needle:
                        break
                    pty_.key("\x1b[5~")
                    pty_.wait_stable(1.2, 60)
                    sc_screen = replay(pty_, pty_.cols, pty_.rows)
                    sc_grid = cells(sc_screen)
                    sc_top = find_cell(sc_grid, top_needle)
                    top_moves += 1
                if sc_top is not None and top_moves:
                    screenshot(
                        sc_screen,
                        os.path.join(out_dir, stem + "-scrolled-top.png"),
                        "CodeLeveler TUI — " + stem + " (scrolled to inline change)",
                    )
                    facts["scroll_top"] = scroll_frame_facts(
                        sc_grid,
                        exp,
                        band_values,
                        top_needle,
                    )
                if TOKENS.get(case):
                    facts["scroll_top_tokens"] = token_facts(sc_grid, TOKENS[case])

            # A long diff is taller than the viewport, and the conversation
            # opens at its newest rows. The top hunks therefore do not exist on
            # the first frame; sample them from frames the real PageUp path
            # produces, instead of requiring one frame to hold the whole diff.
            # This runs for the resize matrix too, which used to judge syntax
            # tokens against the bottom-anchored first frame only.
            if TOKENS.get(case) and expect == "after" and (scroll or resize):
                sampled = [needle for needle, _ in TOKENS[case]]
                known = set()
                for bucket in ("tokens", "scroll_tokens", "scroll_top_tokens"):
                    for needle, entry in (facts.get(bucket) or {}).items():
                        if entry.get("found"):
                            known.add(needle)
                missing = [needle for needle in sampled if needle not in known]
                if missing:
                    if scroll:
                        # Earlier steps moved the viewport; return to the live
                        # bottom first so paging starts from a user-visible
                        # anchor and the round trip is measurable.
                        for _ in range(12):
                            bottom_grid = cells(replay(pty_, pty_.cols, pty_.rows))
                            if find_cell(bottom_grid, exp["needle"]) is not None:
                                break
                            pty_.key("\x1b[6~")
                            pty_.wait_stable(1.2, 60)
                    facts["return_tokens"] = token_facts(
                        cells(replay(pty_, pty_.cols, pty_.rows)), TOKENS[case]
                    )
                    collected = {}
                    leaks = []
                    pages = 0
                    while True:
                        frame_screen = replay(pty_, pty_.cols, pty_.rows)
                        frame_grid = cells(frame_screen)
                        for needle, entry in token_facts(
                            frame_grid, TOKENS[case]
                        ).items():
                            if entry.get("found") and needle not in collected:
                                collected[needle] = entry
                        if band_values:
                            leaks.extend(band_leak_rows(frame_grid, band_values))
                        if all(needle in collected for needle in missing) or pages >= 16:
                            break
                        pty_.key("\x1b[5~")
                        pty_.wait_stable(1.2, 60)
                        pages += 1
                    facts["paged_tokens"] = collected
                    facts["paged"] = {
                        "pages": pages,
                        "cols": pty_.cols,
                        "rows": pty_.rows,
                        "missing": [needle for needle in missing if needle not in collected],
                        "leak_rows": sorted(set(leaks)),
                    }
                    screenshot(
                        frame_screen,
                        os.path.join(out_dir, stem + "-paged.png"),
                        "CodeLeveler TUI — " + stem + " (paged to the sampled hunks)",
                    )

            # ---- assertions -------------------------------------------------
            ok = facts["screen_found"]
            checks = [("the diff row is on the real PTY frame", ok, exp["needle"])]
            if ok:
                if mode == "truecolor" and expect == "after":
                    code = facts["code_cell"]
                    checks.append(
                        (
                            "the code cell carries the soft %s band" % exp["kind"],
                            code["bg"] == expected_bg,
                            "bg=%s expected=%s" % (code["bg"], expected_bg),
                        )
                    )
                    checks.append(
                        (
                            "the marker/gutter columns share the band",
                            any(bg == expected_bg for bg in facts["band_bg"]),
                            "band bg=%s expected=%s" % (facts["band_bg"], expected_bg),
                        )
                    )
                    if exp.get("removed_needle") and not exp.get("scroll_needle"):
                        checks.append(
                            (
                                "the removed row carries the soft removal band",
                                facts.get("removed_found")
                                and facts.get("removed_bg") == expected_removed_bg,
                                "removed=%s expected=%s found=%s"
                                % (
                                    facts.get("removed_bg"),
                                    expected_removed_bg,
                                    facts.get("removed_found"),
                                ),
                            )
                        )
                    if resize:
                        live = facts.get("resize", {})
                        checks.append(
                            (
                                "the diff band survives a live resize to 90x30",
                                live.get("found") and live.get("bg") == expected_bg,
                                "found=%s bg=%s expected=%s"
                                % (live.get("found"), live.get("bg"), expected_bg),
                            )
                        )
                    if scroll and exp.get("scroll_needle") and mode == "truecolor":
                        sc = facts.get("scroll", {})
                        # The inline/context rows may sit above the long block
                        # on a small viewport; use the frame that shows them.
                        frame = facts.get("scroll_top") or sc
                        checks.append(
                            (
                                "the scrolled code row keeps its band",
                                sc.get("bg") == expected_bg,
                                "bg=%s expected=%s" % (sc.get("bg"), expected_bg),
                            )
                        )
                        if exp.get("context_needle"):
                            checks.append(
                                (
                                    "scrolled context rows stay on the canvas",
                                    frame.get("context_bg") is not None
                                    and frame.get("context_bg") != expected_bg,
                                    "context_bg=%s band=%s"
                                    % (frame.get("context_bg"), expected_bg),
                                )
                            )
                        if exp.get("removed_needle"):
                            checks.append(
                                (
                                    "the scrolled removed row keeps the removal band",
                                    frame.get("removed_bg") == bands["remove"],
                                    "removed=%s expected=%s"
                                    % (frame.get("removed_bg"), bands["remove"]),
                                )
                            )
                        if exp.get("changed") and exp.get("unchanged"):
                            checks.append(
                                (
                                    "the scrolled inline change is stronger than its row band",
                                    frame.get("changed_bg") == bands["add_inline"]
                                    and frame.get("unchanged_bg") == bands["add"],
                                    "changed=%s unchanged=%s expected_changed=%s"
                                    % (
                                        frame.get("changed_bg"),
                                        frame.get("unchanged_bg"),
                                        bands["add_inline"],
                                    ),
                                )
                            )
                            if exp.get("removed_changed"):
                                checks.append(
                                    (
                                        "the scrolled removed inline change is stronger",
                                        frame.get("removed_changed_bg")
                                        == bands["remove_inline"],
                                        "removed_changed=%s expected=%s"
                                        % (
                                            frame.get("removed_changed_bg"),
                                            bands["remove_inline"],
                                        ),
                                    )
                                )
                if scroll and expect == "after" and exp.get("scroll_needle"):
                    sc = facts.get("scroll", {})
                    leaks = list(sc.get("leak_rows") or []) + list(
                        (facts.get("scroll_top") or {}).get("leak_rows") or []
                    )
                    checks.append(
                        (
                            "the long diff scrolls with no stale band cells",
                            sc.get("found") and not leaks,
                            "found=%s leak_rows=%s cols=%s rows=%s"
                            % (
                                sc.get("found"),
                                leaks,
                                sc.get("cols"),
                                sc.get("rows"),
                            ),
                        )
                    )
                if (
                    scroll
                    and exp.get("changed")
                    and expect == "after"
                    and mode in ("truecolor", "ansi256")
                    and exp.get("scroll_needle")
                ):
                    sc = facts.get("scroll_top") or facts.get("scroll", {})
                    inline_hex = (
                        bands["add_inline"]
                        if mode == "truecolor"
                        else BANDS_256[theme]["add_inline"]
                    )
                    checks.append(
                        (
                            "the scrolled inline change keeps its stronger band",
                            sc.get("changed_bg") == inline_hex,
                            "changed=%s expected=%s"
                            % (sc.get("changed_bg"), inline_hex),
                        )
                    )
                    if mode == "truecolor":
                        want_ink = INK[theme][exp.get("changed_role", "variable")]
                        observed = sc.get("changed_fg")
                        # A token too close to the inline band is deliberately
                        # mixed toward black/white; that is the only permitted
                        # deviation from the theme ink, and it must still be a
                        # near shade of it (never the direction green/red).
                        corrected = (
                            observed is not None
                            and contrast_ratio(want_ink, inline_hex) < 4.5
                            and contrast_ratio(observed, inline_hex) >= 4.5
                            and channel_distance(observed, want_ink) <= 200
                        )
                        checks.append(
                            (
                                "the scrolled inline change keeps its theme ink",
                                observed == want_ink or corrected,
                                "changed_fg=%s want=%s corrected=%s bg=%s"
                                % (observed, want_ink, corrected, inline_hex),
                            )
                        )
                    else:
                        checks.append(
                            (
                                "the scrolled 256 inline change stays readable",
                                contrast_ratio(sc.get("changed_fg"), inline_hex)
                                >= 4.5,
                                "changed_fg=%s bg=%s contrast=%.2f"
                                % (
                                    sc.get("changed_fg"),
                                    inline_hex,
                                    contrast_ratio(
                                        sc.get("changed_fg"), inline_hex
                                    ),
                                ),
                            )
                        )
                if (
                    exp.get("changed")
                    and expect == "after"
                    and mode == "truecolor"
                    and not exp.get("scroll_needle")
                ):
                    checks.append(
                        (
                            "the added changed characters carry the stronger inline band",
                            facts.get("changed_bg") == bands["add_inline"]
                            and facts.get("unchanged_bg") == expected_bg,
                            "changed=%s unchanged=%s row=%s expected_changed=%s"
                            % (
                                facts.get("changed_bg"),
                                facts.get("unchanged_bg"),
                                expected_bg,
                                bands["add_inline"],
                            ),
                        )
                    )
                    if exp.get("removed_changed"):
                        checks.append(
                            (
                                "the removed changed characters carry the stronger removal band",
                                facts.get("removed_changed_bg")
                                == bands["remove_inline"],
                                "removed_changed=%s expected=%s"
                                % (
                                    facts.get("removed_changed_bg"),
                                    bands["remove_inline"],
                                ),
                            )
                        )
                    want_ink = INK[theme][exp.get("changed_role", "variable")]
                    inline_inks = [
                        facts.get("changed_fg"),
                        facts.get("unchanged_fg"),
                        facts.get("removed_changed_fg"),
                    ]
                    checks.append(
                        (
                            "the inline change keeps its theme ink, not direction ink",
                            all(ink == want_ink for ink in inline_inks if ink is not None)
                            and facts.get("changed_fg") == want_ink,
                            "want=%s inline=%s" % (want_ink, inline_inks),
                        )
                    )
                if mode == "none" and expect == "after":
                    sgr = facts["sgr"]
                    checks.append(
                        (
                            "NO_COLOR paints no colour SGR",
                            not any(
                                sgr[key]
                                for key in (
                                    "truecolor_fg",
                                    "truecolor_bg",
                                    "indexed_fg_base16",
                                    "indexed_bg_base16",
                                    "indexed_fg_wide",
                                    "indexed_bg_wide",
                                    "basic_fg",
                                    "basic_bg",
                                )
                            ),
                            json.dumps(sgr),
                        )
                    )
                if mode == "ansi16" and expect == "after":
                    sgr = facts["sgr"]
                    checks.append(
                        (
                            "ANSI-16 degrades to the 16 base colour slots, never RGB/256",
                            sgr["truecolor_fg"] == 0
                            and sgr["truecolor_bg"] == 0
                            and sgr["indexed_fg_wide"] == 0
                            and sgr["indexed_bg_wide"] == 0
                            and sgr["indexed_fg_base16"] + sgr["indexed_bg_base16"] > 0,
                            json.dumps(sgr),
                        )
                    )
                if mode == "ansi256" and expect == "after":
                    sgr = facts["sgr"]
                    row_hex = BANDS_256[theme][exp["kind"]]
                    body_hex = facts["code_cell"]["bg"]
                    inline_hex = BANDS_256[theme]["add_inline"]
                    checks.append(
                        (
                            "ANSI-256 uses indexed colours, never RGB",
                            sgr["truecolor_fg"] == 0
                            and sgr["truecolor_bg"] == 0
                            and sgr["indexed_bg_wide"] > 0,
                            json.dumps(sgr),
                        )
                    )
                    checks.append(
                        (
                            "the 256 row band is the neutral %s step" % row_hex,
                            body_hex == row_hex and row_hex in facts["band_bg"],
                            "body=%s band_bg=%s expected=%s"
                            % (body_hex, facts["band_bg"], row_hex),
                        )
                    )
                    marker_ink = (facts.get("marker_cell") or {}).get("fg")
                    body_ink = facts["code_cell"]["fg"]
                    checks.append(
                        (
                            "the 256 direction marker keeps its coloured ink",
                            marker_ink is not None and marker_ink != body_ink,
                            "marker_fg=%s body_fg=%s" % (marker_ink, body_ink),
                        )
                    )
                    checks.append(
                        (
                            "the 256 code body keeps a readable syntax ink",
                            body_ink is not None
                            and contrast_ratio(body_ink, body_hex) >= 4.5,
                            "body_fg=%s body_bg=%s contrast=%.2f"
                            % (
                                body_ink,
                                body_hex,
                                contrast_ratio(body_ink, body_hex),
                            ),
                        )
                    )
                    if (
                        exp.get("changed")
                        and exp.get("unchanged")
                        and not exp.get("scroll_needle")
                    ):
                        checks.append(
                            (
                                "the 256 inline change keeps the stronger band",
                                facts.get("changed_bg") == inline_hex,
                                "changed=%s expected=%s"
                                % (facts.get("changed_bg"), inline_hex),
                            )
                        )
                        checks.append(
                            (
                                "the 256 inline band is stronger than the row band",
                                channel_distance(
                                    facts.get("unchanged_bg"), inline_hex
                                )
                                > channel_distance(
                                    facts.get("unchanged_bg"), row_hex
                                ),
                                "unchanged=%s inline=%s row=%s"
                                % (
                                    facts.get("unchanged_bg"),
                                    inline_hex,
                                    row_hex,
                                ),
                            )
                        )
                        if exp.get("removed_changed"):
                            removed_inline = BANDS_256[theme]["remove_inline"]
                            checks.append(
                                (
                                    "the 256 removed inline change keeps the stronger band",
                                    facts.get("removed_changed_bg")
                                    == removed_inline,
                                    "removed_changed=%s expected=%s"
                                    % (
                                        facts.get("removed_changed_bg"),
                                        removed_inline,
                                    ),
                                )
                            )
                        changed_ink = facts.get("changed_fg")
                        unchanged_ink = facts.get("unchanged_fg")
                        # A token that collides with the saturated inline band
                        # is mixed toward black/white on purpose; every other
                        # token keeps the unchanged role ink, and the corrected
                        # one must reach 4.5:1 rather than blend into the band.
                        ink_ok = changed_ink == unchanged_ink or (
                            contrast_ratio(unchanged_ink, inline_hex) < 4.5
                            and contrast_ratio(changed_ink, inline_hex) >= 4.5
                        )
                        checks.append(
                            (
                                "the 256 inline change keeps reading ink",
                                ink_ok,
                                "changed_fg=%s unchanged_fg=%s bg=%s"
                                % (changed_ink, unchanged_ink, inline_hex),
                            )
                        )
            tokens = TOKENS.get(case, [])
            if tokens and expect == "after" and ok:
                frames = [
                    frame
                    for frame in (
                        facts.get("tokens"),
                        facts.get("scroll_tokens"),
                        facts.get("scroll_top_tokens"),
                        facts.get("return_tokens"),
                        facts.get("paged_tokens"),
                    )
                    if frame
                ]
                found = {}
                for frame in frames:
                    for needle, entry in frame.items():
                        if entry.get("found") and needle not in found:
                            found[needle] = entry
                missing = [needle for needle, _ in tokens if needle not in found]
                checks.append(
                    (
                        "every sampled syntax token is on a real frame",
                        not missing,
                        "missing=%s" % missing,
                    )
                )
                # The same token seen on the pre-scroll frame and again after
                # paging must keep exactly the same ink: a foreground that
                # changes with viewport position means leaked SGR state. Only
                # claimed when more than one frame actually shows tokens.
                inks_by_needle = {}
                for frame in frames:
                    for needle, entry in frame.items():
                        if entry.get("found"):
                            inks_by_needle.setdefault(needle, []).append(entry["fg"])
                drift = {
                    needle: inks
                    for needle, inks in inks_by_needle.items()
                    if len(set(inks)) > 1
                }
                resampled = sum(1 for inks in inks_by_needle.values() if len(inks) > 1)
                token_frames = sum(
                    1
                    for frame in frames
                    if any(entry.get("found") for entry in frame.values())
                )
                if token_frames > 1:
                    checks.append(
                        (
                            "paging never changes a token's foreground ink",
                            not drift,
                            "%d tokens seen on more than one frame; drift=%s"
                            % (resampled, drift),
                        )
                    )
                paged = facts.get("paged")
                if paged is not None:
                    checks.append(
                        (
                            "paged diff frames carry no stale band cells",
                            not paged["leak_rows"],
                            "pages=%s cols=%s rows=%s missing=%s leak_rows=%s"
                            % (
                                paged["pages"],
                                paged["cols"],
                                paged["rows"],
                                paged["missing"],
                                paged["leak_rows"],
                            ),
                        )
                    )
                if mode == "truecolor":
                    wrong = [
                        "%s(%s)=%s want %s"
                        % (
                            needle,
                            entry["role"],
                            entry["fg"],
                            INK[theme][entry["role"]],
                        )
                        for needle, entry in found.items()
                        if entry["fg"] != INK[theme][entry["role"]]
                    ]
                    checks.append(
                        (
                            "each syntax role carries its theme ink",
                            not wrong,
                            "; ".join(wrong),
                        )
                    )
                if mode in ("truecolor", "ansi256"):
                    inks = {entry["fg"] for entry in found.values()}
                    want_inks = exp.get("min_inks", max(4, len(tokens) - 1))
                    checks.append(
                        (
                            "keyword/string/type/function/variable/comment/number stay distinct",
                            len(inks) >= want_inks,
                            "%d distinct inks (want %d): %s"
                            % (len(inks), want_inks, sorted(inks)),
                        )
                    )
                    bad_contrast = [
                        "%s=%s/%s %.2f"
                        % (
                            needle,
                            entry["fg"],
                            entry["bg"],
                            contrast_ratio(entry["fg"], entry["bg"]),
                        )
                        for needle, entry in found.items()
                        if contrast_ratio(entry["fg"], entry["bg"]) < 4.5
                    ]
                    checks.append(
                        (
                            "every sampled token clears 4.5:1 on its diff surface",
                            not bad_contrast,
                            "; ".join(bad_contrast),
                        )
                    )
            facts["checks"] = [
                {"name": name, "ok": bool(good), "detail": detail}
                for name, good, detail in checks
            ]
            return facts
        finally:
            pty_.close()
    finally:
        mock.stop()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--expect", choices=["before", "after"], required=True)
    parser.add_argument("--bin", default=None)
    parser.add_argument("--theme", choices=["dark", "light", "high-contrast"], default="dark")
    parser.add_argument(
        "--resize",
        action="store_true",
        help="also shrink the live PTY to 90x30 and re-check the band",
    )
    parser.add_argument(
        "--scroll",
        action="store_true",
        help="scroll a long diff with PageUp and check the rebuilt frame",
    )
    parser.add_argument("--cases", default="ts-add,readme-replace,inline-edit")
    parser.add_argument(
        "--modes", default="truecolor,ansi256,ansi16,none"
    )
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)

    cases = [c for c in args.cases.split(",") if c]
    modes = [m for m in args.modes.split(",") if m]
    results = []
    failures = 0
    for case in cases:
        # Colour-mode coverage is required for one representative case only;
        # running all three in all four modes would be slow with no new fact.
        case_modes = modes if case == cases[0] else [m for m in modes if m == "truecolor"]
        for mode in case_modes:
            print("=== %s / %s (%s) ===" % (case, mode, args.expect))
            try:
                facts = run_case(
                    case,
                    mode,
                    args.out,
                    args.expect,
                    args.bin,
                    args.theme,
                    args.resize,
                    args.scroll,
                )
            except Exception as exc:
                import traceback

                traceback.print_exc()
                failures += 1
                results.append({"case": case, "mode": mode, "error": repr(exc)})
                continue
            for check in facts.get("checks", []):
                print(
                    ("  ok   " if check["ok"] else "  FAIL ")
                    + check["name"]
                    + ("" if check["ok"] else " — " + check["detail"])
                )
                if not check["ok"]:
                    failures += 1
            results.append(facts)
    with open(os.path.join(args.out, "facts.json"), "w") as fh:
        json.dump(results, fh, indent=2, ensure_ascii=False)
    print("\nfailures: %d" % failures)
    print("evidence: %s" % args.out)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
