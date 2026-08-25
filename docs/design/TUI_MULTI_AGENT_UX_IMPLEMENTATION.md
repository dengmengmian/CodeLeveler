# TUI Multi-Agent UX — Phase 1 implementation

**Status:** Phase 1 + Phase 2 implemented · 2026-08-25 · **No runtime behavior changed**

What was built, where it lives, and what a future Web client can reuse.
Analysis of the starting state is in
[TUI_MULTI_AGENT_UX_ANALYSIS](TUI_MULTI_AGENT_UX_ANALYSIS.md).

## What changed, in one sentence

The TUI stopped deriving facts from prose and started rendering the projection
the runtime already computed.

## Data flow

```
leveler-engine                leveler-app                leveler-client-protocol
──────────────────            ────────────────           ───────────────────────
SubAgentStarted            →  bridge forwards         →  SubAgentUpdated {
  {role, profile_id,           profile + role             role, profile_id,
   profile_role,               (was: dropped)             profile_role,
   capabilities}                                          capabilities,
                                                          contribution }
SubAgentFinished           →  project_contribution()  →
  {contribution:               (was: dropped)
   ChildResultProjection}

                                                       leveler-tui
                                                       ───────────────────────
                                                    →  reducer/runtime_apply
                                                         ├→ state.team  (TaskTeamView)
                                                         └→ transcript  (SubAgentBlock)
                                                    →  render/transcript_lines
```

### Bridge

`crates/leveler-app/src/event_bridge.rs`

- `SubAgentStarted` no longer discards `profile_id` / `profile_role` /
  `capabilities` through a `..` pattern.
- `SubAgentFinished` no longer sends `role: String::new()`. The bridge keeps a
  `child_roles` map from the spawn so the terminal event carries the role the
  child was announced under, falling back to the projection's own `role`.
- `project_contribution()` maps `ChildResultProjection` → `ChildContribution`.
  Deliberately total: every field crosses, because a field dropped there is
  invisible at the call site and unrecoverable downstream — which is exactly how
  `contribution` was lost before.

### Wire type

`crates/leveler-client-protocol/src/event.rs`

`ChildContribution` is a flat mirror of the runtime projection rather than the
runtime type itself. This crate is the stable client protocol; an internal
refactor of the evidence ledger must not change what clients parse. It carries
role, profile, capabilities, contribution source, and the six lifecycle counts.

`SubAgentUpdated.contribution` is `Option<ChildContribution>`:

- `None` — **not measured**. The runtime produced no projection.
- `Some` with zero counts — **measured zero**. The child ran and reported
  nothing.

Those are different facts. Collapsing them is the defect that made an eval
report claim five reviewers found nothing when all five had reported.

## UI model

`crates/leveler-tui/src/multi_agent.rs`

```rust
TaskTeamView { children: Vec<ChildAgentView> }

ChildAgentView {
    id, nickname, role,
    profile_id, capabilities,
    purpose,                  // what it was asked to do — leads the running line
    status: ChildStatus,      // Waiting | Running | Completed | Failed
    contribution: Contribution,
    recent_step, input_tokens, output_tokens, started_elapsed_secs,
}

Contribution {
    Pending,                  // not finished
    NotMeasured,              // finished, no projection — unknown, not zero
    NothingToFlag,            // finished, reported nothing — a result
    Reported { total, accepted, verified, rejected, open_blocking },
}
```

Behaviour worth naming:

| Method | Why it exists |
| --- | --- |
| `Contribution::engaged()` | A rejection counts — the parent looked and decided. A finding nobody judged does not. |
| `ChildAgentView::is_read_only()` | Lets the UI *state* read-only from the capability contract instead of implying it. |
| `TaskTeamView::open_blocking()` | Findings that still gate closure, summed across the team. A block discovered at the end is a block the user should have seen coming. |
| `apply_update()` upserts by id | A child transitions in place rather than appearing twice; a finish event never overwrites the purpose. |

`contribution_line()` and `running_line()` live beside the type rather than in
the renderer: "reviewed, nothing to flag" and "not measured" are product
statements, and keeping them next to the type that distinguishes them stops
them drifting apart.

## Renderer

`crates/leveler-tui/src/render/transcript_lines.rs`

- `SubAgentBlock.finding_count: usize` → `contribution: Contribution`.
- `count_adopted_findings(&summary)` is no longer how a finished child gets its
  numbers. The old path scanned the finish summary — a sentence written for a
  language model — for `Structured findings adopted: f-1, f-2`.
- The head line shows a contribution statement instead of `3 findings`, and
  shows one for a clean review where it previously showed nothing.

Three empty states, distinguished in the strings and not only in formatting:

| State | English |
| --- | --- |
| measured zero | `reviewed, nothing to flag` |
| no projection | `contribution not measured` |
| reported, unjudged | `{n} findings · none judged yet` |

`i18n.rs` carries all four strings (the fourth is the judged case) in both
locales.

## Phase 2 — Contribution Inspector

Phase 1 answered *why is this child running*. Phase 2 answers *what did the
waiting buy*.

### The architectural choice

Findings could reach the client two ways. The one not taken:

```
Child → FindingRecord → FindingUpdated event → TUI
```

Rejected. It puts the same record in two places and re-grows the event
payloads the pipeline work deliberately trimmed. It also inverts the split the
project already settled on:

```
EventLog  →  what happened, in order
Ledger    →  what is true now
```

Findings are ledger facts. So the inspector **queries**:

```
user opens a detail view
   ↓
ClientCommand::QueryChildContribution { session_id, child_id, query_id }
   ↓
last EvidenceLedgerUpdated snapshot, filtered by source_child
   ↓
RuntimeEvent::ChildContributionLoaded { query_id, detail }
   ↓
TaskTeamView::apply_detail()
```

Modelled directly on `QueryObservability` / `ObservabilityLoaded`, including
the `query_id` correlation token so a client ignores another client's — or its
own stale — response.

**No event was added to the turn stream. No agent behavior changed.**

### Wire types

`leveler-client-protocol/src/contribution.rs`

```rust
UiChildContribution {
    child_id, role, profile_id, capabilities,
    findings: Vec<UiFinding>,
    measured: bool,        // false = the question could not be answered
}

UiFinding { id, kind, summary, file, symbol, state, resolution_reason, blocking }
```

Projections of `FindingRecord`, not the record itself — same reasoning as
`ChildContribution` in Phase 1.

`measured: false` means no ledger snapshot exists. **It does not mean the child
found nothing**, and the inspector renders the two differently. `findings` is
capped at `CONTRIBUTION_FINDINGS_MAX` (200); the projection counts remain the
authority for totals.

### Runtime

`leveler-app/src/contribution_query.rs`

- `project_child_contribution()` — pure, takes a ledger, returns the read
  model. Testable without a database.
- `last_ledger()` — the last persisted `EvidenceLedgerUpdated` for a session.
- `child_identity()` — role, profile and capabilities from the child's spawn
  event. These are the child's own facts, not ledger facts, and reading them
  from the event rather than inferring from the id keeps the inspector honest
  about a child whose profile was never recorded.

### Rendering

`multi_agent.rs::inspector_rows()` returns `Option<Vec<InspectorRow>>`.

`None` means nothing has been loaded yet, and the caller shows a loading state
— **an empty list is a claim, and "not asked yet" is not one.**

Row content:

| Finding state | Rendered |
| --- | --- |
| any | `src/auth.rs — summary [state]` |
| rejected | `… [rejected] · covered by the existing guard` |
| blocking, not yet resolved | flagged via `InspectorRow::blocking` |

A rejection always carries its reason: a judgement without one is
indistinguishable from being ignored.

Footer: `{accepted} accepted · {verified} verified · {rejected} rejected`, plus
`{n} not judged` when any finding was never judged — the protocol's definition
of noise, made visible.

Three terminal states, again distinguished in the strings:

| State | Rendered |
| --- | --- |
| measured, no findings | `reviewed, nothing to flag` — no tally, because a clean review is not a count of zeros |
| not measured | `contribution not measured` |
| loaded with findings | the list plus the footer |

### Phase 2 tests

`multi_agent.rs` — 9 added (30 total):

- nothing renders until something is loaded
- explorer with findings shows file, state and the accepted/verified tally
- **a clean review reads as a result and shows no `0 accepted` tally**
- a rejected finding carries the reason it was declined
- **an unmeasured detail says so rather than listing nothing**
- an open blocking finding is marked; a verified one is not
- unjudged findings are called out separately
- read-only is stated from the capability contract
- a late response for an unknown child is ignored

`contribution_query.rs` — 8:

- a child sees only its own findings
- **no ledger is unmeasured, not clean**
- a measured child with no findings is a clean review
- lifecycle states counted by what the parent did (verified counts as accepted)
- a rejection carries its reason
- the capability contract travels with the detail
- the findings list is bounded
- blocking is preserved

## What Phase 1 does not do

Recorded so the gaps are not mistaken for oversights. All three need runtime or
protocol changes and are out of scope while agent behavior is frozen.

| Missing | Where the data is | Why it is not rendered |
| --- | --- | --- |
| Per-child changed files | `SubAgentRunResult.modified_files` | Appended into the finish summary as prose. Rendering it means parsing prose — the defect this phase removed. |
| Per-child verification | task-level `VerificationUpdated` | Verification is a task fact; there is no per-child verify record. |
| Finding text / file / kind | `EvidenceLedger.findings` | `EvidenceLedgerUpdated` is bridged to a no-op. Forwarding the whole ledger per update is a payload question the event-pipeline work deliberately paid down. |

Consequence: the Agent Detail View shows the lifecycle breakdown and the
capability contract, not per-finding rows. The counts are real; the rows do not
exist yet.

Options if they are wanted later, recorded not chosen:

- `modified_files` as an additive field on `SubAgentFinished`
- a `FindingsUpdated` event carrying findings only, not the whole ledger

## Tests

`multi_agent.rs` — 21 unit tests:

- running child leads with purpose, not status
- progress separates running from queued
- transitions in place, finish does not erase the purpose
- explorer contribution reports what the parent accepted
- **zero findings is a result, not an empty state** — and the line contains no `0`
- **unmeasured is not a zero** — asserts `NotMeasured != NothingToFlag`
- failed child stays failed even with a projection
- findings nobody judged are not engagement; a rejection is
- read-only derived from the capability contract
- open blocking findings surface at team level
- role from spawn survives a finish without one

`event_bridge.rs` — 3 added:

- contribution survives the hop, role not blanked
- unmeasured stays `None`
- profile survives the hop

## Web reuse

`ChildContribution` is on the wire and language-neutral, so a Web client gets
the same facts with no further runtime work.

`TaskTeamView` currently lives in `leveler-tui`. It has no ratatui dependency —
only `leveler_client_protocol` and `crate::i18n` — so the move is mechanical
when a second client needs it: lift the module into a shared crate and pass the
string table in rather than reaching for `crate::i18n`.

Do that when the second consumer exists, not before.

## Related

- [TUI_MULTI_AGENT_UX_ANALYSIS](TUI_MULTI_AGENT_UX_ANALYSIS.md)
- [MULTI_AGENT_UX](MULTI_AGENT_UX.md)
- [MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY](../evaluations/MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY.md) — why "nothing to flag" is the common case
