# Multi-agent product design (minimal)

Main stays the authoritative owner. Explorer investigates, Worker executes scoped subtasks,
Reviewer judges — all through the ONE existing child primitive, all reporting into ONE durable
findings ledger, all gated by the existing completion truth.

## A. Findings — the one new model

`FindingRecord { id, source_child, role, kind, summary, file?, symbol?, blocking, state,
resolution_reason? }` living in **EvidenceLedger.findings** (serde-default field ⇒ replay
compatible, persisted by the existing `EvidenceLedgerUpdated` events — no second persistence
system, no new event stream).

Kinds (small, closed): `relevant_file, relevant_symbol, dependency, callsite, risk, test,
config, observation, correctness`.

States and legal transitions:

```
Created → Acknowledged → Accepted → Addressed → Verified
                       ↘ Rejected(reason)     (Accepted → Rejected(reason) also legal)
```

No other jumps. `Created → Verified` is illegal by construction.

## B. How findings are born — typed at the source

Children (depth > 0) get one injected host tool, **`report_finding`**, intercepted exactly like
`update_goal`: validated, recorded into the child's ledger, persisted via EvidenceLedgerUpdated.
No parsing of model prose, ever. Explorer/Reviewer prompts direct the child to report each
discovery/defect as it is confirmed (which also gives every abnormal stop real partial content).

## C. Flow to the parent

Typed records travel on `SubAgentRunResult` / `DelegatedChildResult` (the existing child-join
structs). `ChildResult` keeps the N1 prose status line. On join, the parent adopts the typed
records into ITS ledger at `Acknowledged` (automatic, deterministic — receipt is not judgment).

## D. Parent judgment

Parent-side injected tool **`resolve_finding { id, resolution: accepted|rejected|addressed,
reason? }`**. Reject requires a reason. All transitions durable through the same ledger events.
An incomplete Worker also produces a host blocking finding the parent must settle.

## E. Closure truth

At `conclude_direct`'s closure boundary (beside the review stage):
- a **blocking** finding not in `{Rejected, Verified}` ⇒ `Verified` becomes
  `CompletedUnverified`, with a persisted `review_stage{action:"blocking_finding_open"}` naming it;
- `Addressed` findings are host-promoted to `Verified` only when a fresh post-mutation green
  verification exists (reusing `has_fresh_successful_verify` — N2 semantics untouched).

## F. Worker scope safety

Same-batch spawn admission rejects a worker whose `files` overlap an already-accepted worker's
scope (path or directory-prefix overlap) — honest denial, not last-writer-wins. Per-child
enforcement stays as is.

## G. Child profiles

One resolve point (`ChildProfile::resolve(role)`) formalising what already exists: explorer =
read-only registry, no scope; worker = full registry + required scope + serial tools; reviewer =
read-only + 20-round bound; default = full registry. Contract tests assert the capability truths
(explorer/reviewer registries contain no mutating tool, etc.). No new permission machinery.

## NON-GOALS

Second runtime/scheduler/EventLog; ACP or remote capability negotiation; child providers;
multi-level hierarchies (depth stays 1); swarm/voting/debate; trajectory viewer; automatic
harness-spawned Explorer (Main decides, as today); fixing R012-F1 or other deferred findings.
