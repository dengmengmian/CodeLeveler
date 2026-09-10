# PTY real-TUI final acceptance — evidence

The report is `docs/evaluations/PTY_REAL_TUI_FINAL_ACCEPTANCE.md`. This is what
it rests on. Nothing here was typed by hand: `pty_assemble.py` writes the tree
and the coverage table, `pty_gate.py` writes the verdicts.

| Path | What |
|---|---|
| `environment.json` | host, toolchain, the frozen commit and both binary hashes, and how each run was isolated |
| `manifest.json` | coverage, agent-runtime attribution per session, terminal-process CPU and RSS |
| `tables.md` | the same numbers rendered, as they appear in the report |
| `pty/<stage>/<run>@<lab>/` | per-run metadata, metrics (including mid-turn samples), and the screens a claim rests on |
| `replay/corpus.json` | one row per replayed session |
| `replay/invariants.json` | the corpus totals the release gates read |
| `stress/replay-soak.json` | replay determinism and the memory soak |
| `stress/process-soak.json` | launch/exit lifecycle and the resize soak |
| `findings/findings.json` | every finding and observation, with root causes |
| `findings/F2-before-fix.txt` | the eight frames F2 was found in, before the fix |
| `findings/gates.json` | the release gates and the evidence each one read |

`@<lab>` in a run's directory name says which binary drove it:
`pty-acceptance-494ed1b` is the frozen one, `pty-rerun` is the same binary with
corrected driver assertions, `pty-fix` is the one carrying this round's fixes.

Raw PTY streams and session stores stay out of the repository — they are large
and full of one machine's paths. The lab keeps them; what is here is the
metadata, the derived results, and the screens worth reading.
