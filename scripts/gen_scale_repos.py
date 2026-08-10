#!/usr/bin/env python3
"""Generate the C1.4 exploration scale ladder repositories.

One repository family, one bug, one fix — rendered at several sizes. The
dependency chain to the defect is IDENTICAL at every scale:

    cmd/telemetryd/main.go        entry point
      -> internal/pipeline        the service that assembles a report
        -> internal/sink          the interface it writes through
          -> internal/sink/rollup the implementation the pipeline selects
            -> internal/window    THE DEFECT lives here

Scale only adds plausible surrounding code: more domain packages, each with a
type, a couple of functions and a test, several of which contain their own
(correct) windowing/merging helpers so a keyword search alone cannot pick the
answer out. Nothing is named after the bug.

The generated tree is deterministic (no randomness, no timestamps) so two
runs at the same scale produce byte-identical repositories, and the sizes are
comparable to each other.

Usage:
    python3 scripts/gen_scale_repos.py                 # all scales
    python3 scripts/gen_scale_repos.py 100 300         # selected scales

Output: fixtures/repos/scale-s<N>/ (git repo, one commit). Not committed —
regenerate on demand, like scripts/fetch_eval_repos.sh does for ripgrep.
"""

from __future__ import annotations

import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCALES = [100, 300, 800, 1500]

# Domain vocabulary for generated packages. Realistic names, none of which
# hints at the defect; several deliberately deal in "windows"/"merge" so the
# obvious searches return many plausible hits.
AREAS = [
    "billing", "catalog", "inventory", "shipping", "pricing", "accounts",
    "sessions", "audit", "quota", "routing", "scheduling", "alerting",
    "ingest", "export", "retention", "sampling", "tagging", "lookup",
    "replay", "backfill", "dedupe", "partition", "compaction", "throttle",
    "checkpoint", "leases", "digest", "ledger", "webhooks", "roster",
]

MODULE = "telemetryd"

# ---------------------------------------------------------------------------
# The invariant core: identical at every scale.
# ---------------------------------------------------------------------------

CORE = {
    "go.mod": f"module {MODULE}\n\ngo 1.21\n",

    "README.md": """# telemetryd

Aggregates event counters into fixed-size time windows and prints a report.

    telemetryd <events-file>

Each input line is `<unix-seconds> <metric> <count>`. Events are bucketed into
windows and reported per metric. See docs/pipeline.md for the stage layout.
""",

    "docs/pipeline.md": """# Pipeline

    cmd/telemetryd   parse arguments, read the event file, print the report
    internal/event   the event record and its parser
    internal/window  bucketing of event timestamps into fixed-size windows
    internal/sink    the Sink interface the pipeline writes through
    internal/sink/rollup  the sink the daemon uses (counts per window)
    internal/pipeline     wires parser + sink together

Windows are half-open: an event at exactly the window boundary belongs to the
window that STARTS at that instant, never to the one that ends there.
""",

    "internal/event/event.go": """package event

// Event is one recorded measurement.
type Event struct {
\tUnix   int64
\tMetric string
\tCount  int64
}
""",

    "internal/event/parse.go": """package event

import (
\t"fmt"
\t"strconv"
\t"strings"
)

// Parse reads `<unix-seconds> <metric> <count>` lines.
func Parse(text string) ([]Event, error) {
\tvar events []Event
\tfor n, line := range strings.Split(strings.TrimSpace(text), "\\n") {
\t\tline = strings.TrimSpace(line)
\t\tif line == "" || strings.HasPrefix(line, "#") {
\t\t\tcontinue
\t\t}
\t\tfields := strings.Fields(line)
\t\tif len(fields) != 3 {
\t\t\treturn nil, fmt.Errorf("line %d: want `<unix> <metric> <count>`, got %q", n+1, line)
\t\t}
\t\tunix, err := strconv.ParseInt(fields[0], 10, 64)
\t\tif err != nil {
\t\t\treturn nil, fmt.Errorf("line %d: bad timestamp %q", n+1, fields[0])
\t\t}
\t\tcount, err := strconv.ParseInt(fields[2], 10, 64)
\t\tif err != nil {
\t\t\treturn nil, fmt.Errorf("line %d: bad count %q", n+1, fields[2])
\t\t}
\t\tevents = append(events, Event{Unix: unix, Metric: fields[1], Count: count})
\t}
\tif len(events) == 0 {
\t\treturn nil, fmt.Errorf("no events")
\t}
\treturn events, nil
}
""",

    "internal/event/parse_test.go": """package event

import "testing"

func TestParseReadsLines(t *testing.T) {
\tevents, err := Parse("100 hits 3\\n# comment\\n160 hits 4\\n")
\tif err != nil {
\t\tt.Fatal(err)
\t}
\tif len(events) != 2 || events[1].Count != 4 {
\t\tt.Fatalf("unexpected events: %+v", events)
\t}
}

func TestParseRejectsGarbage(t *testing.T) {
\tif _, err := Parse("nonsense\\n"); err == nil {
\t\tt.Fatal("malformed line must be rejected")
\t}
}
""",

    # ---- THE DEFECT ----------------------------------------------------
    # Start() uses a rounding division that pulls an event sitting exactly on
    # a boundary back into the PREVIOUS window, contradicting the documented
    # half-open rule. Only visible end to end.
    "internal/window/window.go": """package window

// Size is the window width in seconds.
const Size int64 = 60

// Start returns the start of the window an instant belongs to.
//
// Windows are half-open: an instant exactly on a boundary starts a new
// window (see docs/pipeline.md).
func Start(unix int64) int64 {
\treturn ((unix + Size/2) / Size) * Size
}

// Label renders a window start for the report.
func Label(start int64) string {
\treturn formatUnix(start)
}
""",

    "internal/window/format.go": """package window

import "fmt"

func formatUnix(unix int64) string {
\treturn fmt.Sprintf("t=%d", unix)
}
""",

    "internal/window/window_test.go": """package window

import "testing"

// Mid-window instants are unambiguous and pass today.
func TestStartBucketsMidWindowInstants(t *testing.T) {
\tif got := Start(75); got != 60 {
\t\tt.Fatalf("Start(75) = %d, want 60", got)
\t}
\tif got := Start(119); got != 60 {
\t\tt.Fatalf("Start(119) = %d, want 60", got)
\t}
}
""",

    "internal/sink/sink.go": """package sink

// Sink accepts events already assigned to a window.
type Sink interface {
\tAdd(windowStart int64, metric string, count int64)
\tReport() []Row
}

// Row is one line of the report.
type Row struct {
\tWindowStart int64
\tMetric      string
\tCount       int64
}
""",

    "internal/sink/rollup/rollup.go": """package rollup

import (
\t"sort"

\t"telemetryd/internal/sink"
)

// Sink totals counts per (window, metric).
type Sink struct {
\ttotals map[key]int64
}

type key struct {
\tstart  int64
\tmetric string
}

func New() *Sink {
\treturn &Sink{totals: map[key]int64{}}
}

func (s *Sink) Add(windowStart int64, metric string, count int64) {
\ts.totals[key{start: windowStart, metric: metric}] += count
}

func (s *Sink) Report() []sink.Row {
\trows := make([]sink.Row, 0, len(s.totals))
\tfor k, count := range s.totals {
\t\trows = append(rows, sink.Row{WindowStart: k.start, Metric: k.metric, Count: count})
\t}
\tsort.Slice(rows, func(i, j int) bool {
\t\tif rows[i].WindowStart != rows[j].WindowStart {
\t\t\treturn rows[i].WindowStart < rows[j].WindowStart
\t\t}
\t\treturn rows[i].Metric < rows[j].Metric
\t})
\treturn rows
}
""",

    "internal/sink/rollup/rollup_test.go": """package rollup

import "testing"

func TestSinkTotalsPerWindowAndMetric(t *testing.T) {
\ts := New()
\ts.Add(60, "hits", 2)
\ts.Add(60, "hits", 3)
\ts.Add(120, "hits", 1)
\trows := s.Report()
\tif len(rows) != 2 || rows[0].Count != 5 || rows[1].Count != 1 {
\t\tt.Fatalf("unexpected rows: %+v", rows)
\t}
}
""",

    "internal/pipeline/pipeline.go": """package pipeline

import (
\t"fmt"
\t"strings"

\t"telemetryd/internal/event"
\t"telemetryd/internal/sink"
\t"telemetryd/internal/window"
)

// Run buckets every event and renders the report.
func Run(events []event.Event, out sink.Sink) string {
\tfor _, e := range events {
\t\tout.Add(window.Start(e.Unix), e.Metric, e.Count)
\t}
\tvar b strings.Builder
\tfor _, row := range out.Report() {
\t\tfmt.Fprintf(&b, "%s %s %d\\n", window.Label(row.WindowStart), row.Metric, row.Count)
\t}
\treturn b.String()
}
""",

    "cmd/telemetryd/main.go": """package main

import (
\t"fmt"
\t"os"

\t"telemetryd/internal/event"
\t"telemetryd/internal/pipeline"
\t"telemetryd/internal/sink/rollup"
)

func main() {
\tif len(os.Args) != 2 {
\t\tfmt.Fprintln(os.Stderr, "usage: telemetryd <events-file>")
\t\tos.Exit(2)
\t}
\tdata, err := os.ReadFile(os.Args[1])
\tif err != nil {
\t\tfmt.Fprintln(os.Stderr, err)
\t\tos.Exit(1)
\t}
\tevents, err := event.Parse(string(data))
\tif err != nil {
\t\tfmt.Fprintln(os.Stderr, err)
\t\tos.Exit(1)
\t}
\tfmt.Print(pipeline.Run(events, rollup.New()))
}
""",
}

# ---------------------------------------------------------------------------
# Filler: plausible domain packages, several of which do their own bucketing
# or merging so a keyword search returns many honest hits.
# ---------------------------------------------------------------------------

def area_files(area: str, index: int) -> dict[str, str]:
    """Four files for one domain package: type, logic, helper, test."""
    pkg = area
    cap = area.capitalize()
    base = f"internal/{area}"
    # Every third package carries its own windowing helper — correct, and a
    # magnet for `grep window` / `grep Start`.
    windowed = index % 3 == 0
    helper = (
        f"""package {pkg}

// bucket groups a timestamp into this area's reporting interval. Unrelated to
// the pipeline's event windows: {area} reports on its own cadence.
func bucket(unix int64, width int64) int64 {{
\tif width <= 0 {{
\t\treturn unix
\t}}
\treturn unix - (unix % width)
}}

// mergeCounts folds b into a, summing shared keys.
func mergeCounts(a, b map[string]int64) map[string]int64 {{
\tout := make(map[string]int64, len(a)+len(b))
\tfor k, v := range a {{
\t\tout[k] = v
\t}}
\tfor k, v := range b {{
\t\tout[k] += v
\t}}
\treturn out
}}
"""
        if windowed
        else f"""package {pkg}

import "strings"

// normalizeName canonicalizes a {area} identifier for comparison.
func normalizeName(raw string) string {{
\treturn strings.ToLower(strings.TrimSpace(raw))
}}
"""
    )
    logic = f"""package {pkg}

import "fmt"

// Register records a {area} entry, rejecting duplicates.
func (s *Store) Register(entry {cap}Entry) error {{
\tif entry.ID == "" {{
\t\treturn fmt.Errorf("{area}: entry needs an id")
\t}}
\tif _, exists := s.entries[entry.ID]; exists {{
\t\treturn fmt.Errorf("{area}: duplicate entry %q", entry.ID)
\t}}
\ts.entries[entry.ID] = entry
\treturn nil
}}

// Lookup returns the entry for an id.
func (s *Store) Lookup(id string) ({cap}Entry, bool) {{
\tentry, ok := s.entries[id]
\treturn entry, ok
}}

// Count reports how many entries are held.
func (s *Store) Count() int {{
\treturn len(s.entries)
}}
"""
    typ = f"""package {pkg}

// {cap}Entry is one {area} record.
type {cap}Entry struct {{
\tID     string
\tLabel  string
\tWeight int64
}}

// Store holds {area} entries by id.
type Store struct {{
\tentries map[string]{cap}Entry
}}

// NewStore builds an empty {area} store.
func NewStore() *Store {{
\treturn &Store{{entries: map[string]{cap}Entry{{}}}}
}}
"""
    test = f"""package {pkg}

import "testing"

func TestStoreRegisterAndLookup(t *testing.T) {{
\ts := NewStore()
\tif err := s.Register({cap}Entry{{ID: "a", Label: "first", Weight: 2}}); err != nil {{
\t\tt.Fatal(err)
\t}}
\tif err := s.Register({cap}Entry{{ID: "a"}}); err == nil {{
\t\tt.Fatal("duplicate id must be rejected")
\t}}
\tif entry, ok := s.Lookup("a"); !ok || entry.Weight != 2 {{
\t\tt.Fatalf("unexpected entry: %+v %v", entry, ok)
\t}}
\tif s.Count() != 1 {{
\t\tt.Fatalf("Count = %d, want 1", s.Count())
\t}}
}}
"""
    files = {
        f"{base}/{pkg}.go": typ,
        f"{base}/store.go": logic,
        f"{base}/helper.go": helper,
        f"{base}/{pkg}_test.go": test,
    }
    if windowed:
        files[f"{base}/helper_test.go"] = f"""package {pkg}

import "testing"

func TestBucketFloorsToWidth(t *testing.T) {{
\tif got := bucket(125, 60); got != 120 {{
\t\tt.Fatalf("bucket(125, 60) = %d, want 120", got)
\t}}
\tif got := bucket(60, 60); got != 60 {{
\t\tt.Fatalf("bucket(60, 60) = %d, want 60", got)
\t}}
}}

func TestMergeCountsSumsSharedKeys(t *testing.T) {{
\tout := mergeCounts(map[string]int64{{"a": 1}}, map[string]int64{{"a": 2, "b": 3}})
\tif out["a"] != 3 || out["b"] != 3 {{
\t\tt.Fatalf("unexpected merge: %+v", out)
\t}}
}}
"""
    return files


def build(scale: int, mapped: bool = True) -> pathlib.Path:
    """Render the family at `scale`. `mapped=False` drops the architecture map
    (the README layout section and docs/pipeline.md) — the single variable that
    separates "the repo tells you where things live" from "you have to find
    out". Everything else, including the defect and its depth, is identical."""
    suffix = "" if mapped else "-nomap"
    dest = ROOT / "fixtures" / "repos" / f"scale-s{scale}{suffix}"
    if dest.exists():
        shutil.rmtree(dest)
    files = dict(CORE)
    if not mapped:
        del files["docs/pipeline.md"]
        files["README.md"] = f"""# {MODULE}

Aggregates event counters into fixed-size time windows and prints a report.

    {MODULE} <events-file>

Each input line is `<unix-seconds> <metric> <count>`.
"""
    index = 0
    while len(files) < scale:
        area = AREAS[index % len(AREAS)]
        suffix = index // len(AREAS)
        name = area if suffix == 0 else f"{area}{suffix + 1}"
        files.update(area_files(name, index))
        index += 1
    for rel, content in files.items():
        path = dest / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    run = lambda *a: subprocess.run(a, cwd=dest, check=True, capture_output=True)
    run("git", "init", "-q")
    run("git", "config", "user.email", "eval@leveler")
    run("git", "config", "user.name", "leveler-eval")
    run("git", "add", "-A")
    run("git", "commit", "-qm", f"scale-s{scale}{suffix} baseline")
    return dest


def main() -> None:
    args = sys.argv[1:]
    mapped = "--no-map" not in args
    wanted = [int(a) for a in args if a != "--no-map"] or SCALES
    for scale in wanted:
        dest = build(scale, mapped)
        count = sum(1 for _ in dest.rglob("*.go"))
        total = sum(1 for p in dest.rglob("*") if p.is_file() and ".git" not in p.parts)
        print(f"{dest.relative_to(ROOT)}: {total} files ({count} .go)")


if __name__ == "__main__":
    main()
