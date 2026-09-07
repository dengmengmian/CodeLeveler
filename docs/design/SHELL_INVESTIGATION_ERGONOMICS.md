# Why read-only investigation goes through the shell

1,224 main-agent shell calls across 14 runs — the seven historical successes
plus the seven controls from the L1 confirmation cohort — classified by what
their arguments do rather than by which binary they name.

**Verdict: the shell is mostly doing something no structured tool offers, and
the part that could be replaced is small. This is not the FAST lever.**

```
SHELL_ERGONOMICS_AS_PRIMARY_LEVER = REJECTED
SHELL_ERGONOMICS_PRODUCT_CHANGE   = NOT_MADE
```

## The corpus

```
TOTAL_SHELL_CALLS        1224
INSPECTION_SHELL_CALLS    920   75.2%
```

| category | calls | share |
|---|---:|---:|
| TEXT_SEARCH | 421 | 34.4% |
| SOURCE_INSPECTION | 205 | 16.7% |
| GIT_INSPECTION | 190 | 15.5% |
| TEST | 161 | 13.2% |
| MUTATING | 73 | 6.0% |
| DIRECTORY_INSPECTION | 60 | 4.9% |
| FILE_DISCOVERY | 44 | 3.6% |
| BUILD | 31 | 2.5% |
| other | 39 | 3.2% |

A first pass filed 22% of the corpus as MUTATING because the pattern for a
redirect matched `2>/dev/null`. Those are read-only commands suppressing
stderr. Corrected, inspection rises from 60.8% to 75.2% — worth recording
because it is the kind of error that would have pointed the whole package the
wrong way.

## The finding: 87% of inspection is composition

| category | calls | composed | single |
|---|---:|---:|---:|
| TEXT_SEARCH | 421 | **413 (98%)** | 8 |
| SOURCE_INSPECTION | 205 | 114 (56%) | 91 |
| GIT_INSPECTION | 190 | 176 (93%) | 14 |
| DIRECTORY_INSPECTION | 60 | 54 (90%) | 6 |
| FILE_DISCOVERY | 44 | 42 (95%) | 2 |
| **all inspection** | **920** | **799 (87%)** | **121 (13%)** |

"Composed" means a pipe, a `;`/`&&` chain, or command substitution. Of 421 text
searches, **eight** are a single unpiped command. The rest look like:

```
grep -rn "X" pkg/ | head -20; echo ---; grep -c "Y" go.mod
find "$(go env GOMODCACHE)" -maxdepth 3 -type d -name 'dev-tunnels*' 2>/dev/null
```

Flag usage in text search: piped 83%, chained 81%, context lines (`-C`/`-A`/`-B`)
~50%, glob/include/exclude 9%, case-insensitive 2%.

The model is not reaching for the shell because `grep` lacks a flag. It is
reaching for the shell because it is **composing** — search, then filter, then
count, then a second search, in one round trip. Composition is what a shell is,
and no bounded structured search replaces it.

```
SHELL_INTRINSIC_RATIO = 87% of inspection
```

## The 13% that could be replaced, and why it is not

Of 121 single uncomposed inspection calls:

**91 SOURCE_INSPECTION** — 80 are `sed -n 'A,Bp' file`, which is *exactly*
`read_file(path, start_line, end_line)`. The tool already does this.

But **69 of the 91 point at absolute paths outside the repository**, almost all
of them Go module cache:

```
sed -n '467,570p' /Users/…/go/pkg/mod/github.com/microsoft/dev-tunnels@v0.1.27/go/tunnels/manager.go
```

`Workspace::resolve_read` refuses anything outside the workspace root and the
configured `--readonly-root`s, and the eval configures none. So `read_file` on
dependency source is **denied** and `sed` on the same file **succeeds**. The
model is not choosing shell over the tool; the tool cannot reach the file.

**14 GIT_INSPECTION** — `git show`, `git log`, `git ls-files`. Only `git_status`
and `git_diff` exist as tools; the rest have no structured equivalent at all.

**8 remaining** — `ls`/`find` on out-of-repo absolute paths, same scope problem.

```
DIRECTLY_REPLACEABLE_INSPECTION_RATIO    ~2%   (in-repo line-range reads)
SMALL_CAPABILITY_GAP_RATIO               ~8%   (out-of-repo reads: scope, not features)
SUBSTANTIAL_GAP_RATIO                    ~2%   (git show/log/ls-files)
SHELL_INTRINSIC_RATIO                    ~87%
```

Even a perfect fix for every one of those addresses ~13% of inspection calls,
which are themselves a fraction of a run's round trips. That is not where the
cost is.

## A boundary asymmetry worth reporting on its own

Independent of FAST, the audit surfaced this:

- `read_file("/Users/…/go/pkg/mod/…/manager.go")` → **refused**, outside the
  workspace and any readonly-root.
- `shell_command("sed -n '467,570p' /Users/…/go/pkg/mod/…/manager.go")` →
  **allowed**. `shell_guard` refuses credential-bearing paths only, and its own
  comment says it is "a guard rail for the honest-model path, not a security
  boundary".

So the structured read path enforces a workspace boundary that the shell path
does not. Whether that is intended — the OS sandbox and `env_clear` are
described as the hard line — is a product question, not one this package should
answer. It is recorded because an audit found it, and because it is the
mechanical reason the model routes dependency reading through `sed`.

## Where this leaves FAST

Four levers measured, none survived:

```
quiet-round early closeout       REFUTED    truncates correct runs
delegation reconsideration       WEAKENED   adoption 0/2 after reoffer
independent-inspection batching  REJECTED   effect < control variance
shell investigation ergonomics   REJECTED   87% of it is composition
```

The first three tried to change what the model chooses. This one tried to
change what it can reach — and found that what it reaches for is mostly
irreplaceable.

What has never been touched is the secondary amplifier from the original
diagnosis: **the same tool results are re-sent to the model on every subsequent
round trip inside one ever-growing transcript.** formal C3 carried 445 KB of
tool results and spent 10.2M input tokens carrying them, about ninety times
over. `MODEL_CONTEXT_MODE = FULL_ACCUMULATED_WITH_TRIMMING`,
`COMPACTION_COUNT = 0` across every run measured.

That is the one remaining multiplier, and it is mechanical rather than
persuasive.

```
NEXT = TOOL_RESULT_CONTEXT_GROWTH_CLOSURE
```

## Reproducing

`shell_corpus.py <run_dir>… [--dump N]` in `dogfood/eval/phase-c/` classifies
every main-agent shell call from durable transcripts alone.
