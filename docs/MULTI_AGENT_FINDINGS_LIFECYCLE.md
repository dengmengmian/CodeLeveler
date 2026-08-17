# Multi-agent findings lifecycle

One typed record, one ledger, one EventLog path. Findings live on
`EvidenceLedger` and persist through existing `EvidenceLedgerUpdated` snapshots.

## Record

```
FindingRecord { id, source_child, role, kind, summary, file?, symbol?, blocking, state, resolution_reason? }
```

Kinds (closed): `relevant_file`, `relevant_symbol`, `dependency`, `callsite`,
`risk`, `test`, `config`, `observation`, `correctness`.

Identity is `f-{seq}` assigned by the ledger that owns the record. Adoption
re-keys into the parent sequence so child ids never collide.

## States

```
Created → Acknowledged → Accepted → Addressed → Verified
                       ↘ Rejected            (Accepted → Rejected is also legal)
```

`Created → Verified` is illegal. `Verified` is host-promotion only, never a
model write.

Not every finding walks the full path. Explorer knowledge typically stops at
Accepted. A reviewer correctness finding that is `blocking` must reach
Rejected or Verified before the task can be `Verified`.

## How they are born

| Source | How | Landing state |
| --- | --- | --- |
| Child `report_finding` | validated at the tool boundary, recorded on the child's ledger | Created |
| Parent join / harness reviewer | `adopt_finding` | Acknowledged |
| Host (incomplete Worker) | `record_parent_finding` | Acknowledged + blocking |

No prose parsing.

## Parent judgment

`resolve_finding { id, resolution: accepted\|rejected\|addressed, reason? }`.
Reject requires a reason. Addressed becomes Verified only when
`has_fresh_successful_verify` is true (N2: baseline-green does not count).

## Closure truth

An open blocking finding (`blocking && state ∉ {Rejected, Verified}`) refuses:

- `update_goal(complete)` (agent intercept `blocking_finding`)
- engine `TaskOutcome::Verified` (staged as `review_stage{action:blocking_finding_open}`)

If the findings ledger cannot be read at the closure boundary, Verified is
refused (fail-closed).

Rejected findings stay in the ledger. They never disappear.

## Replay

Last `EvidenceLedgerUpdated` snapshot is the seed. Adoption is not replayed, so
ids do not duplicate. A fresh epoch that does not inherit mutation/verify
evidence still carries unsettled findings via `carry_forward_findings`.
