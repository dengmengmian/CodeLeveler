#!/usr/bin/env python3
"""C5-E2 fixture: a history-dependent long-context coding task.

Builds `fixtures/repos/telemetryd-e2`: the scale-s1500 filler family for
repository realism (distractor packages, same telemetryd core), plus a REAL
relevant working set — 56 ingest adapters, each ~15-20KB of differentiated Go
whose parsing behavior carries seeded local quirks along four dimensions:

    fallback   what a missing count value becomes   (zero|skip|last_seen|neg1)
    ordering   what order events leave the adapter  (input|by_metric|by_count|by_ts)
    aliases    legacy metric-name renames           (per-adapter map)
    scale      unit normalization factor            (1|10|1000)

The facts live in the CODE of each adapter's parse path, not in any manifest:
implementing a truthful `Snapshot()` for adapter N requires having read
adapter N. That is what makes the case history-dependent — an agent that read
the early adapters, folded, and later implements their snapshots must come
back for the facts it no longer holds.

The generator also knows every quirk it planted, so it can emit the exact
reference `Snapshot()` for each adapter (--emit-reference) and the acceptance
cross-check drives parse() itself to derive ACTUAL behavior — a snapshot that
lies about its adapter fails on behavior, not on string matching.

Usage:
    python3 scripts/gen_e2_fixture.py                 # build the fixture repo
    python3 scripts/gen_e2_fixture.py --emit-reference <workspace>
"""

from __future__ import annotations

import pathlib
import random
import shutil
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import gen_scale_repos as scale

ROOT = pathlib.Path(__file__).resolve().parent.parent
MODULE = "telemetryd"
ADAPTER_COUNT = 56
SEED = 20260810

FALLBACKS = ("zero", "skip", "last_seen", "neg1")
ORDERINGS = ("input", "by_metric", "by_count", "by_ts")
SCALES = (1, 1, 10, 1000)  # weighted: most adapters don't rescale
ALIAS_POOL = [
    ("reqs", "requests"), ("lat", "latency"), ("err", "errors"),
    ("conns", "connections"), ("mem", "memory"), ("cpu", "cpu_time"),
    ("q", "queue_depth"), ("rt", "round_trip"), ("bw", "bandwidth"),
    ("iops", "io_operations"),
]


def adapter_quirks(name: str, rnd: random.Random) -> dict:
    return dict(
        fallback=rnd.choice(FALLBACKS),
        ordering=rnd.choice(ORDERINGS),
        aliases=dict(rnd.sample(ALIAS_POOL, rnd.randrange(1, 4))),
        scale=rnd.choice(SCALES),
    )


def fallback_code(kind: str) -> str:
    return {
        "zero": """\t\t\t// A blank count is an explicit zero observation for this feed.
\t\t\tvalue = 0""",
        "skip": """\t\t\t// This feed's blanks are transport noise, not observations.
\t\t\tcontinue""",
        "last_seen": """\t\t\t// The exporter repeats the previous reading when it has no
\t\t\t// fresh sample; carry it forward.
\t\t\tvalue = lastSeen[metric]""",
        "neg1": """\t\t\t// Downstream treats -1 as "unknown"; keep the row, mark it.
\t\t\tvalue = -1""",
    }[kind]


def ordering_code(kind: str) -> str:
    return {
        "input": "\t// Arrival order is contractual for this feed; do not sort.\n",
        "by_metric": """\tsort.SliceStable(out, func(i, j int) bool { return out[i].Metric < out[j].Metric })
""",
        "by_count": """\tsort.SliceStable(out, func(i, j int) bool { return out[i].Count > out[j].Count })
""",
        "by_ts": """\tsort.SliceStable(out, func(i, j int) bool { return out[i].Unix < out[j].Unix })
""",
    }[kind]


EXTRA_TEMPLATE = r"""
// transport framing ------------------------------------------------------------

// frameState tracks the collector's line-reassembly state for this feed. Some
// exporters flush mid-line on rotation; the collector hands us fragments and
// we must not fabricate observations from them.
type frameState struct {{
	partial   string
	fragments int
	recovered int
}}

// Reassemble merges transport fragments back into whole lines. A fragment is
// any chunk not ending in a newline; it prefixes the next chunk. The bounded
// buffer (64KB) protects against a wedged exporter streaming one endless line.
func (f *frameState) Reassemble(chunk string) []string {{
	const maxPartial = 64 * 1024
	data := f.partial + chunk
	f.partial = ""
	var lines []string
	for {{
		idx := strings.IndexByte(data, '\n')
		if idx < 0 {{
			break
		}}
		lines = append(lines, data[:idx])
		data = data[idx+1:]
	}}
	if len(data) > 0 {{
		if len(data) > maxPartial {{
			f.fragments++
			data = ""
		}} else {{
			f.partial = data
			f.recovered++
		}}
	}}
	return lines
}}

// retry policy -----------------------------------------------------------------

// backoffSchedule is this feed's reconnect ladder in milliseconds. Tuned per
// exporter: chatty feeds reconnect fast, batch feeds are patient.
var backoffSchedule = []int{{{backoff}}}

// NextBackoff returns the delay before reconnect attempt n (0-based),
// saturating at the ladder's last rung.
func NextBackoff(attempt int) int {{
	if attempt < 0 {{
		attempt = 0
	}}
	if attempt >= len(backoffSchedule) {{
		return backoffSchedule[len(backoffSchedule)-1]
	}}
	return backoffSchedule[attempt]
}}

// checkpointing ----------------------------------------------------------------

// Checkpoint is the high-water mark the collector persists so a restart does
// not re-ingest this feed's already-processed window.
type Checkpoint struct {{
	LastUnix  int64
	LineCount int64
}}

// Advance folds one accepted event into the checkpoint. Regressing timestamps
// (exporter clock skew) never move the mark backwards.
func (c *Checkpoint) Advance(unix int64) {{
	c.LineCount++
	if unix > c.LastUnix {{
		c.LastUnix = unix
	}}
}}

// Stale reports whether the feed has been silent past its tolerance —
// {stale_min} minutes for this exporter's expected cadence.
func (c *Checkpoint) Stale(nowUnix int64) bool {{
	return nowUnix-c.LastUnix > {stale_secs}
}}

// health signals ---------------------------------------------------------------

// healthThresholds are this feed's alerting knobs, chosen from its historical
// baseline rather than a global default.
var healthThresholds = struct {{
	MaxDropRate   float64
	MaxRenameRate float64
	MinAcceptRate float64
}}{{
	MaxDropRate:   {drop_rate},
	MaxRenameRate: {rename_rate},
	MinAcceptRate: {accept_rate},
}}

// Healthy evaluates ingest statistics against this feed's thresholds.
func Healthy(s Stats) bool {{
	total := s.Accepted + s.Dropped
	if total == 0 {{
		return true
	}}
	dropRate := float64(s.Dropped) / float64(total)
	acceptRate := float64(s.Accepted) / float64(total)
	renameRate := float64(s.Renamed) / float64(total)
	if dropRate > healthThresholds.MaxDropRate {{
		return false
	}}
	if renameRate > healthThresholds.MaxRenameRate {{
		return false
	}}
	return acceptRate >= healthThresholds.MinAcceptRate
}}

// batch shaping ----------------------------------------------------------------

// batchLimit caps how many events one Parse call forwards downstream in a
// single slice; the remainder is chunked. Sized to this feed's typical burst.
const batchLimit = {batch_limit}

// Chunk splits events into forwarding batches of at most batchLimit.
func Chunk(events []Event) [][]Event {{
	if len(events) == 0 {{
		return nil
	}}
	var out [][]Event
	for start := 0; start < len(events); start += batchLimit {{
		end := start + batchLimit
		if end > len(events) {{
			end = len(events)
		}}
		out = append(out, events[start:end])
	}}
	return out
}}
"""


def rnd_local(name: str) -> random.Random:
    return random.Random(f"{SEED}:{name}")


def adapter_source(name: str, q: dict) -> str:
    """~15-20KB of real adapter: parse with the quirks, plus the plausible
    machinery a feed adapter actually carries (validation, framing, stats)."""
    alias_lines = "\n".join(
        f'\t"{a}": "{b}",' for a, b in sorted(q["aliases"].items()))
    needs_sort = q["ordering"] != "input"
    needs_last = q["fallback"] == "last_seen"
    imports = ['"strconv"', '"strings"']
    if needs_sort:
        imports.append('"sort"')
    imports_block = "\n\t".join(sorted(imports))
    scale_code = (
        "\t\t// This feed reports in raw units.\n"
        if q["scale"] == 1 else
        f"\t\t// This feed reports in 1/{q['scale']} units; normalize on ingest.\n"
        f"\t\tvalue *= {q['scale']}\n"
    )
    last_decl = "\tlastSeen := map[string]int64{}\n" if needs_last else ""
    last_track = "\t\tlastSeen[metric] = value\n" if needs_last else ""
    helpers = f"""
// frame validation ------------------------------------------------------------

// wellFormed rejects transport frames this feed is known to emit when the
// collector restarts: leading NULs, truncated tails, bare separators.
func wellFormed(line string) bool {{
\tif line == "" || strings.HasPrefix(line, "\\x00") {{
\t\treturn false
\t}}
\tif strings.Count(line, " ") < 1 {{
\t\treturn false
\t}}
\treturn true
}}

// canonicalMetric applies this feed's legacy renames. The alias table is part
// of the adapter's public contract: dashboards depend on the canonical names.
func canonicalMetric(raw string) string {{
\tif mapped, ok := metricAliases[raw]; ok {{
\t\treturn mapped
\t}}
\treturn raw
}}

var metricAliases = map[string]string{{
{alias_lines}
}}

// ingest statistics ------------------------------------------------------------

// Stats counts what this adapter did with its input; surfaced by the
// collector's debug endpoint.
type Stats struct {{
\tAccepted int
\tDropped  int
\tRenamed  int
}}

func (s *Stats) note(accepted, renamed bool) {{
\tif accepted {{
\t\ts.Accepted++
\t}} else {{
\t\ts.Dropped++
\t}}
\tif renamed {{
\t\ts.Renamed++
\t}}
}}
"""
    extra = EXTRA_TEMPLATE.format(
        backoff=", ".join(str(v) for v in sorted(rnd_local(name).sample(
            (100, 250, 500, 1000, 2000, 5000, 10000, 30000), 5))),
        stale_min=rnd_local(name).randrange(2, 30),
        stale_secs=rnd_local(name).randrange(2, 30) * 60,
        drop_rate=round(rnd_local(name).uniform(0.01, 0.2), 2),
        rename_rate=round(rnd_local(name).uniform(0.1, 0.9), 2),
        accept_rate=round(rnd_local(name).uniform(0.5, 0.95), 2),
        batch_limit=rnd_local(name).choice((64, 128, 256, 512, 1024)),
    )
    return f"""package {name}

// Feed adapter "{name}". Every feed speaks the shared line protocol
// `<unix> <metric> <count>` but each exporter has its own dialect: how blanks
// are meant, whether arrival order is contractual, which legacy names must be
// canonicalized, and what unit the counts arrive in. Those dialect rules are
// THIS file's contract — downstream aggregation assumes the adapter has
// already normalized them.

import (
\t{imports_block}

\t"{MODULE}/internal/adapterspec"
)

// Event is one normalized observation leaving this adapter.
type Event struct {{
\tUnix   int64
\tMetric string
\tCount  int64
}}

// Parse normalizes raw feed lines into events, applying this feed's dialect.
func Parse(lines []string) []Event {{
\tvar out []Event
\tstats := &Stats{{}}
{last_decl}\tfor _, line := range lines {{
\t\tline = strings.TrimRight(line, "\\r\\n")
\t\tif !wellFormed(line) {{
\t\t\tstats.note(false, false)
\t\t\tcontinue
\t\t}}
\t\tfields := strings.Fields(line)
\t\tif len(fields) < 2 {{
\t\t\tstats.note(false, false)
\t\t\tcontinue
\t\t}}
\t\tunix, err := strconv.ParseInt(fields[0], 10, 64)
\t\tif err != nil {{
\t\t\tstats.note(false, false)
\t\t\tcontinue
\t\t}}
\t\traw := fields[1]
\t\tmetric := canonicalMetric(raw)
\t\tvar value int64
\t\tif len(fields) < 3 || fields[2] == "" {{
{fallback_code(q["fallback"])}
\t\t}} else {{
\t\t\tvalue, err = strconv.ParseInt(fields[2], 10, 64)
\t\t\tif err != nil {{
\t\t\t\tstats.note(false, false)
\t\t\t\tcontinue
\t\t\t}}
\t\t}}
{scale_code}{last_track}\t\tstats.note(true, metric != raw)
\t\tout = append(out, Event{{Unix: unix, Metric: metric, Count: value}})
\t}}
{ordering_code(q["ordering"])}\treturn out
}}

// Spec placeholder: the unified inventory work adds this adapter's
// self-description here. See internal/adapterspec.
var _ = adapterspec.Summary{{}}
{helpers}{extra}"""


def adapter_test(name: str, q: dict) -> str:
    """A local maintained test pinning ONE visible aspect (so the suite is
    meaningful) without disclosing the whole dialect."""
    a, b = sorted(q["aliases"].items())[0]
    extra = EXTRA_TEMPLATE.format(
        backoff=", ".join(str(v) for v in sorted(rnd_local(name).sample(
            (100, 250, 500, 1000, 2000, 5000, 10000, 30000), 5))),
        stale_min=rnd_local(name).randrange(2, 30),
        stale_secs=rnd_local(name).randrange(2, 30) * 60,
        drop_rate=round(rnd_local(name).uniform(0.01, 0.2), 2),
        rename_rate=round(rnd_local(name).uniform(0.1, 0.9), 2),
        accept_rate=round(rnd_local(name).uniform(0.5, 0.95), 2),
        batch_limit=rnd_local(name).choice((64, 128, 256, 512, 1024)),
    )
    return f"""package {name}

import "testing"

func TestCanonicalRename(t *testing.T) {{
\tgot := Parse([]string{{"100 {a} 5"}})
\tif len(got) != 1 || got[0].Metric != "{b}" {{
\t\tt.Fatalf("legacy name must canonicalize: %+v", got)
\t}}
}}
"""


SPEC_PKG = f"""package adapterspec

// Summary is an adapter's self-description of its dialect. The inventory
// command aggregates these, and downstream tooling TRUSTS them — a summary
// that misstates its adapter's actual behavior is a bug of the worst kind,
// so acceptance cross-checks every claim against the adapter's real output.
type Summary struct {{
\t// Fallback names what a missing count becomes: "zero", "skip",
\t// "last_seen", or "neg1".
\tFallback string
\t// Ordering names the output order contract: "input", "by_metric",
\t// "by_count", or "by_ts".
\tOrdering string
\t// Aliases lists this adapter's legacy renames (raw -> canonical).
\tAliases map[string]string
\t// Scale is the unit multiplier applied on ingest (1 when none).
\tScale int64
}}
"""


def build() -> pathlib.Path:
    rnd = random.Random(SEED)
    dest = ROOT / "fixtures" / "repos" / "telemetryd-e2"
    if dest.exists():
        shutil.rmtree(dest)

    files = dict(scale.CORE)
    # The scale family plants its own task's defect (rounding window Start) in
    # CORE. E2's task is the adapter inventory — its substrate must be healthy,
    # or the maintained suite is red for reasons unrelated to this case.
    for rel, content in files.items():
        if "func Start(unix int64) int64" in content:
            files[rel] = content.replace(
                "\treturn ((unix + Size/2) / Size) * Size",
                "\treturn (unix / Size) * Size",
            )
    files["internal/adapterspec/spec.go"] = SPEC_PKG

    names = []
    for i in range(ADAPTER_COUNT):
        name = f"feed{i + 1:02d}"
        names.append(name)
        q = adapter_quirks(name, rnd)
        files[f"internal/adapters/{name}/{name}.go"] = adapter_source(name, q)
        files[f"internal/adapters/{name}/{name}_test.go"] = adapter_test(name, q)

    files["internal/adapters/registry.go"] = (
        "package adapters\n\n// Registered feed adapter packages, one per exporter dialect:\n"
        + "".join(f"// - internal/adapters/{n}\n" for n in names)
    )

    # filler to repository scale, same machinery as scale-s1500
    index = 0
    while len(files) < 1500:
        area = scale.AREAS[index % len(scale.AREAS)]
        suffix = index // len(scale.AREAS)
        pkg = area if suffix == 0 else f"{area}{suffix + 1}"
        files.update(scale.area_files(pkg, index))
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
    run("git", "commit", "-qm", "telemetryd-e2 baseline")
    print(f"{dest}: {len(files)} files, {ADAPTER_COUNT} relevant adapters")
    return dest


def emit_reference(workspace: str) -> None:
    """Write the correct Snapshot() for every adapter into `workspace` (the
    generator knows the quirks it planted) plus the inventory command."""
    rnd = random.Random(SEED)
    ws = pathlib.Path(workspace)
    names = []
    for i in range(ADAPTER_COUNT):
        name = f"feed{i + 1:02d}"
        names.append(name)
        q = adapter_quirks(name, rnd)
        alias_lines = "\n".join(
            f'\t\t"{a}": "{b}",' for a, b in sorted(q["aliases"].items()))
        snapshot = f"""package {name}

import "{MODULE}/internal/adapterspec"

// Snapshot reports this adapter's dialect, exactly as Parse implements it.
func Snapshot() adapterspec.Summary {{
\treturn adapterspec.Summary{{
\t\tFallback: "{q['fallback']}",
\t\tOrdering: "{q['ordering']}",
\t\tAliases: map[string]string{{
{alias_lines}
\t\t}},
\t\tScale: {q['scale']},
\t}}
}}
"""
        (ws / f"internal/adapters/{name}/snapshot.go").write_text(snapshot)

    imports = "\n".join(
        f'\t{n} "{MODULE}/internal/adapters/{n}"' for n in names)
    rows = "\n".join(
        f'\t\t{{"{n}", {n}.Snapshot()}},' for n in names)
    inventory = f"""package main

import (
\t"fmt"
\t"sort"

\t"{MODULE}/internal/adapterspec"
{imports}
)

type inventoryRow struct {{
\tName    string
\tSummary adapterspec.Summary
}}

// inventoryRows collects every registered adapter's self-description.
func inventoryRows() []inventoryRow {{
\trows := []inventoryRow{{
{rows}
\t}}
\tsort.Slice(rows, func(i, j int) bool {{ return rows[i].Name < rows[j].Name }})
\treturn rows
}}

func printInventory() {{
\tfor _, row := range inventoryRows() {{
\t\tfmt.Printf("%s fallback=%s ordering=%s aliases=%d scale=%d\\n",
\t\t\trow.Name, row.Summary.Fallback, row.Summary.Ordering,
\t\t\tlen(row.Summary.Aliases), row.Summary.Scale)
\t}}
}}
"""
    (ws / "cmd/telemetryd/inventory.go").write_text(inventory)
    # wire the subcommand into main
    main_path = ws / "cmd/telemetryd/main.go"
    main_src = main_path.read_text()
    hook = 'if len(os.Args) > 1 && os.Args[1] == "inventory" {\n\t\tprintInventory()\n\t\treturn\n\t}\n\t'
    if "printInventory" not in main_src:
        marker = main_src.index("func main() {") + len("func main() {")
        main_src = main_src[:marker] + "\n\t" + hook + main_src[marker + 1:]
        main_path.write_text(main_src)
    print(f"reference emitted into {ws} ({ADAPTER_COUNT} snapshots + inventory)")


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--emit-reference":
        emit_reference(sys.argv[2])
    else:
        build()
