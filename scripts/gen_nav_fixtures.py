#!/usr/bin/env python3
"""Generate the shared repository the N1-N8 navigation eval runs against.

A navigation benchmark needs somewhere to get lost. The C1 fixtures are six
files each, so locating anything costs one search — which is why C2.3B could
not tell whether navigation guidance helped. This builds one realistic Go
service instead, and the eight cases overlay a defect or a feature onto it.

What makes it navigable-but-not-trivial, by construction:

  dispatch indirection   sinks and stages are chosen through a registry, so
                         the name in the task never points straight at the
                         code that runs
  live/dead pairs        legacy/ and examples/ hold older implementations
                         whose names are *closer* to how a user would phrase
                         the request than the live ones
  a real large file      decoder.go is ~2000 lines of plausible sections with
                         several similar-looking symbols, target in the middle
  chains that compile    config → load → validate → runtime → behaviour, where
                         dropping a link still builds

Written once, reproducibly, with no randomness: same input, same repo.

    python3 scripts/gen_nav_fixtures.py [--out fixtures/repos/navsvc]
"""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess

MODULE = "navsvc"


def go_mod() -> str:
    return f"module {MODULE}\n\ngo 1.21\n"


# ---------------------------------------------------------------- config ----

CONFIG = {
    "internal/config/config.go": '''package config

// Config is the parsed, validated runtime configuration.
type Config struct {
	Input     InputConfig
	Pipeline  PipelineConfig
	Output    OutputConfig
}

type InputConfig struct {
	Path      string
	Format    string
	BatchSize int
}

type PipelineConfig struct {
	Stages      []string
	DropInvalid bool
	// MaxDepth bounds nested record expansion.
	MaxDepth int
}

type OutputConfig struct {
	Sink   string
	Pretty bool
}
''',
    "internal/config/defaults.go": '''package config

// Defaults returns the configuration used when a file omits a field. Every
// zero value that is not a sensible default has to be listed here.
func Defaults() Config {
	return Config{
		Input: InputConfig{
			Format:    "jsonl",
			BatchSize: 128,
		},
		Pipeline: PipelineConfig{
			Stages:   []string{"normalize"},
			MaxDepth: 4,
		},
		Output: OutputConfig{
			Sink: "stdout",
		},
	}
}
''',
    "internal/config/load.go": '''package config

import (
	"bufio"
	"fmt"
	"os"
	"strconv"
	"strings"
)

// Load reads a `key = value` configuration file and merges it over Defaults.
// Unknown keys are an error: a typo that silently does nothing is worse than
// a refusal.
func Load(path string) (Config, error) {
	cfg := Defaults()
	file, err := os.Open(path)
	if err != nil {
		return cfg, fmt.Errorf("open config: %w", err)
	}
	defer file.Close()

	scanner := bufio.NewScanner(file)
	line := 0
	for scanner.Scan() {
		line++
		text := strings.TrimSpace(scanner.Text())
		if text == "" || strings.HasPrefix(text, "#") {
			continue
		}
		key, value, ok := strings.Cut(text, "=")
		if !ok {
			return cfg, fmt.Errorf("config line %d: expected key = value", line)
		}
		key = strings.TrimSpace(key)
		value = strings.TrimSpace(value)
		if err := assign(&cfg, key, value); err != nil {
			return cfg, fmt.Errorf("config line %d: %w", line, err)
		}
	}
	return cfg, scanner.Err()
}

func assign(cfg *Config, key, value string) error {
	switch key {
	case "input.path":
		cfg.Input.Path = value
	case "input.format":
		cfg.Input.Format = value
	case "input.batch_size":
		n, err := strconv.Atoi(value)
		if err != nil {
			return fmt.Errorf("input.batch_size: %w", err)
		}
		cfg.Input.BatchSize = n
	case "pipeline.stages":
		cfg.Pipeline.Stages = splitList(value)
	case "pipeline.drop_invalid":
		cfg.Pipeline.DropInvalid = value == "true"
	case "pipeline.max_depth":
		n, err := strconv.Atoi(value)
		if err != nil {
			return fmt.Errorf("pipeline.max_depth: %w", err)
		}
		cfg.Pipeline.MaxDepth = n
	case "output.sink":
		cfg.Output.Sink = value
	case "output.pretty":
		cfg.Output.Pretty = value == "true"
	default:
		return fmt.Errorf("unknown key %q", key)
	}
	return nil
}

func splitList(value string) []string {
	parts := strings.Split(value, ",")
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		if trimmed := strings.TrimSpace(p); trimmed != "" {
			out = append(out, trimmed)
		}
	}
	return out
}
''',
    "internal/config/validate.go": '''package config

import "fmt"

// Validate rejects a configuration that would fail later in a confusing place.
func Validate(cfg Config) error {
	if cfg.Input.BatchSize <= 0 {
		return fmt.Errorf("input.batch_size must be positive, got %d", cfg.Input.BatchSize)
	}
	if cfg.Pipeline.MaxDepth < 0 {
		return fmt.Errorf("pipeline.max_depth must not be negative")
	}
	switch cfg.Input.Format {
	case "jsonl", "csv":
	default:
		return fmt.Errorf("unsupported input.format %q", cfg.Input.Format)
	}
	return nil
}
''',
}

# ---------------------------------------------------------------- record ----

API = {
    "pkg/api/types.go": '''package api

// Record is one ingested row after decoding.
type Record struct {
	Name   string
	Value  float64
	Labels map[string]string
	// Depth records how deeply nested this record was in its source document.
	Depth int
	// Valid is false when a decoder could parse the shape but not the content.
	Valid bool
}

// Batch is a decoded group of records handed to the pipeline as a unit.
type Batch struct {
	Records []Record
	Source  string
}

// Len reports how many records the batch carries.
func (b Batch) Len() int { return len(b.Records) }
''',
    "pkg/api/encode.go": '''package api

import (
	"fmt"
	"sort"
	"strings"
)

// Encode renders a record in the service's canonical single-line form.
func Encode(r Record) string {
	var b strings.Builder
	fmt.Fprintf(&b, "%s=%g", r.Name, r.Value)
	if len(r.Labels) > 0 {
		keys := make([]string, 0, len(r.Labels))
		for k := range r.Labels {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			fmt.Fprintf(&b, " %s=%s", k, r.Labels[k])
		}
	}
	return b.String()
}
''',
}

# ---------------------------------------------------------------- ingest ----

INGEST = {
    "internal/ingest/reader.go": '''package ingest

import (
	"bufio"
	"io"
	"navsvc/internal/config"
	"navsvc/pkg/api"
)

// Reader turns an input stream into batches sized by configuration.
type Reader struct {
	decoder   Decoder
	batchSize int
	source    string
}

// NewReader picks a decoder for the configured format. An unknown format is
// caught by config.Validate long before this runs.
func NewReader(cfg config.InputConfig, source string) *Reader {
	return &Reader{
		decoder:   decoderFor(cfg.Format),
		batchSize: cfg.BatchSize,
		source:    source,
	}
}

// Read consumes the whole stream, emitting batches as they fill.
func (r *Reader) Read(in io.Reader, emit func(api.Batch) error) error {
	scanner := bufio.NewScanner(in)
	scanner.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	batch := api.Batch{Source: r.source}
	for scanner.Scan() {
		record, ok := r.decoder.DecodeLine(scanner.Bytes())
		if !ok {
			continue
		}
		batch.Records = append(batch.Records, record)
		if len(batch.Records) >= r.batchSize {
			if err := emit(batch); err != nil {
				return err
			}
			batch = api.Batch{Source: r.source}
		}
	}
	if len(batch.Records) > 0 {
		if err := emit(batch); err != nil {
			return err
		}
	}
	return scanner.Err()
}
''',
    "internal/ingest/registry.go": '''package ingest

// decoderFor resolves a format name to its decoder. Formats register here
// rather than being switched on at the call site, so adding one does not
// require touching the reader.
func decoderFor(format string) Decoder {
	if d, ok := decoders[format]; ok {
		return d
	}
	return decoders["jsonl"]
}

var decoders = map[string]Decoder{
	"jsonl": jsonlDecoder{},
	"csv":   csvDecoder{},
}
''',
}

# --------------------------------------------------------------- pipeline ---

PIPELINE = {
    "internal/pipeline/pipeline.go": '''package pipeline

import (
	"navsvc/internal/config"
	"navsvc/pkg/api"
)

// Pipeline applies the configured stages to each batch in order.
type Pipeline struct {
	stages []Stage
	cfg    config.PipelineConfig
}

// New builds a pipeline from the configured stage names. Unknown names are
// skipped: configuration validation is the place that refuses them.
func New(cfg config.PipelineConfig) *Pipeline {
	p := &Pipeline{cfg: cfg}
	for _, name := range cfg.Stages {
		if stage, ok := stageFor(name); ok {
			p.stages = append(p.stages, stage)
		}
	}
	return p
}

// Apply runs every stage over the batch and returns what survived.
func (p *Pipeline) Apply(batch api.Batch) api.Batch {
	for _, stage := range p.stages {
		batch = stage.Run(batch, p.cfg)
	}
	return batch
}
''',
    "internal/pipeline/stage.go": '''package pipeline

import (
	"navsvc/internal/config"
	"navsvc/pkg/api"
)

// Stage is one transformation applied to a batch.
type Stage interface {
	// Name is the identifier used in `pipeline.stages`.
	Name() string
	// Run returns the batch after this stage's transformation.
	Run(batch api.Batch, cfg config.PipelineConfig) api.Batch
}

// stageFor resolves a configured stage name to its implementation. Stages
// register in this table; the name in a config file never names a Go symbol
// directly.
func stageFor(name string) (Stage, bool) {
	stage, ok := stages[name]
	return stage, ok
}

var stages = map[string]Stage{
	"normalize": normalizeStage{},
	"filter":    filterStage{},
	"flatten":   flattenStage{},
}
''',
    "internal/pipeline/transform.go": '''package pipeline

import (
	"strings"

	"navsvc/internal/config"
	"navsvc/pkg/api"
)

// normalizeStage lowercases names and trims label whitespace so downstream
// aggregation groups equivalent records together.
type normalizeStage struct{}

func (normalizeStage) Name() string { return "normalize" }

func (normalizeStage) Run(batch api.Batch, _ config.PipelineConfig) api.Batch {
	for i := range batch.Records {
		batch.Records[i].Name = strings.ToLower(strings.TrimSpace(batch.Records[i].Name))
		for k, v := range batch.Records[i].Labels {
			batch.Records[i].Labels[k] = strings.TrimSpace(v)
		}
	}
	return batch
}

// flattenStage collapses records nested deeper than the configured maximum
// onto that maximum, so a pathological document cannot produce unbounded
// depth downstream.
type flattenStage struct{}

func (flattenStage) Name() string { return "flatten" }

func (flattenStage) Run(batch api.Batch, cfg config.PipelineConfig) api.Batch {
	for i := range batch.Records {
		if batch.Records[i].Depth > cfg.MaxDepth {
			batch.Records[i].Depth = cfg.MaxDepth
		}
	}
	return batch
}
''',
    "internal/pipeline/filter.go": '''package pipeline

import (
	"navsvc/internal/config"
	"navsvc/pkg/api"
)

// filterStage removes records a decoder marked unusable.
type filterStage struct{}

func (filterStage) Name() string { return "filter" }

func (filterStage) Run(batch api.Batch, cfg config.PipelineConfig) api.Batch {
	if !cfg.DropInvalid {
		return batch
	}
	kept := batch.Records[:0]
	for _, record := range batch.Records {
		if record.Valid {
			kept = append(kept, record)
		}
	}
	batch.Records = kept
	return batch
}
''',
}

# ------------------------------------------------------------------ sink ----

SINK = {
    "internal/sink/sink.go": '''package sink

import "navsvc/pkg/api"

// Sink receives batches after the pipeline has run.
//
// Write is called once per batch, in order. Close is called exactly once, and
// an implementation that buffers must flush there — a sink whose output only
// appears on Close is still correct.
type Sink interface {
	Write(batch api.Batch) error
	Close() error
}

// For resolves a configured sink name to its implementation. Like stages,
// sinks are registered rather than switched on.
func For(name string) (Sink, bool) {
	build, ok := sinks[name]
	if !ok {
		return nil, false
	}
	return build(), true
}

var sinks = map[string]func() Sink{
	"stdout": func() Sink { return newStdoutSink() },
	"file":   func() Sink { return newFileSink() },
	"null":   func() Sink { return nullSink{} },
}
''',
    "internal/sink/stdout.go": '''package sink

import (
	"bufio"
	"fmt"
	"os"

	"navsvc/pkg/api"
)

type stdoutSink struct {
	out *bufio.Writer
}

func newStdoutSink() Sink {
	return &stdoutSink{out: bufio.NewWriter(os.Stdout)}
}

func (s *stdoutSink) Write(batch api.Batch) error {
	for _, record := range batch.Records {
		if _, err := fmt.Fprintln(s.out, api.Encode(record)); err != nil {
			return err
		}
	}
	return nil
}

func (s *stdoutSink) Close() error { return s.out.Flush() }
''',
    "internal/sink/file.go": '''package sink

import (
	"bufio"
	"fmt"
	"os"

	"navsvc/pkg/api"
)

type fileSink struct {
	file *os.File
	out  *bufio.Writer
}

func newFileSink() Sink {
	f, err := os.CreateTemp("", "navsvc-*.out")
	if err != nil {
		return nullSink{}
	}
	return &fileSink{file: f, out: bufio.NewWriter(f)}
}

func (s *fileSink) Write(batch api.Batch) error {
	for _, record := range batch.Records {
		if _, err := fmt.Fprintln(s.out, api.Encode(record)); err != nil {
			return err
		}
	}
	return nil
}

func (s *fileSink) Close() error {
	if err := s.out.Flush(); err != nil {
		return err
	}
	return s.file.Close()
}
''',
    "internal/sink/null.go": '''package sink

import "navsvc/pkg/api"

// nullSink discards everything. Used by `output.sink = null` for dry runs.
type nullSink struct{}

func (nullSink) Write(api.Batch) error { return nil }
func (nullSink) Close() error          { return nil }
''',
}

# ---------------------------------------------------------------- report ----

REPORT = {
    "internal/report/summary.go": '''package report

import (
	"fmt"
	"sort"
	"strings"

	"navsvc/pkg/api"
)

// Summary accumulates per-name statistics across every batch that ran.
type Summary struct {
	counts map[string]int
	totals map[string]float64
}

// NewSummary returns an empty summary ready to observe batches.
func NewSummary() *Summary {
	return &Summary{
		counts: map[string]int{},
		totals: map[string]float64{},
	}
}

// Observe folds one batch into the running totals.
func (s *Summary) Observe(batch api.Batch) {
	for _, record := range batch.Records {
		s.counts[record.Name]++
		s.totals[record.Name] += record.Value
	}
}

// Render produces the human-readable report, one line per name, sorted.
func (s *Summary) Render() string {
	names := make([]string, 0, len(s.counts))
	for name := range s.counts {
		names = append(names, name)
	}
	sort.Strings(names)

	var b strings.Builder
	for _, name := range names {
		fmt.Fprintf(&b, "%s count=%d total=%g\\n", name, s.counts[name], s.totals[name])
	}
	return b.String()
}
''',
    "internal/report/aggregate.go": '''package report

import "navsvc/pkg/api"

// Aggregate is the entry point the command uses: it observes every batch and
// returns the rendered report.
func Aggregate(batches []api.Batch) string {
	summary := NewSummary()
	for _, batch := range batches {
		summary.Observe(batch)
	}
	return summary.Render()
}

// Distinct reports how many distinct names a set of batches contains. Used by
// the CLI's --names-only mode.
func Distinct(batches []api.Batch) int {
	seen := map[string]struct{}{}
	for _, batch := range batches {
		for _, record := range batch.Records {
			seen[record.Name] = struct{}{}
		}
	}
	return len(seen)
}
''',
}

# ------------------------------------------------------------- distractors --

DISTRACTORS = {
    "legacy/oldsummary.go": '''package legacy

// This package is the pre-1.0 implementation. It is kept for reference while
// downstream tooling migrates and is not wired into the running service: the
// command builds its report through internal/report.

import (
	"fmt"
	"sort"
	"strings"
)

// RecordSummary is the old flat summary record.
type RecordSummary struct {
	Name  string
	Count int
	Total float64
}

// SummarizeRecords produced the report in the pre-1.0 layout.
func SummarizeRecords(names []string, values []float64) string {
	counts := map[string]int{}
	totals := map[string]float64{}
	for i, name := range names {
		counts[name]++
		if i < len(values) {
			totals[name] += values[i]
		}
	}
	keys := make([]string, 0, len(counts))
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var b strings.Builder
	for _, k := range keys {
		fmt.Fprintf(&b, "%s count=%d total=%g\\n", k, counts[k], totals[k])
	}
	return b.String()
}
''',
    "legacy/oldpipeline.go": '''package legacy

import "strings"

// NormalizeName was the pre-1.0 normalization. The live pipeline stage lives
// in internal/pipeline and is selected through the stage registry.
func NormalizeName(name string) string {
	return strings.ToLower(strings.TrimSpace(name))
}

// DropInvalidRecords was the pre-1.0 filter.
func DropInvalidRecords(names []string, valid []bool) []string {
	kept := make([]string, 0, len(names))
	for i, name := range names {
		if i < len(valid) && valid[i] {
			kept = append(kept, name)
		}
	}
	return kept
}
''',
    "examples/example_summary.go": '''//go:build ignore

package main

// A standalone example showing how to summarize records without running the
// service. Not built as part of the module.

import "fmt"

func summarizeExample(names []string) {
	counts := map[string]int{}
	for _, n := range names {
		counts[n]++
	}
	for name, count := range counts {
		fmt.Printf("%s count=%d\\n", name, count)
	}
}
''',
    "internal/testutil/fakesink.go": '''package testutil

import "navsvc/pkg/api"

// FakeSink records everything written to it, for tests that assert on output
// without touching the filesystem.
type FakeSink struct {
	Batches []api.Batch
	Closed  bool
}

func (f *FakeSink) Write(batch api.Batch) error {
	f.Batches = append(f.Batches, batch)
	return nil
}

func (f *FakeSink) Close() error {
	f.Closed = true
	return nil
}
''',
}

# ------------------------------------------------------------------- cmd ----

CMD = {
    "cmd/navsvc/main.go": '''package main

import (
	"fmt"
	"os"

	"navsvc/internal/config"
	"navsvc/internal/ingest"
	"navsvc/internal/pipeline"
	"navsvc/internal/report"
	"navsvc/internal/sink"
	"navsvc/pkg/api"
)

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "navsvc:", err)
		os.Exit(1)
	}
}

func run(args []string) error {
	configPath := ""
	summarize := false
	for _, arg := range args {
		switch {
		case arg == "--summary":
			summarize = true
		case configPath == "":
			configPath = arg
		}
	}
	if configPath == "" {
		return fmt.Errorf("usage: navsvc <config> [--summary]")
	}

	cfg, err := config.Load(configPath)
	if err != nil {
		return err
	}
	if err := config.Validate(cfg); err != nil {
		return err
	}

	input := os.Stdin
	if cfg.Input.Path != "" && cfg.Input.Path != "-" {
		f, err := os.Open(cfg.Input.Path)
		if err != nil {
			return err
		}
		defer f.Close()
		input = f
	}

	out, ok := sink.For(cfg.Output.Sink)
	if !ok {
		return fmt.Errorf("unknown output.sink %q", cfg.Output.Sink)
	}
	defer out.Close()

	pipe := pipeline.New(cfg.Pipeline)
	reader := ingest.NewReader(cfg.Input, cfg.Input.Path)

	var collected []api.Batch
	err = reader.Read(input, func(batch api.Batch) error {
		batch = pipe.Apply(batch)
		if summarize {
			collected = append(collected, batch)
			return nil
		}
		return out.Write(batch)
	})
	if err != nil {
		return err
	}
	if summarize {
		fmt.Print(report.Aggregate(collected))
	}
	return nil
}
''',
}


def large_decoder() -> str:
    """internal/ingest/decoder.go — a genuinely large file with the target in
    its middle, surrounded by plausible neighbours rather than filler."""
    head = '''package ingest

import (
	"strconv"
	"strings"

	"navsvc/pkg/api"
)

// Decoder turns one input line into a record. Implementations register in
// registry.go; nothing switches on a format string outside that table.
type Decoder interface {
	DecodeLine(line []byte) (api.Record, bool)
}

'''
    parts = [head]

    # A run of small, plausible helpers before the target, so the top of the
    # file does not answer the question.
    for i in range(1, 34):
        parts.append(f'''// trimField{i} strips the delimiter variant used by exporter revision {i}.
func trimField{i}(field string) string {{
	field = strings.TrimSpace(field)
	field = strings.TrimPrefix(field, "{'|' if i % 2 else ';'}")
	return strings.TrimSuffix(field, "{'|' if i % 2 else ';'}")
}}

// parseValue{i} converts exporter revision {i}'s numeric column.
func parseValue{i}(field string) (float64, bool) {{
	value, err := strconv.ParseFloat(trimField{i}(field), 64)
	if err != nil {{
		return 0, false
	}}
	return value, true
}}

''')

    # The live jsonl decoder — the target region.
    parts.append('''// jsonlDecoder reads the service's primary format: one shallow JSON object
// per line. It is deliberately hand-rolled rather than using encoding/json so
// a malformed line costs a scan rather than an allocation and an error value.
type jsonlDecoder struct{}

func (jsonlDecoder) DecodeLine(line []byte) (api.Record, bool) {
	text := strings.TrimSpace(string(line))
	if text == "" || text[0] != '{' {
		return api.Record{}, false
	}
	record := api.Record{Labels: map[string]string{}, Valid: true}
	for _, field := range splitTopLevel(text[1 : len(text)-1]) {
		key, value, ok := strings.Cut(field, ":")
		if !ok {
			continue
		}
		key = strings.Trim(strings.TrimSpace(key), `"`)
		value = strings.Trim(strings.TrimSpace(value), `"`)
		switch key {
		case "name":
			record.Name = value
		case "value":
			parsed, err := strconv.ParseFloat(value, 64)
			if err != nil {
				record.Valid = false
				continue
			}
			record.Value = parsed
		case "depth":
			depth, err := strconv.Atoi(value)
			if err != nil {
				continue
			}
			record.Depth = depth
		default:
			record.Labels[key] = value
		}
	}
	if record.Name == "" {
		return api.Record{}, false
	}
	return record, true
}

// splitTopLevel splits a comma-separated object body without descending into
// quoted sections.
func splitTopLevel(body string) []string {
	var fields []string
	var current strings.Builder
	inQuotes := false
	for _, r := range body {
		switch {
		case r == '"':
			inQuotes = !inQuotes
			current.WriteRune(r)
		case r == ',' && !inQuotes:
			fields = append(fields, current.String())
			current.Reset()
		default:
			current.WriteRune(r)
		}
	}
	if current.Len() > 0 {
		fields = append(fields, current.String())
	}
	return fields
}

''')

    # More plausible neighbours after the target.
    for i in range(34, 61):
        parts.append(f'''// normalizeLabel{i} rewrites the label spelling used by collector {i}.
func normalizeLabel{i}(key, value string) (string, string) {{
	if strings.HasPrefix(key, "x_{i}_") {{
		key = strings.TrimPrefix(key, "x_{i}_")
	}}
	return key, strings.TrimSpace(value)
}}

// looksNumeric{i} reports whether collector {i}'s column holds a number.
func looksNumeric{i}(field string) bool {{
	_, err := strconv.ParseFloat(strings.TrimSpace(field), 64)
	return err == nil
}}

''')

    parts.append('''// csvDecoder reads the secondary format: name,value[,label=value...].
type csvDecoder struct{}

func (csvDecoder) DecodeLine(line []byte) (api.Record, bool) {
	text := strings.TrimSpace(string(line))
	if text == "" || strings.HasPrefix(text, "#") {
		return api.Record{}, false
	}
	columns := strings.Split(text, ",")
	if len(columns) < 2 {
		return api.Record{}, false
	}
	record := api.Record{
		Name:   strings.TrimSpace(columns[0]),
		Labels: map[string]string{},
		Valid:  true,
	}
	value, err := strconv.ParseFloat(strings.TrimSpace(columns[1]), 64)
	if err != nil {
		record.Valid = false
	} else {
		record.Value = value
	}
	for _, column := range columns[2:] {
		key, labelValue, ok := strings.Cut(column, "=")
		if !ok {
			continue
		}
		record.Labels[strings.TrimSpace(key)] = strings.TrimSpace(labelValue)
	}
	if record.Name == "" {
		return api.Record{}, false
	}
	return record, true
}
''')
    return "".join(parts)


TESTS = {
    "internal/pipeline/pipeline_test.go": '''package pipeline

import (
	"testing"

	"navsvc/internal/config"
	"navsvc/pkg/api"
)

func TestNormalizeLowercasesNames(t *testing.T) {
	cfg := config.PipelineConfig{Stages: []string{"normalize"}, MaxDepth: 4}
	batch := api.Batch{Records: []api.Record{{Name: "  Requests ", Valid: true}}}
	got := New(cfg).Apply(batch)
	if got.Records[0].Name != "requests" {
		t.Fatalf("want requests, got %q", got.Records[0].Name)
	}
}

func TestFilterDropsInvalidWhenConfigured(t *testing.T) {
	cfg := config.PipelineConfig{Stages: []string{"filter"}, DropInvalid: true, MaxDepth: 4}
	batch := api.Batch{Records: []api.Record{
		{Name: "a", Valid: true},
		{Name: "b", Valid: false},
	}}
	got := New(cfg).Apply(batch)
	if got.Len() != 1 {
		t.Fatalf("want 1 record kept, got %d", got.Len())
	}
}
''',
    "internal/report/summary_test.go": '''package report

import (
	"strings"
	"testing"

	"navsvc/pkg/api"
)

func TestSummaryCountsPerName(t *testing.T) {
	out := Aggregate([]api.Batch{{Records: []api.Record{
		{Name: "a", Value: 1},
		{Name: "a", Value: 2},
		{Name: "b", Value: 5},
	}}})
	if !strings.Contains(out, "a count=2 total=3") {
		t.Fatalf("unexpected report: %q", out)
	}
	if !strings.Contains(out, "b count=1 total=5") {
		t.Fatalf("unexpected report: %q", out)
	}
}
''',
    "internal/config/load_test.go": '''package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestLoadMergesOverDefaults(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "navsvc.conf")
	if err := os.WriteFile(path, []byte("input.format = csv\\npipeline.max_depth = 9\\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	cfg, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Input.Format != "csv" {
		t.Fatalf("want csv, got %q", cfg.Input.Format)
	}
	if cfg.Pipeline.MaxDepth != 9 {
		t.Fatalf("want 9, got %d", cfg.Pipeline.MaxDepth)
	}
	if cfg.Input.BatchSize != 128 {
		t.Fatalf("default batch size lost: %d", cfg.Input.BatchSize)
	}
}

func TestLoadRejectsUnknownKey(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "navsvc.conf")
	if err := os.WriteFile(path, []byte("output.colour = true\\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(path); err == nil {
		t.Fatal("expected an error for an unknown key")
	}
}
''',
}


def build(out: pathlib.Path) -> None:
    files: dict[str, str] = {"go.mod": go_mod()}
    for group in (CONFIG, API, INGEST, PIPELINE, SINK, REPORT, DISTRACTORS, CMD, TESTS):
        files.update(group)
    files["internal/ingest/decoder.go"] = large_decoder()
    files["README.md"] = (
        "# navsvc\n\n"
        "A small metrics ingestion service: read a stream, decode it, run the\n"
        "configured pipeline stages, then either write batches to a sink or\n"
        "print an aggregated summary.\n\n"
        "`legacy/` holds the pre-1.0 implementation, kept for reference while\n"
        "downstream tooling migrates. It is not wired into the service.\n"
    )

    for rel, body in sorted(files.items()):
        target = out / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body)

    source = [r for r in files if r.endswith(".go")]
    loc = sum(files[r].count("\n") + 1 for r in source)
    biggest = max(source, key=lambda r: files[r].count("\n"))
    print(f"{out}: {len(files)} files, {len(source)} Go files, {loc} LOC")
    print(f"  largest: {biggest} ({files[biggest].count(chr(10)) + 1} lines)")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="fixtures/repos/navsvc")
    ap.add_argument("--no-git", action="store_true")
    args = ap.parse_args()

    out = pathlib.Path(args.out).resolve()
    if out.exists():
        import shutil

        shutil.rmtree(out)
    build(out)

    if not args.no_git:
        env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "navsvc",
            "GIT_AUTHOR_EMAIL": "navsvc@example.com",
            "GIT_COMMITTER_NAME": "navsvc",
            "GIT_COMMITTER_EMAIL": "navsvc@example.com",
        }
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=out, check=True)
        subprocess.run(["git", "add", "-A"], cwd=out, check=True, env=env)
        subprocess.run(
            ["git", "commit", "-q", "-m", "navsvc: initial service"],
            cwd=out,
            check=True,
            env=env,
        )
        print("  git: initialized with one commit")


if __name__ == "__main__":
    main()
