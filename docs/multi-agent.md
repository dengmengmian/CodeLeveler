# Multi-agent delegation

CodeLeveler can run several focused **sub-agents** in parallel for independent
investigation or disjoint edits. The parent agent keeps the conversation and
synthesizes child results. Sub-agents do not talk to each other (star topology).

## When it runs

The model calls the injected `spawn_agent` tool. Emitting **several**
`spawn_agent` calls in **one** assistant turn runs them **concurrently**.

The product steers the model toward this when:

- You ask for parallel / multi-agent work (e.g. “并行 review”, “split the work”).
- The task has independent facets (e.g. architecture + stability + tools review).
- A one-shot host hint is injected for matching request text (`## Multi-agent delegation`).

It does **not** auto-spawn without a model `spawn_agent` call. Trivial one-step
tasks should stay on the parent.

## Roles

| `role` | Behavior |
| --- | --- |
| `explorer` | Read-only toolset (cannot edit the workspace). |
| `worker` | May write; must pass exclusive `files` it owns. |
| `default` | Full tools (when unspecified). |

Assign **disjoint** `files` to parallel workers so they never edit the same path.

## Hard limits

| Limit | Default |
| --- | --- |
| Nesting depth | **1** (sub-agents cannot spawn further sub-agents) |
| Concurrent sub-agents | **4** |
| Total spawns per top-level run | **6** |
| Max duration per sub-agent | **15 minutes** (also bounded by parent residual) |

## How to force

Phrase the request so the model is steered, for example:

- “并行开三个 explorer：架构 / 稳定性 / 工具，查完汇总。”
- “Use multi-agent: spawn explorers for architecture and security in parallel.”

## How to disable

**Project** (`.leveler/config.yaml`):

```yaml
agents:
  delegation: false
```

**Global** (`~/.leveler/config.toml`):

```toml
[agents]
delegation = false
```

When either level sets `delegation = false` (project ANDs with global: both
must allow), `spawn_agent` is **not** advertised in the tool list.

## What you see in the TUI

Concurrent children appear as a **sub-agent tree**. While running, each child
shows its latest real tool/step from the runtime (e.g. `list_files`), not
invented stats. Token totals still come from `SubAgentProgress` when reported.

## Browser UI

Web multi-agent chrome is not required for this product surface; protocol events
`sub_agent_updated`, `sub_agent_progress`, and `sub_agent_activity` are available
for clients that bind to them.
