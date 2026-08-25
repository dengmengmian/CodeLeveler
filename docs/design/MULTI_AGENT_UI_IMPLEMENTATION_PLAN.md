# Multi-Agent UI — implementation plan

**Status:** plan · **Opened:** 2026-08-24 · **No code written**

Sequencing for [MULTI_AGENT_UX](MULTI_AGENT_UX.md). Preparation only: this
document names what must change and in what order. It does not authorize the
changes.

## The blocking fact

Every screen in the UX design renders `ChildResultProjection`. **The projection
never reaches a client.** It is dropped at one line:

`crates/leveler-app/src/event_bridge.rs:507`

```rust
EngineEvent::SubAgentFinished { id, nickname, ok, summary, .. } => {
    //                                                       ^^ contribution
    let _ = self.events.send(RuntimeEvent::SubAgentUpdated {
        id, nickname,
        role: String::new(),   // ← also dropped, on the finish event
        done: true, ok, detail: summary,
    });
}
```

Two losses at that site:

1. `contribution` is discarded by the `..` rest pattern.
2. `role` is blanked. `SubAgentStarted` (line 476) forwards the real role;
   the finish event replaces it with `""`. A client that only holds the latest
   `SubAgentUpdated` per id loses the role on completion.

Until this is fixed, no amount of renderer work produces the UX design. Fixing
it is step 1 and is small.

## Data the UI needs, and where it is today

| Datum | Exists in engine | Reaches client | Gap |
| --- | --- | --- | --- |
| child id, nickname | ✅ | ✅ | — |
| role | ✅ | ⚠️ start only | blanked on finish |
| profile id / capabilities | ✅ `SubAgentStarted` | ❌ | not on the wire |
| contribution counts | ✅ `SubAgentFinished` | ❌ | dropped at bridge |
| contribution source | ✅ (Phase 1) | ❌ | not on the wire |
| per-agent cost | ✅ `SubAgentProgress` | ✅ | — |
| finding records | ✅ `EvidenceLedgerUpdated` | ❓ | verify before designing the inspector |
| finding → fix causality | ⚠️ derivable | ❌ | see Open question 1 |

`EvidenceLedgerUpdated` carries the full ledger including `findings`. Whether
the bridge forwards it, and at what cost per update, must be measured before
the Timeline and Finding Inspector are designed further — a full ledger on
every finding change is a payload the event pipeline just finished paying down.

## Migration order

Strictly sequential. Each step is independently shippable and independently
revertible.

### Step 1 · Stop dropping what already exists

**Scope:** `event_bridge.rs` only. No new events, no new fields on the wire.

- Forward `role` on the finish branch instead of `String::new()`.
- Add `contribution: Option<ChildResultProjection>` to `SubAgentUpdated`,
  `#[serde(default, skip_serializing_if = "Option::is_none")]`.

`Option` is load-bearing: `None` means "not measured" and must render as such.
A non-optional zeroed struct would reintroduce exactly the unmeasured-reads-as-
zero defect that invalidated the reviewer pilot's first report.

**Test:** an event-bridge test asserting the projection survives the hop, and
one asserting `None` survives as `None`.

**Risk:** low. Additive, serde-defaulted, back-compatible with older clients.

### Step 2 · Profile on the wire

**Scope:** `SubAgentUpdated` gains `profile_id`, `profile_role`,
`capabilities`, forwarded from `SubAgentStarted`.

Needed for the Agent Card's "read-only" line — the capability contract is what
lets the UI state what an agent was allowed to do rather than implying it.

**Risk:** low. Same additive shape as Step 1.

### Step 3 · TUI Agent Card

**Scope:** `leveler-tui`. Files, in dependency order:

| File | Change |
| --- | --- |
| `state.rs` | hold `contribution` + profile per child |
| `reducer/runtime_apply.rs` | store them from `SubAgentUpdated` (line ~305) |
| `transcript.rs` | `complete_sub_agent` renders contribution, not just `detail` |
| `render/transcript_lines.rs` | the card layout |
| `i18n.rs` | strings — including the three distinct empty states |

The three states must be distinguishable in the strings themselves, not by
formatting: *"Nothing to flag"* (measured zero) / *"Not measured"* (no
projection) / *"Failed"* (child errored).

**Risk:** medium. Touches the transcript model, which the whole TUI reads.

### Step 4 · TUI Task Overview

**Scope:** `workbench.rs`, `status_line.rs`.

Depends on Step 3's state. Adds the open-blocking-findings line, which needs
`findings_open_blocking` summed across children — available from Step 1.

**Risk:** medium. New always-visible surface; needs a layout budget decision.

### Step 5 · Timeline

**Scope:** TUI transcript nesting, then Web.

**Blocked on Open question 1.** Do not start until causality has a data source.

### Step 6 · Web

**Scope:** `leveler-web` + frontend. Sidebar, panel, graph, detail route.

Deliberately last. The TUI is the tighter constraint; a contribution that
cannot be stated in one terminal line is not a clear enough fact to render
richly.

## What must not happen

- **No new Finding model.** `FindingRecord` / `FindingState` are the model.
  A UI-side reinterpretation of the lifecycle will drift from the ledger.
- **No payloads in events beyond the projection.** The projection is counts and
  a reference by design. Finding text belongs in the ledger, which is already
  persisted; the inspector reads it from there.
- **No client-side aggregation into a single "agents" number.** Roles are not
  interchangeable.
- **No renderer work before Step 1.** Rendering data that does not arrive
  produces a mock, and a mock in the product is a lie about what CodeLeveler
  measures.

## Validation per step

| Step | Gate |
| --- | --- |
| 1 | bridge tests; `cargo test --workspace` green |
| 2 | bridge tests; existing spawn-reliability gate unchanged |
| 3 | TUI snapshot tests for all three empty states |
| 4 | layout under narrow terminals |
| 5 | — |
| 6 | — |

Every step: **Explorer behavior unchanged, spawn reliability unchanged, no
permission regression.** These are the standing invariants for anything
touching the multi-agent path.

## Open questions for the owner

1. **Finding → fix causality has no data source.** The Timeline's central
   claim — *this change happened because of that finding* — is not recorded
   anywhere. `FindingState::Addressed` says a finding was addressed; it does
   not name the mutation. Options:
   - (a) record the mutation seq on the finding at `Addressed`
   - (b) infer from ordering — fragile, and inference presented as fact is
     worse than an honest gap
   - (c) drop the causal claim; render the timeline chronologically

   This changes runtime data. **Documented, not implemented**, per the freeze.
   *Leaning: (a), as a small additive field, once the Reviewer evaluation
   unfreezes.*

2. **Ledger forwarding cost.** Does `EvidenceLedgerUpdated` reach clients
   today, and what does a full-ledger payload cost per finding change? Measure
   before designing the inspector, not after.

3. **Web frontend location.** `crates/leveler-web` is a Rust server; where the
   frontend lives and what it is built with was not established in this survey.
   Step 6 cannot be estimated until it is.

## Sequencing against the product

Steps 1–2 are pure observability plumbing and are safe to land during the
Reviewer evaluation freeze — they change no agent behavior. **They also unblock
the formal experiment's secondary metrics**, so doing them early has value
beyond the UI.

Steps 3–6 are product surface and should wait for
[MA-VALUE-REVIEWER-FORMAL](../evaluations/MA-VALUE-REVIEWER-FORMAL.md). If the
Reviewer does not earn its cost, the UI that showcases Reviewer contribution is
showcasing a feature that should be re-scoped, and the work is wasted.

## Related

- [MULTI_AGENT_UX](MULTI_AGENT_UX.md)
- [CHILD_PROFILE](CHILD_PROFILE.md)
- [MA-VALUE-REVIEWER-FORMAL](../evaluations/MA-VALUE-REVIEWER-FORMAL.md)
