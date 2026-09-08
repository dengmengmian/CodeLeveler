# Does the prompt teach the agent to investigate one step at a time?

Read-only audit. No prompt changed, no code changed, no model run.

**Verdict: no. The prompt contains no instruction to call one tool per turn,
and the serialization that does occur is too small to be the cost driver.**

```
PROMPT_SERIALIZATION_VERDICT   = NOT_ESTABLISHED
PROMPT_AS_PRIMARY_FAST_LEVER   = REJECTED
CANDIDATE_PROMPT_LEVER         = NONE
```

## The model-visible stack

| component | source | injected | persistent |
|---|---|---|---|
| base prompt | `prompts/base.md` (20 KB) | every request | yes |
| turn context / operating rules | `prompt.rs::operating_rules` | every request | yes |
| tool schemas | tool registry | every request | yes |
| goal | task spec | first user message | yes |
| plan | `update_plan` results | on change | yes |
| project rules (AGENTS.md) | scoped loader | on first touch of a tree | yes |
| runtime nudges | 22 injection sites in `drive.rs` | conditional | some ephemeral |

```
SYSTEM_PROMPT_SIZE        31,948 chars  ≈  7,987 tok
TOOL_SCHEMAS + scaffold                 ≈ 10,457 tok
TOTAL_FIXED_PROMPT                      ≈ 18,444 tok
```

## What the fixed prompt costs

That block is re-sent verbatim on every round.

| round | input tokens | fixed-prompt share |
|---|---:|---:|
| R1 | 18,837 | 97.9% |
| R25 | 44,828 | 41.1% |
| R50 | 65,385 | 28.2% |
| R100 | 106,037 | 17.4% |
| R190 | 108,697 | 17.0% |

Across post-closure C3: 190 × 18,444 ≈ **3.5M tokens, 24.6% of the run's
14.26M input**. A quarter of the bill is the same instructions and schemas,
sent 190 times.

That is a re-send cost, not a serialization cost. It belongs to the next
package, and it is the largest single number this audit produced.

## Serializing language, classified

Nothing in the prompt says "one tool per turn", "wait for the result", or
"decide the next step after each observation". What is there:

| instruction | class |
|---|---|
| "Long-lived processes … then a **separate** tool call for health checks" | CORRECTNESS_REQUIRED — you cannot curl a server you have not started |
| "Read before you edit" | CORRECTNESS_REQUIRED — a logical dependency, silent on turns |
| "Keep exactly ONE step in_progress … never batch-complete several steps; mark each one as you actually finish it" | LIKELY_SERIALIZING, but scoped to `update_plan` |
| "any conclusion about code must rest on code you actually read THIS turn … re-read before asserting" | F7_REQUIRED |
| "Prefer `shell_command` … for git and ad-hoc shell work" | routes git inspection to shell — real, but shell is already closed |

Only one is plausibly unnecessary serialization, and its blast radius is one
tool: `update_plan` fired **4 times in 190 rounds** in post-closure C3. Roughly
2% of turns.

```
PLAN_PROMPT_SERIALIZATION            = PARTIAL, ~2% of turns
TOOL_DESCRIPTION_SERIALIZATION       = NO
TOOL_RESULT_FOLLOWUP_SERIALIZATION   = NO   (no "based on this result, decide…" contract)
COMPLETION_PROMPT_CAUSES_MICRO_CLOSEOUT = NOT_SUPPORTED
```

## The behavioural measurement

The decisive test is not what the prompt says but whether the model serialises
work it has already decided on. Sampled across six long runs: assistant
messages that name several concrete next inspections, with no stated dependency
between them.

```
KNOWN_MULTI_ACTION_INTENT_CASES     22
  no stated dependency              21
  SAME_TURN_MULTI_TOOL_EXECUTION     6
  SERIALIZED_DESPITE_KNOWN_ACTIONS  15   (71%)
```

Examples of the serialized ones — the model says it will do two things and does
one:

> "Let me inspect the dev-tunnels Client for concurrency behavior, **and** check
> how `CodespaceConnection`/forwarder are copied" → one `shell_command`

> "Now let me read `ports_test.go` **and** check where these APIs are used
> before designing the fix." → one `read_file`

So the behaviour is real: **when the model announces two independent
inspections, it emits one of them 71% of the time.**

But 22 such cases across six runs carrying roughly 900 tool-using turns is
about 2.4% of turns. Fixing every one saves ~15 round trips out of ~900.

## Why this does not become a lever

The fix for exactly this behaviour has already been built and measured. The L1
batching rule told the model, in the prompt, to issue independent inspections
together. Under a contemporaneous latency-matched A/B it moved multi-tool turns
by about five points inside a control spread of twenty-four, and requests, cost
and wall all moved the wrong way at the median.

Re-proposing it in different words is not a new lever, and this audit found no
*different* serialization mechanism with source evidence behind it. So:

```
CANDIDATE_PROMPT_LEVER = NONE
```

## Comparison to other harnesses

The AtomCode and DeepSeek-harness prompts are not present as source in this
tree — only build artifacts.

```
PROMPT_SERIALIZATION_RELATIVE_TO_PI  = UNKNOWN
PROMPT_SERIALIZATION_RELATIVE_TO_DSH = UNKNOWN
```

Not claimed either way.

## Where this leaves FAST

Five levers measured, none survived:

```
quiet-round early closeout        REFUTED    truncates correct runs
delegation reconsideration        WEAKENED   adoption 0/2 after reoffer
independent-inspection batching   REJECTED   effect < control variance
shell investigation ergonomics    REJECTED   87% of it is composition
prompt serialization              REJECTED   ~2% of turns, and its fix already failed
```

Every one of them tried to change what the model does. What is left is
arithmetic that does not depend on the model's choices at all:

```
fixed prompt + schemas   18,444 tok × every round   = 24.6% of C3's input
tool results             445 KB carried ~90 times   = most of the remainder
MODEL_CONTEXT_MODE       FULL_ACCUMULATED_WITH_TRIMMING
COMPACTION_COUNT         0 in every run measured
```

```
NEXT = TOOL_RESULT_CONTEXT_GROWTH_CLOSURE
```
