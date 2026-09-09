# Round-Trip Closure

Measured 2026-09-09 against `e695b82`. Follows
`docs/RUNTIME_LATENCY_CLOSURE.md`, which located the root causes but
implemented no lever.

## Verdict

| | |
| --- | --- |
| P0-B closeout nudge | **NO_EFFECT** — the reword did not move the rate |
| P0-C first-round context | **PASS** — mechanism confirmed; effect since revised to 5–11% fewer rounds, wall not reproducible |
| P0-D parallel capability | **NO CHANGE** — the profile is honest |
| Overall | **PARTIAL** |

One of three levers paid. The other two are recorded as negative results
rather than shipped on the strength of a plausible story.

## 1. Why round reduction, not token reduction

The previous stage measured 84 real model requests and found input is a
prefix-cache hit 92.3% of the time, while doubling a request costs only 16%
more time-to-first-byte. Wall clock is `round trips × ~3.8 s`, and 91.3% of
wall is model wait.

So the unit of cost is the round trip, not the token. A change that saves
4,000 cached tokens but risks one extra round trip is a loss. A change that
adds 2,000 stable prefix tokens and removes one round trip is a win. Every
decision below is sized that way.

## 2. Round baseline (A0)

Seven real cases, `deepseek/deepseek-v4-flash`, `reasoning_effort = max`,
release build, Assisted, one run each. Classified by
`evals/scripts/round_taxonomy.py`, which reads the tracing the runtime already
emits and adds no events.

| | |
| --- | --- |
| Cases | 7, all passing |
| Model rounds | 80 |
| Median rounds / case | 7 |
| Tool rounds | 75 |
| Closeout nudge rounds | 5 (6.2%), on 5 of 7 cases |
| Tool calls per round | 1 on 75 rounds, 0 on 5. **Never 2.** |
| Useful tool calls / tool round | 1.00 |
| First tool of the case | `list_files` on 5 of 7 |
| Model wait | 289.9 s (TTFB 220.4 s) |

Two targets fall out of this, and one non-target: rounds spent on closeout,
rounds spent discovering what files exist, and the flat one-call-per-round
ceiling.

## 3. P0-B — closeout nudge

**Root cause found.** `prompts/base.md` said `update_goal` was "silent
bookkeeping only", while the tool's own description says "Going silent does NOT
end the task — you must call this." The prompt contradicted the tool. A model
that reads the first writes its answer and stops, and the runtime spends a
whole round trip asking it to close the goal.

**Change.** One clause in `base.md`: `update_goal` is how a goal ends, called
in the same turn as the final answer; final prose does not close a goal. The
"never narrate process state to the user" rule is unchanged, and nothing about
what `complete` *means* was touched — that lives in the tool description and is
the completion contract.

**Measured.** Ten repetitions of one small case, before and after:

| | A0 | A1 |
| --- | --- | --- |
| Runs | 10 | 10 |
| Total rounds | 53 | 54 |
| Runs firing a nudge | 8 | 6 |
| Nudge rate (of rounds) | 15.1% | 11.1% |
| Model wait | 160.4 s | 156.7 s |

8/10 → 6/10 at n = 10 is well inside binomial noise. On the seven-case suite,
nudges went 5 → 3 on the deterministic six while total rounds stayed at 39.

**Verdict: NO_EFFECT.** The wording change is kept because it removes a real
contradiction between two parts of the prompt, and it demonstrably costs
nothing — but it is not a latency improvement and is not counted as one.

A structural explanation was tested and refuted: across 247 observed rounds, 21
carried both prose and a tool call, so the model *can* close in the finishing
turn. It usually does not, and prompt wording did not change that.

## 4. P0-C — first-round context

**Audit.** `TurnContext` — everything the system prompt says about the
workspace — carries model, permission mode, network, cwd, project rules and
user language. **No file listing, no candidate files, no project summary.**

`leveler-context` already contains `RepositoryMap`, `select_candidates` and
`TaskContext`, bounded and tested. Nothing outside that crate calls them: the
agent uses only `load_rules`. The machinery existed and was unreachable from
the request path.

That is why `list_files` was the first tool on 5 of 7 cases. The first thing
the agent did was spend a round trip asking what files exist.

**Change.** `TurnContext` gains `repo_map`, filled from the existing
`RepositoryMap` and rendered into the system prompt. Bounded twice: the map's
own 400-file cap, plus an 8 KB byte cap that truncates on a line boundary and
says it truncated. No second repo map was built.

This is prefix-cache safe by construction. `system_prompt()` is built once
before the loop and is documented as byte-identical for its whole duration, so
the listing is paid for once, uncached, on the first request.

**Measured** (deterministic six; the navigation case is excluded from the
comparison because it swings 41/21/24 rounds run to run):

| | A0 | A1 | A2 |
| --- | --- | --- | --- |
| Total rounds | 39 | 39 | **32** |
| Median rounds / case | 6.5 | 6.0 | **5.5** |
| Total wall | 144.2 s | 135.1 s | **123.0 s** |
| Median wall | 23.4 s | 20.4 s | 21.1 s |
| `list_files` as first tool | 4 / 6 | 4 / 6 | **0 / 6** |
| Cases passed | 6 / 6 | 6 / 6 | 6 / 6 |

Per case, A2 is at or below A0 on rounds everywhere:

| case | A0 | A1 | A2 |
| --- | --- | --- | --- |
| go-json-defaults | 8 | 7 | 6 |
| go-triple | 7 | 6 | 6 |
| rust-first-even | 6 | 6 | 5 |
| rust-mul | 4 | 6 | 4 |
| rust-utf8-truncate | 6 | 6 | 5 |
| ts-deep-merge | 8 | 8 | 6 |

**Verdict: PASS**, with the size of the effect since revised down.

> **Superseded by repeated measurement.** These figures are one run per case.
> A 24-run interleaved A/B (`docs/evaluations/REPOSITORY_MAP_AB_VALIDATION.md`)
> reproduces the mechanism completely — `list_files`-first went 12/12 to 0/12 —
> but puts the round saving at 5–11%, not 17.9%, and finds no reproducible wall
> improvement. Quote that document, not the −17.9% / −14.7% here.

## 5. P0-D — parallel tool-call capability

The profile declares `parallel_tool_calls = false` and
`max_parallel_tool_calls = 1`. `openai_chat/mod.rs:112` turns that into an
explicit `parallel_tool_calls: false` on the wire — so observing one call per
round proves nothing on its own. The runtime asked for it.

**Probe.** An isolated `LEVELER_HOME` with the same model reconfigured to
`parallel_tool_calls = true`, `max_parallel_tool_calls = 4`, run over a
read-heavy navigation case plus six repetitions of a small case:

| | |
| --- | --- |
| Rounds observed | 57 |
| Tool calls per round | 1 on 53, 0 on 4 |
| Multi-tool responses | **0 / 57** |

With permission explicitly granted, the model still never returned more than
one tool call.

**Verdict: NO CHANGE.** `max_parallel_tool_calls = 1` is an honest capability
declaration for this model through this gateway. Whether the limit is the model
or the gateway is not distinguishable from here, and does not matter: the
config is not lying, so there is nothing to fix. No prompt rule was added to
push batching — that was A/B-rejected twice in earlier work
(`docs/design/INVESTIGATION_EFFICIENCY_DIAGNOSIS.md`).

## 6. Correctness

| gate | result |
| --- | --- |
| Task success | 7/7 in every arm |
| Completion accuracy | 100% in every arm |
| False verified | 0 |
| Incorrect + verified | 0 |
| Completion contract | untouched — `update_goal`'s meaning of `complete` is unchanged |
| Closeout nudge | retained as the fallback; it still fires |
| Verification / evidence / grounded authority | untouched |
| Permissions, ownership, sub-agents, resume | untouched |

Nothing here weakens a gate to save a round. The closeout change is a wording
fix in the prompt; the repo map is read-only context.

## 7. Remaining round-trip sources

1. **The fixed ~2.5 s before the response stream opens**, 76–80% of every round
   trip. Not the runtime's to shorten.
2. **One tool call per round**, confirmed as a capability limit. On this model
   every observation is a separate round trip.
3. **The closeout nudge**, still firing on roughly half of small tasks. The
   cause is model behaviour, not a missing instruction — prompt wording did not
   move it.
4. **Navigation variance.** The one navigation case ran 41, 21 and 24 rounds
   across three arms of identical configuration. On read-heavy work the round
   count is dominated by how quickly the model finds the right file, which
   swamps every runtime lever measured here.

## 8. Composite observation decision

**DEFERRED.** The entry conditions were not met: the closeout nudge is not
closed (P0-B was NO_EFFECT), so the cheapest remaining round is still on the
table, and the first-round fix has only just removed the most common
exploration round. Building a bounded composite read tool now would be sized
against a baseline that just moved.

The one condition that *is* met is #3 — parallelism is genuinely unavailable on
this model — which is what would make a composite read valuable if read-heavy
tasks remain round-bound after the above settles. Revisit with a read-heavy
suite large enough to see past navigation variance.

## 9. Method notes and limits

- One run per case on the seven-case suite; ten repetitions only for the
  closeout probe. Round counts are stable enough to compare, wall times less
  so.
- The navigation case is excluded from the headline comparison and reported
  separately, because its variance (41/21/24) exceeds every effect measured.
- `evals/scripts/round_taxonomy.py` classifies rounds from existing tracing.
  It infers a closeout round from the `closeout decided` event, not from
  "this round had no tool call".
