# Real-project TUI matrix

Drives the **installed** `leveler tui` over a real PTY against real
repositories, with a real model, and reports what broke. Unit tests prove each
layer separately; this is the layer they cannot see — the assembled binary in a
real terminal.

## Running it

```sh
cargo install --path crates/leveler-cli --bin leveler --force
codesign --force -s - ~/.cargo/bin/leveler     # macOS: an overwritten binary
                                               # loses its ad-hoc signature and
                                               # is SIGKILLed on launch
bash fixtures/matrix/setup.sh                  # regenerate synthesized projects
python3 -m pip install pyte

python3 fixtures/matrix/tui_drive.py  --matrix fixtures/matrix/matrix.json \
        --out /tmp/pass1.json --log-dir /tmp/logs1
python3 fixtures/matrix/tui_stress.py --matrix fixtures/matrix/matrix.json \
        --out /tmp/pass2.json --log-dir /tmp/logs2
```

Both exit **non-zero** if anything was found, a project is missing, or a
project ran fewer rounds than expected. `RESULT: PASS` on the last line plus a
zero exit is the only green.

`matrix.json` holds paths **relative to the checkout**; the drivers resolve
them against the repo root, so a fresh clone or CI runs unchanged.

## What is and is not committed

Committed: the two drivers, `matrix.json`, `setup.sh`, this file.
Ignored: everything they produce — the generated projects, the cloned
`fixtures/repos/*` corpus, result JSON, and raw PTY logs.

Entries under `fixtures/repos/` are real upstream libraries and are **not**
provided here; supply them yourself (see `scripts/fetch_eval_repos.sh`) or run
a subset with `--only`.

## Two properties worth keeping

**The drivers must fail loudly.** An earlier version returned 0
unconditionally, so a run that found defects — or crashed in the driver — was
indistinguishable from a clean one. `report()` is the single place that decides
the exit code; keep it that way.

**A scenario that did not happen is not a pass.** The adversarial rounds assert
their own preconditions: a turn that never went busy did not test the
interrupt, and "no approval overlay AND the file survived" usually means the
model never attempted the deletion. Both are recorded as
`precondition-unmet` — an untested boundary, not a passing one.

## Three PTY traps

1. Set `TIOCSWINSZ`. A forked PTY defaults to 0×0 and `render()` returns before
   drawing a single character.
2. Assert on the **current frame**. PTY output is cumulative; concatenating it
   mixes in history.
3. Resize the **emulator** together with the PTY. `pyte`'s `display` calls
   `wcwidth(char[0])` and raises on the orphaned wide-character cells a CJK
   screen leaves behind after a resize — which reads as a product crash.

Turn completion keys off the product's own busy signal (the status line repaints
a spinner every 150ms), not off byte silence: a reasoning model can go quiet for
95 seconds mid-turn.
