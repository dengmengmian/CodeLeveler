# Agent Kernel

**Status: CURRENT.** Describes `crates/leveler-agent-core` and the seam between
it and `crates/leveler-agent` as they stand today. Chinese: none — this file is
the single source, referenced from both architecture guides.

## What it is

`leveler-agent-core` is the generic agent kernel: one authoritative loop over
the provider-neutral vocabulary in `leveler-model`.

```text
model request → model response → tool calls → tool results → next request
```

It is embeddable. Running it needs a `ModelRuntime` and a way to execute a tool
call — no repository, no workspace, no database, no `LEVELER_HOME`, no
configuration file. `cargo run -p leveler-agent-core --example minimal` proves
this with a deterministic in-process model and two toy tools.

There is exactly one such loop in the workspace. `leveler-agent` does not keep
a second one: it is a *harness* the kernel calls back into.

## What the kernel owns

- The round loop and the round counter.
- The model round: streaming, assembly of the assistant message, retry with
  backoff on retryable provider failures, and the terminal-event rule (a stream
  that ends without one is never success).
- Admission of the next round against mechanical limits — round ceiling, pinned
  window limit, model tokens, cost, wall clock — plus cancellation and the
  deadline timer.
- Usage accounting: one fold of what each call reported, priced once, with a
  transcript estimate standing in for a provider that reports nothing.
- Neutral stop reasons and neutral events.
- One tool-dispatch seam.

## What the kernel does not own

Anything that answers a question about *this product* or *this repository*:

- permission, approval, write scope, sandboxing, network policy
- workspaces, files, commands, checkpoints
- prompts, repository rules, memory, skills
- persistence, event logs, resume, ownership fencing
- delegation, sub-agents, settlement
- whether a task is complete, verified, or acceptable

It also owns no semantic reading of a run. Its stop reasons are `ModelEnd`,
`Cancelled`, `RoundCeiling`, `WindowLimit`, `BudgetExhausted` — never
`Completed`, `Verified`, or `Accepted`. The harness maps them onto CodeLeveler's
`StopReason` and `AgentOutcome`, which is where a product word like `Completed`
is allowed to appear, and only because the model explicitly said so through
`update_goal`.

A test enforces the first half of this list mechanically:
`crates/leveler-agent-core/tests/kernel_boundary.rs` fails the build if the
kernel gains a workspace dependency or names a host security concept.

## Dependency direction

```text
leveler-model            provider-neutral request / response / message / tool call
      ▲
leveler-agent-core       the loop
      ▲
leveler-agent            the CodeLeveler coding harness
      ▲
leveler-engine           durable task / turn / session runtime
      ▲
leveler-app              product composition
```

The kernel's only workspace dependencies are `leveler-model` (the vocabulary it
is written against) and, for its example and tests only, `leveler-core` (ids).
This is an *ownership* graph: real edges also run sideways into the harness and
the engine, and nothing is wrapped merely to make the picture a straight line.

## The tool boundary

The kernel defines the smallest thing it can:

```rust
trait ToolRuntime {
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn execute(
        &self,
        call: ToolCall,
        cancellation: CancellationToken,
    ) -> Result<ToolOutcome, ToolRuntimeError>;
}
```

`ToolOutcome` is text plus an `is_error` flag. A refused, unknown, or failed
tool is a *result* the model reads; `Err` is reserved for the runtime failing
to produce any result at all, which aborts the run.

`leveler-tools::Tool` is deliberately **not** this trait, and was not moved.
A CodeLeveler built-in receives a rich `ToolContext` — workspace, command
runner, checkpoints, resolved execution policy, write scope, LSP, artifact
store, browser, session scope — and pulling that into the kernel would drag the
whole product in behind it. The two contracts coexist:

```text
leveler-agent-core::ToolRuntime          generic: a call in, text out
        │
        │  implemented by the harness
        ▼
leveler-agent  ToolHost admission        hooks → permission rules → profile
        │                                policy → approval → barriers → fence
        ▼
ResolvedExecutionPolicy (frozen)
        ▼
leveler-tools built-ins  ──▶  leveler-execution
```

The harness implements the generic boundary in terms of its own pipeline. It
adds no second authorization: the `ToolRuntime` seam is a dispatch contract,
not a policy one.

## Security boundary: one authorization point

`Executor::admit` in `crates/leveler-agent/src/executor/host.rs` is the only
place a model-proposed call becomes an execution. It runs the side-effect
barrier, pre-hooks, permission rules, profile policy, auto-review/approval, the
barrier again, and the ownership fence, and returns an `AdmittedCall` whose
only constructor is that pipeline. `dispatch` accepts nothing else, so
execution without admission does not typecheck.

Two tests hold the line: `crates/leveler-agent/tests/tool_host_boundary.rs`
fails if any other file in the agent crate — or in `leveler-engine` or
`leveler-app` — reaches `registry.execute` or the hook gate directly.

Extracting the kernel changed none of this:

- `ResolvedExecutionPolicy` is still frozen per admitted call. A profile switch
  or a grant made after admission reaches the *next* call, not this one.
- `WriteScope` (`None` / `Workspace { root }` / `Unrestricted`) is still the one
  write boundary.
- Reads still need no authorization, and are still subject to the credential
  denylist and the secret scrub on every tool result.
- `network` is still a capability of its own; a write grant never implies it.

## The harness seams

`leveler-agent`'s `Drive` implements `AgentHarness`. The kernel calls it at
fixed points of every round:

| Seam | When | What the harness does with it |
| --- | --- | --- |
| `on_round_start` | before admission | mid-turn steering, settle finished background children |
| `on_round_admitted` | round counter advanced | scoped `AGENTS.md` rules, plan nudge, budget note |
| `tool_definitions` | building the request | the CodeLeveler tool table plus injected tools |
| `on_model_error` | model round failed after kernel retries | bounded decode-retry feedback |
| `on_response` | model answered, spend already folded | persist the model-request row, truncation/filter handling, length continuation |
| `on_quiet` | response carried no tool calls | closeout decision, goal-mode stall, terminal outcome |
| `execute_calls` | response carried tool calls | the whole tool batch: gates, admission, dispatch, parallel batch, delegation, ledgers, compaction |
| `on_stop` | kernel stopped | map the neutral reason onto `AgentOutcome` |

Each seam returns a `Flow`: continue, start the next round, or stop with the
harness's own outcome. The harness never iterates; the kernel never decides.

## Compaction, budgets, usage: where each lives

Judged per mechanism, not as a block.

| Mechanism | Owner | Why |
| --- | --- | --- |
| Round / token / cost / duration limits | kernel | pure functions of messages, spend, and the clock |
| Cancellation and the deadline timer | kernel | generic, and the loop must observe them |
| Usage folding and pricing | kernel | one number the budget guard and the ledger share |
| Token estimate fallback | kernel | needed to keep a token budget binding on a zero-usage gateway |
| Retry and backoff | kernel | a property of talking to a model |
| *When* to fold the transcript | harness | reads the product's context budget and its checkpoint port |
| *How* to fold it | harness | anchors on the repository objective and scoped project rules |
| Commands / modified-files budgets | harness | they count workspace side effects, which the kernel cannot see |

## Embedding

```rust
let agent = Agent::new(runtime, ModelRef::new("provider", "model"))
    .with_limits(RoundLimits { window_round_limit: Some(8), ..Default::default() });
let mut harness = BasicHarness::new(my_tools);
let stop = agent.run(messages, &mut harness, CancellationToken::new()).await?;
```

`BasicHarness` is the whole harness for a plain tool-calling agent: every call
goes to one `ToolRuntime`, and the run ends where the model stops. Implement
`AgentHarness` directly when the embedding needs more, as `leveler-agent` does.

Multi-agent orchestration is deliberately absent. The kernel can be a parent
loop or a child loop, but *how a child is created, scoped, and settled* is the
harness's question, and a future multi-agent design composes kernel instances
rather than teaching the kernel about them.
