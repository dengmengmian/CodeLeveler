# TUI Multi-Agent UX — current-state analysis

**Status:** analysis · 2026-08-25 · No runtime behavior changed

What the TUI shows about child agents today, what data it has, and what it is
missing. Input to the Phase 1 implementation.

## Current flow

```
EngineEvent                 event_bridge.rs            RuntimeEvent
─────────────────────────   ───────────────────────    ────────────────────
SubAgentStarted          →  role forwarded          →  SubAgentUpdated{done:false}
  {id,nickname,role,task,    profile DROPPED (`..`)
   profile_id,profile_role,
   capabilities}

SubAgentProgress         →  passthrough             →  SubAgentProgress
SubAgentActivity         →  passthrough             →  SubAgentActivity

SubAgentFinished         →  role BLANKED            →  SubAgentUpdated{done:true}
  {id,nickname,ok,summary,   contribution DROPPED
   contribution}             (`..`)

EvidenceLedgerUpdated    →  no-op, deliberately     →  (nothing)
```

```
RuntimeEvent → reducer/runtime_apply.rs → transcript.rs        → render/transcript_lines.rs
                                          SubAgentBlock          sub_agent_lines()
```

`SubAgentBlock` holds: `id`, `nickname`, `role`, `status`, `detail`,
`progress{active,tokens}`, `recent_step`, `started_elapsed_secs`,
`finding_count`, `expanded`.

The renderer produces one line:

```
✓ Explorer · completed · 3 findings · 12s · read_file
```

Plus the result summary, truncated unless expanded (Ctrl+O).

## What is actually wrong

### 1. The UI parses prose to get a number

`transcript.rs:747`

```rust
let finding_count = Self::count_adopted_findings(&summary);
```

`finding_count` is recovered by scanning the finish summary text for the
`Structured findings adopted: f-1, f-2` line the runtime writes for the model.
The projection carrying those counts was computed at settlement and thrown
away one layer up.

Consequences:

- Only a total is recoverable. `accepted`, `verified`, `rejected` — the counts
  that say whether the work *mattered* — are not in the prose at all.
- It is coupled to a sentence written for a language model. Rewording the
  parent-facing summary silently changes what the user sees.
- A child that reported nothing and a child nobody measured both render as
  `finding_count: 0`.

### 2. Finding count is the metric on screen

`transcript_lines.rs:553` shows `3 findings` when the count is non-zero, and
nothing otherwise. That is backwards twice over:

- Volume is not contribution. Three findings the parent ignored are worse than
  one it verified.
- Zero is rendered as absence. "Reviewed 12 files, nothing to flag" is a
  result, and the current UI has no way to say it — it just omits the segment.

The [Reviewer boundary study](../evaluations/MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY.md)
measured 15/15 control runs where nothing-to-flag was the *correct* answer.
A UI that renders that as blank is hiding the most common outcome.

### 3. Role is lost at the finish event

`event_bridge.rs` sent `role: String::new()` on `SubAgentFinished`. A client
holding only the latest `SubAgentUpdated` per id loses the role exactly when it
starts rendering the result. The TUI survives this only because it mutates an
existing block in place rather than replacing it.

### 4. The capability contract never reaches the client

`profile_id`, `profile_role` and `capabilities` are on `SubAgentStarted` and
dropped by the bridge's `..` pattern. The UI cannot say a reviewer is read-only
because it does not know.

### 5. Waiting is unexplained

`sub_agent_status()` maps `Running + !active` to `t.sub_agent_waiting`. The
user sees "waiting" with no reason. The runtime does know what the child was
asked to do — it is in `detail` — but the running line leads with status, not
purpose.

## What data exists, after the Phase 1 bridge fix

| Datum | Source | On the wire |
| --- | --- | --- |
| id, nickname, detail | `SubAgentStarted/Finished` | ✅ |
| role | `SubAgentStarted`, now kept through finish | ✅ fixed |
| profile_id / profile_role / capabilities | `SubAgentStarted` | ✅ fixed |
| findings total/acknowledged/accepted/verified/rejected/open_blocking | `SubAgentFinished.contribution` | ✅ fixed |
| contribution source | `ContributionSource` | ✅ fixed |
| live tokens | `SubAgentProgress` | ✅ |
| current tool | `SubAgentActivity` | ✅ |

## What is still missing — not invented

| Datum | Where it exists | Why it does not reach the UI |
| --- | --- | --- |
| **Per-child changed files** | `SubAgentRunResult.modified_files` | Appended into the finish summary as prose (`Files touched: a.go, b.go`), never a structured field. Recovering it means parsing prose — the defect above. |
| **Per-child verification** | task-level `VerificationUpdated` | Verification is a task fact, not a child fact. There is no per-child verify record. |
| **Finding text / file / kind** | `EvidenceLedger.findings` | `EvidenceLedgerUpdated` is bridged to a no-op. The ledger is the full record; forwarding it per update is a payload question the event pipeline work was explicitly paying down. |

**Consequence for Phase 1 scope.** The prompt's Agent Detail View asks for an
Explorer findings list (file, reason, confidence) and a Worker changed-files
list. Neither is available as structured data. Phase 1 renders what exists —
counts, profile, purpose, status, cost — and the detail view shows the
lifecycle breakdown rather than per-finding rows.

Adding them is a runtime/protocol change and is out of Phase 1 scope. Options,
recorded not chosen:

- `modified_files` as a field on `SubAgentFinished` (small, additive)
- a `FindingsUpdated` event carrying only findings, not the whole ledger

## Design constraints this implies

1. **A projection layer, not renderer logic.** The TUI must stop deriving facts
   from text. One typed view model, built from events, consumed by the
   renderer.
2. **Three distinct empty states.** Nothing to flag (measured zero) / not
   measured (no projection) / failed. They must differ in the strings, not
   only in formatting.
3. **Purpose leads on the running line.** "Analyzing repository structure"
   before "running".
4. **Contribution, not count.** Lead with what the parent did with the work.

## Related

- [MULTI_AGENT_UX](MULTI_AGENT_UX.md) — the design this implements
- [MULTI_AGENT_UI_IMPLEMENTATION_PLAN](MULTI_AGENT_UI_IMPLEMENTATION_PLAN.md)
- [MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY](../evaluations/MA-VALUE-REVIEWER-CAPABILITY-BOUNDARY.md)
