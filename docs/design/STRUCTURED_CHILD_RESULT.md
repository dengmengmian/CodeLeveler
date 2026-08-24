# Structured Child Result — Phase 1 analysis

**Opened:** 2026-08-24 · **Subject:** `main` @ `ac1352ba` · **Status:** analysis
only, no code changed

## The short version

Most of this contract **already exists and is heavily used**. The brief assumed
child output is free-form prose reaching a parent that has no structured view of
it. That is half true, and the half that is false changes what should be built.

What exists: a fully structured, durable, traceable finding record with
parent-consumption states. What is missing: the child's **result envelope**
still carries its substance as prose, and the two channels are not the same
channel.

## What is actually there

### `FindingRecord` — already structured, already traceable

`crates/leveler-lifecycle/src/findings.rs:92`

```
id
source_child        ← which child produced it
role                ← explorer / worker / reviewer
kind                ← RelevantFile · Risk · Correctness · Test · Config · …
summary
file, symbol        ← location
blocking            ← an open blocking finding prevents verified closure
state               ← consumption tracking, below
resolution_reason   ← required when Rejected
```

### `FindingState` — parent consumption is already modelled

```
Created → Acknowledged → Accepted → Addressed → Verified
                       ↘ Rejected (terminal, reason required)
```

That is exactly the chain the brief asks for: *generated → parent saw it →
parent judged it → work done → proven*. `Acknowledged` even carries the right
caveat in its own doc comment — "receipt is not judgment".

### It is not theoretical — it runs at scale

Usage across the 20 formal MA-VALUE-A runs:

| Arm | `report_finding` | `resolve_finding` |
| --- | --- | --- |
| Control (single agent) | 6 | 1 |
| **Treatment (multi-agent)** | **490** | **30** |

An 80× difference. Children report findings prolifically; a single agent barely
touches the mechanism. **This is itself a result**: the structured-finding path
is close to being a multi-agent-only feature in practice.

### Durability

`EvidenceLedgerUpdated` is a canonical (non-transient) engine event, so ledgers
including their findings are persisted and replay with the session.

## The actual gap

There are **two parallel channels** carrying child output, and they are not
connected the way the brief assumes.

```
Channel 1 — what the PARENT MODEL reads
  ChildResult {
      status: ChildStatus,   ✓ structured (4 states)
      findings: String,      ✗ FREE-FORM PROSE
      stop_reason: String,   ~ prose, bounded
      partial: bool,         ✓
  }
        ↓  for_parent(nickname) → one text blob
  SubAgentFinished { id, nickname, ok, summary }   ← summary is a prose preview

Channel 2 — what is DURABLE and TRACEABLE
  EvidenceLedger.findings: Vec<FindingRecord>      ← fully structured
```

So the three gaps, precisely:

1. **The envelope is prose.** `ChildResult.findings` is a `String`. The parent
   model consumes text; the structured records live somewhere else.
2. **The terminal event carries a preview, not a result.**
   `SubAgentFinished.summary` is a truncated string. Replaying the log tells you
   a child finished and roughly what it said — not what it found, in what form.
3. **The envelope lacks fields the brief names**: no `changed_files`, no
   `verification`, no `metrics` on `ChildResult` itself. Some exist at the call
   site (`result.modified_files` is read in `drive.rs`) but they are not part of
   the child's own contract.

## Why this matters, concretely

MA-VALUE-A could measure *that* Multi-Agent scored 16 % higher, but not *why*.
The reason is exactly gap 1: the scorer graded the parent's final prose report,
because that is the artefact the pipeline produces. The finding records existed
the whole time — 490 of them — and nothing joined them to the outcome.

That is a measurement gap created by a contract gap, and it is what makes this
work observability rather than architectural tidiness. The brief is right about
the goal; it is the diagnosis that needed correcting.

## What Phase 2 should therefore define

**Not a new parallel structure.** `FindingRecord` is the finding contract and it
is good. What needs defining is the **envelope** that carries those records
across the child→parent boundary:

```
StructuredChildResult {
    agent_id, parent_id, role
    status            ← reuse ChildStatus, do not invent a second enum
    summary           ← keep the prose; the model still reads it
    findings          ← Vec<FindingRecord>, NOT a String
    changed_files
    verification
    metrics
}
```

Two constraints that follow from what is already built:

- **Keep the prose.** The parent model reads `for_parent()` text and that path
  works. Adding structure must be additive, or the spawn workflow that just
  demonstrated +16 % breaks.
- **Reuse `FindingRecord` and `FindingState` verbatim.** A second finding type
  would fork the consumption chain that already works, which is the one part of
  this the codebase got right first.

## What this analysis does not settle

- Whether `SubAgentFinished` should carry the full structured result or a
  reference to it. Embedding a large record in every terminal event has a cost
  the event-pipeline work just finished paying down.
- Whether `changed_files` / `verification` / `metrics` belong on the child
  contract or are better read from the child's own ledger, which already holds
  mutations and verifications.

Both are Phase 2 decisions and neither is answered by reading the code.
