# Multi-Agent Contribution Trace

**Opened:** 2026-08-24 · **Subject:** `main` @ `06e265af` · **Status:**
implemented, no runtime semantics changed

Answers one question that could not be answered before: **which child's finding
affected the final result?**

## Existing architecture

Two channels carried child output, and they were not the same channel.

```
Channel A — what the parent MODEL reads
  ChildResult.findings : String       ← prose
        ↓ for_parent()
  SubAgentFinished { summary }        ← a truncated preview

Channel B — what is DURABLE and JOINABLE
  EvidenceLedger.findings : Vec<FindingRecord>
      · source_child   ← which child
      · role
      · state          ← Created → Acknowledged → Accepted → Addressed → Verified
      ·                             ↘ Rejected (terminal, reason required)
```

Channel B was already complete, already durable (`EvidenceLedgerUpdated` is
canonical, not transient), and already used hard: across the twenty formal
MA-VALUE-A runs the treatment arm made **490** `report_finding` calls against the
control's **6**.

## The gap

Nothing joined A to B. `SubAgentFinished` carried prose, so replaying a log told
you a child finished and roughly what it said — never what it found in a form
anything could join on.

That is precisely why MA-VALUE-A could measure that Multi-Agent scored 16 %
higher and not *why*: the scorer graded the parent's prose, because prose is
what the pipeline produced, while 490 finding records sat beside it unjoined.

**A measurement gap created by a contract gap.** Observability work, not
architectural tidiness.

## Design decision

**Events carry a reference and counts. Never the records.**

```rust
ChildResultProjection {
    child_id,                    // joins FindingRecord::source_child
    role,
    findings_total,
    findings_acknowledged,       // reached the parent at all
    findings_accepted,           // Accepted | Addressed | Verified
    findings_verified,
    findings_rejected,
    findings_open_blocking,
}
```

Three choices worth defending:

**No second Finding type.** `FindingRecord` and `FindingState` are reused
verbatim. Forking the consumption chain would break the one part of this the
codebase already got right.

**A rejection counts as contribution.** `contributed()` is
`accepted > 0 || rejected > 0`. The parent read the finding and made a call —
that is the child doing its job. Counting only acceptances would score a child
whose findings are never judged *above* one whose findings are judged and
declined, which inverts the thing being measured.

**`None` means "not measured", zeros mean "contributed nothing".** They are
different facts and are kept different. A child that never reported, and the
review stage which runs outside the executor's ledger, both project `None`
rather than a zeroed record that would read as a judgement.

## Implementation

| Layer | Change |
| --- | --- |
| `leveler-lifecycle/findings.rs` | `ChildResultProjection` + `from_findings()` (pure) + `contributed()` |
| `leveler-agent/executor.rs` | `AgentEvent::SubAgentFinished` gains `contribution: Option<…>` |
| `leveler-agent/executor/drive.rs` | projection computed at settlement, where child identity and the parent ledger are both in scope |
| `leveler-engine/event.rs` | `EngineEvent::SubAgentFinished` gains the field, `#[serde(default)]` so old events replay |
| `leveler-engine/turn.rs` | ghost-child settlement and the review stage pass `None` — honestly "not measured" |
| `leveler-app/observability.rs` | trace surfaces `Findings: N reported · N accepted · N rejected · N verified` |
| `leveler-cli/render.rs` | JSON output carries the projection, so eval reads counts instead of parsing prose |

`from_findings` is pure — records in, counts out — so contribution logic is
testable without a running agent.

**Not changed:** spawn semantics, child execution, prompts, tool schema, the
prose the parent model reads. The `for_parent()` path that just demonstrated
+16 % is untouched; this is additive.

## Tests

| Test | Pins |
| --- | --- |
| `a_projection_counts_only_its_own_child` | the join is by `source_child`; a sibling's finding is not yours |
| `a_rejected_finding_still_counts_as_contribution` | judged-and-declined is contribution |
| `findings_nobody_judged_are_not_contribution` | receipt is not judgment |
| `open_blocking_findings_are_surfaced_separately` | a rejected blocking finding no longer blocks |
| `a_child_with_no_findings_projects_zeros` | empty projects cleanly |
| `a_projection_roundtrips_through_json` | survives replay |
| `a_settled_child_reports_a_contribution_projection` | end to end: the terminal event carries a projection naming its child |
| `observability` (app) | the counts actually reach `leveler trace`, not merely compile |

The end-to-end test asserts *presence and attribution*, not counts: its scripted
child reports nothing, so the counts are zero. Zero-from-a-child-that-ran and
`None` are asserted as different, because conflating them is how a child that
contributed nothing becomes indistinguishable from a child nobody looked at.

## Limitations

- **The review stage projects `None`.** It runs outside the executor's ledger,
  so no projection is available there. Honest, but a hole.
- **`changed_files` / `verification` / `metrics` are not on the projection.**
  The child's own ledger already holds mutations and verifications; duplicating
  them into an event was rejected under the no-payloads rule. Whether the parent
  should see them at settlement is unresolved.
- **Not in the client protocol.** `UiAgentObservation` is unwidened, so the Web
  and TUI clients cannot render contribution yet. Deliberate: widening it pulls
  in schema regeneration and generated TS, which is UX-phase work.

## Future UI implication

MA-VALUE-A priced the real cost of delegation at **2.5× wall time**, and the
surface currently shows *"等待任务"*. This work supplies what a better surface
needs: not a count of running agents, but an answer to **why the wait is worth
it**.

```
Euclid    explorer   ✓   7 reported · 5 accepted · 2 verified
Newton    explorer   ✓   4 reported · 1 accepted · 3 rejected
Curie     explorer   ✓   9 reported · 0 accepted        ← nothing landed
```

The third row is the one that matters. It is the case a spinner cannot show and
an agent count actively hides, and it is now derivable from a single terminal
event.
