# Batch #2 Targeted Repair Gate — closeout

**Candidate `739111a` on `fix/batch-02-targeted-repair` (base `04347d2`). READY_FOR_MERGE = YES.**
**OPEN_BETA_BLOCKER = 0 · OPEN_BETA_REQUIRED = 0.**

Four commits, in dependency order:

| Commit | What |
| --- | --- |
| `983ca76` | engine: window progress counts refinement — mutation ops (re-edits included) and fresh-green verification join set growth; spin still terminates |
| `b2cf1e5` | agent/engine: `closure_review_stage` — the required reviewer runs at every mutated closure exit (failed paths included) with durable `review_stage` eligibility/launch/terminal events; launch failures persist their cause |
| `d09ada2` | tests: clippy tidy |
| `739111a` | agent: reviewer bounded at 20 rounds; non-clean child stops keep what the child said instead of the synthetic stop sentence |

## Production evidence (the gate's own reruns, original frozen goals)

- **R011r**: window 1 ran 2.1 h with three engine turns — the old predicate died at turn 1 (that
  was R011). Terminal `failed` came from real verification gates; the required review launched on
  that failed path ("security-sensitive path, 39 modified file(s)"). Functional FAIL is the
  model's (it faked empty Go packages to appease the build) and was labelled honestly.
- **R013r / R013r-2**: functional 7/7 both times. The reviewer launched both times; the first
  burned the unbounded 100-round ceiling (the finding that produced `739111a`), the second
  stopped at the 20-round bound, 5× cheaper, fully explained by two `review_stage` rows.
- **Reviewer adoption 3/3 qualifying reruns + 5/5 Verified-path smoke** (Batch #2: 0/4), with
  zero model spawn calls throughout.
- **Bonus**: N1's missing production evidence arrived on its own — three abnormal reviewer
  terminals, partial results intact, statuses honest.

## Revalidation on the candidate

Workspace 2817/0 · clippy `-D warnings` · fmt · P0 byte-identical · P1/P3/P4 PASS · Recovery
Truth 4/4 · toolchain-env 3/3 · F6 12/12 · reviewer smoke 5/5 (`verified` outcome) · provenance
manifest with self-reporting binary.

## Honest boundary

- R012-F1 (durable-args truncation) stays OPEN_EVIDENCE_NEEDED — out of scope, not smuggled.
- New R013r-M1 (MODEL/LOW/POST_BETA): flash's reviewer reads wide diffs without concluding inside
  its bound; the one-line brief fix belongs to the next train — applying it after the reruns
  would have invalidated this gate's evidence for a prompt tweak.
- Cross-client resume of an unfinished goal and goal-owned process survival remain NOT_OBSERVED —
  driver limitation, unchanged from Batch #2.
