# Continuable SubAgent — Phase 0 Current-State Audit & Minimal Durable Child Design

Status: audit + design only. No production code changed.
Baseline: main = eebb0b67. Sources: full code audit of leveler-agent
(sub_agent.rs, executor/drive.rs, executor/handlers.rs, ownership.rs,
child_profile.rs), leveler-engine (event.rs, log.rs, turn.rs, engine.rs,
reaper.rs, recovery.rs), leveler-storage, leveler-execution, leveler-app.
File:line references were verified at this baseline; they drift with edits.

The question this audit answers: can CodeLeveler evolve from
`Child = process-local execution` to `ChildSession = durable identity +
Activation = process-local execution` without a second runtime, without
unsafe replay, and without weakening existing write/completion safety?

**Answer: yes — the identity and safety substrate already exists; what is
missing is child context persistence, terminal idempotency, and durable
budget/caps. Architecture: EXISTING_CHILD_ID_PLUS_ACTIVATION. Beta
classification: POST_BETA.**

---

## 1. Current architecture (verified lifecycle trace, condensed)

    spawn_agent (deferred to batch pass, drive.rs ~1726)
      → admission: profile/role/scope contract, sibling overlap, live-claim
        conflict, total cap (all in-memory)
      → child_id = AgentId UUIDv4 (sub_agent.rs new_delegated_agent_id)
      → SubAgentStarted { id, nickname, role, task(FULL text + [scope:…]),
        profile_id/role/capabilities } — durable, but QUEUED (no barrier
        flush at spawn; becomes durable at the pump's next batch / the
        child's first admit() flush; the reviewer path by contrast awaits
        the append before running)
      → child Executor cloned from parent (shared ownership registry, shared
        event barrier + execution fence, permission rules cloned BY VALUE,
        clarifier→AutoClarify, sink→SubAgentProgressSink whose append() is
        a NO-OP)
      → tokio::spawn, handle in drive-local BackgroundChildren (Drop =
        abort + release_all — the crash path; every normal exit DRAINS)
      → outstanding_children += "id|nickname|role|scope" via ProgressUpdated
        (durable; BACKGROUND children only; unescaped pipe-packing)
      → child tool calls: durable in the PARENT log as
        ToolCallStarted/Finished{agent_id} (durable-before-effect via the
        barrier); claim_write_scope → DelegationStage{ownership_granted/
        denied} (durable fact; lease itself in-memory only)
      → child's own transcript / ContextSnapshot / model_requests /
        EvidenceLedgerUpdated: captured into in-process mutexes or DROPPED
        (handlers.rs capture closure `_ => {}`); findings hand over ONLY at
        settlement
      → settlement: fold_child_settlement — absorb spend, adopt findings
        (re-keyed, state=Acknowledged), Worker-incomplete → BLOCKING parent
        finding, emit EvidenceLedgerUpdated then SubAgentFinished{ok,
        summary(1200-char preview), contribution: ChildResultProjection}
      → settlement notice (FULL untruncated result) sink-appended to the
        parent transcript (必改④) — the richest durable child artifact
      → clear_outstanding_child + ownership.release_all + ProgressUpdated

Turn end: every exit path drains (awaits) children; the engine then writes
a synthetic SubAgentFinished{ok:false, contribution:None} for any child
with a Started and no Finished (turn.rs ghost reconciler) — but this runs
at the END OF THE NEXT TURN, not at resume, and reap_after_restart reaps
turns only, never children.

## 2. Durable / process-local matrix (verified)

| Fact | Class |
|---|---|
| child_id (UUIDv4) | DURABLE_CANONICAL — keyed on Started/Finished, ToolCall agent_id, FindingRecord.source_child, ChildResultProjection, CheckpointChild |
| child task/objective (full composed text incl. persona) | DURABLE_CANONICAL (SubAgentStarted.task, untruncated) |
| role / profile_id / profile_role / capabilities | DURABLE_CANONICAL |
| parent session linkage | DURABLE_DERIVED (row's session log) |
| parent turn linkage | DURABLE_DERIVED and partly wrong (UnfinishedChild.turn_id captured but its only consumer stamps the CURRENT turn; reviewer events carry turn_id:None) |
| goal linkage | RECONSTRUCTABLE (weak; no goal id on child events) |
| child transcript / messages | LOST (sink no-op) |
| child ContextSnapshot | LOST (capture closure drops it) |
| child model_requests | LOST |
| child findings BEFORE settlement | LOST on process death; preserved on in-process panic/cancel (mutex snapshot is read at join) |
| adopted findings / contribution | DURABLE (EvidenceLedgerUpdated + SubAgentFinished.contribution) |
| ChildStatus 4-way | DEGRADED at the boundary — schema has only ok:bool + summary prose; cancelled/crashed/never-reported are all ok:false, distinguishable only by prose |
| per-child spend (rounds/tokens/cost) | PROCESS_LOCAL; only the SUM survives via absorb_child_spend |
| child StepLimits granted | PROCESS_LOCAL (recomputed residuals) |
| total-children cap counter | PROCESS_LOCAL **and per-drive** — resets every turn/window, not just on restart (audit §26 confirmed, worse than feared); the durable SubAgentStarted count exists but is never read |
| concurrency semaphore | per-drive |
| write-scope lease | PROCESS_LOCAL (Mutex<HashMap>, one per Executor, rebuilt every turn); grant/deny FACTS durable as free-text DelegationStage; NO release event |
| outstanding_children | DURABLE (ProgressUpdated), background children only, cleared unconditionally at next depth-0 drive |
| settlement notice (full result text) | DURABLE (parent transcript) |

## 3. What "continue child" means (frozen definition)

Chosen semantics = **C: restore the same ChildSession from durable
context + delta**, concretely:

    same durable child_id
    + same objective (SubAgentStarted.task, verbatim)
    + same role/profile contract as recorded at spawn
    + restored durable child context (requires new persistence — see §7)
    + durable delta (its own prior tool-call facts from the parent log)
    + structured recovery note
    + fresh Activation (new process resources, fresh lease state, fresh
      budget grant policy per §12)

Explicitly rejected: A (fresh child, same task — that is today's
re-delegation, not continuation), D (replay all messages verbatim as
execution), E (re-run prior tool calls — NEVER; see §9 invariant).

## 4. Crash matrix (current behavior → resumability classification)

| # | Window | Durable state today | Classification for a future resume |
|---|---|---|---|
| C1 | after spawn persisted | SubAgentStarted (maybe — spawn emit is queued, not flushed; a crash in the gap can lose even the Started) | SAFE_TO_RESUME once Started is durable; the unflushed-spawn gap is a Phase-1 fix (flush at spawn, as the reviewer path already does) |
| C2 | before first model request | Started + nothing else | SAFE_TO_RESUME (fresh context from task text) |
| C3 | during model stream, pre-tool | same as C2 (child transcript not persisted) | SAFE_TO_RESUME (stream loss is free) |
| C4 | ToolCallStarted persisted, before execution | dangling (call_id, agent_id) pair | SAFE_AFTER_REVALIDATION — existing recover_crash_window: safe+replay-free tools replay through the single reconcile funnel; mutating → RecoveryConfirmationRequired |
| C5 | during tool execution | same dangling pair; side effect possibly partial | MUST_NOT_REPLAY (mutating) → existing acknowledge_crash_window flow; resumed child receives OutcomeUnknown truth, must inspect |
| C6 | after side effect, before ToolCallFinished | dangling pair, effect real | MUST_NOT_REPLAY; same as C5 — never convert "uncertain" into "did not happen" |
| C7 | after finding reported | **LOST today** (finding only in child's in-memory ledger) | NEEDS_RECONCILIATION → Phase-1 persistence gap: findings must reach durability before settlement for continuation to preserve them |
| C8 | after partial contribution | LOST today (contribution computed at settlement only) | NEEDS_RECONCILIATION (same fix as C7 — contribution is derived from findings, so persisting findings incrementally suffices) |
| C9 | ChildResult produced, before SubAgentFinished persists | adopted findings may already be durable (ELU precedes SAF); ghost reconciler later writes synthetic ok:false contribution:None **contradicting the adopted findings** | NEEDS_RECONCILIATION — known current inconsistency, independent of continuability |
| C10 | SubAgentFinished persisted, notice not appended | findings durable + seeded on resume; the PROSE result is lost (nothing re-delivers; event summary is never read back into parent context) | ALREADY_SETTLED (child), parent-side delivery gap — settlement re-delivery from the event is a candidate hardening |
| C11 | notice appended, parent not yet consumed | notice is a transcript row; resume loads it | ALREADY_SETTLED — covered today (必改④) |
| C12 | child holds write scope at death | lease evaporates with the process (fail-closed); durable trace = ownership_granted free-text with no release; dangling mutating calls block resume via RecoveryConfirmationRequired | SAFE_AFTER_REVALIDATION — resumed child MUST re-claim; stale-authority resurrection is structurally impossible today and must stay so |

A fourth found window (not in the requested list): SubAgentFinished
persisted but ProgressUpdated (cleared outstanding) not — the lost-children
note then tells the model a successfully settled child "did NOT survive"
(false statement). NEEDS_RECONCILIATION; pre-existing.

No window classifies UNSAFE / DESIGN_BLOCKER.

## 5. Ownership / single-activation analysis

Existing primitives: per-task (== per-session today) owner_runtime_id +
owner_epoch CAS; OwnershipToken threaded into the child executor via the
shared ExecutionFence; every authoritative store write fenced; foreign
runtime → OwnershipConflict, never stolen; restart bumps the epoch.

Consequences for single-activation of a ChildSession:

- CROSS-RUNTIME double activation is already impossible: activating a
  child means running inside its parent session's turn machinery, which
  requires the task's current token. Two runtimes cannot both hold it.
- WITHIN one runtime, no per-child primitive exists — but none is needed
  as a *storage* fence: activation happens only through the parent
  session's engine paths, which are already serialized per session by the
  active-turns admission (one live turn per session). The needed rule is a
  process-local one: one Activation per child_id per drive, derived from
  the drive's own bookkeeping + a durable "activation claimed" fact if
  discovery/resume ever becomes cross-command.
- Verdict: single-activation is ENFORCEABLE with the existing task
  CAS/epoch + per-session turn admission; a per-child ownership table is
  NOT required. Double-activation prevention = (task token) ∧ (session's
  single live turn) ∧ (drive-level child registry).

## 6. Write-scope analysis (restart target semantics — decided)

Today: lease is per-Executor in-memory; process death (and even a turn
boundary) releases everything; grants/denials persist as free-text
DelegationStage with no release event; nothing machine-reads them back.
The lost-children note's "their exclusive scopes are released" is TRUE by
construction, not by action.

Frozen target for continuation (matches the spec's strong default):

    ChildSession survives; ChildActivation dies; the active write lease
    dies with the activation; a resumed Worker MUST re-claim via
    claim_write_scope and MAY be denied (denial nonterminal, as today).
    Historical claims never block a new claimant; stale authority is
    never resurrected.

This requires zero changes to the ownership registry. It requires the
resume context to SAY it (recovery note §10): "your previous scope is not
held; re-claim before mutating." The existing zero-authority fail-closed
default (effective_write_allowlist → Some(vec![])) already enforces it
mechanically for a fresh activation.

## 7. Context continuity analysis (the real gap)

Execution continuity for a child requires, minimally, what Main already
has and children do not:

| Needed | Exists today? |
|---|---|
| child objective | YES (SubAgentStarted.task, full) |
| child persona/system prompt | reconstructable by re-composition (persona files re-read; config drift possible — same accepted drift class as Main, HCH-DEBT-6) |
| child conversation so far | **NO** — sink is a no-op; the single largest gap |
| child context snapshot | **NO** — dropped by the capture closure |
| child tool-call facts | YES (parent log, (call_id, agent_id) pairs, with previews ≤1200 chars) |
| child findings so far | NO before settlement (in-memory only) |
| child budget spent | NO |
| workspace/git truth | YES (re-read live, as Main does) |

Two viable context sources for V1, in preference order:
(a) persist a child ContextSnapshot-equivalent at the child's own fold
    boundaries (the child drive already emits AgentEvent::ContextSnapshot
    — today it dies in the `_ => {}` arm; routing it to the parent's
    durable log with agent attribution is a small, additive change), and
(b) reconstruct approximately from SubAgentStarted.task + the child's
    durable tool-call pairs + previews — lossy but honest, suitable as a
    fallback exactly like Main's pre-P3 path.
GoalCheckpoint is NOT reused as-is (a child is not a Goal; no nested goal
lifecycle) — but the P3 checkpoint BUILDER pattern (structured facts
first, bounded, versioned, Unknown≠success) is the template for a child
checkpoint payload if (a) proves insufficient. Prefer (a): it is
ContextSnapshot reuse, not a new subsystem.

## 8. Findings / contribution / settlement analysis

- Findings before process death: LOST (handed over only at settlement).
  For continuation this must change: the child's EvidenceLedgerUpdated
  (already emitted by the child loop) must stop dying in the capture
  closure and reach the durable log with agent attribution. Adoption
  stays at settlement; what becomes durable earlier is the child-side
  ledger state, which a resumed activation re-seeds from.
- Adoption is NOT idempotent (adopt_finding = monotone append, no natural
  key; (child_id, child_finding_id) is the viable key but child ids are
  discarded at adoption). A resumable child REQUIRES adoption dedupe.
- SubAgentFinished has NO first-terminal-wins rule and no schema dedupe
  (contrast goal_checkpoints' unique index — the codebase's own idiom).
  Today safe only because a child cannot outlive its turn; a resumable
  child breaks that assumption, so Phase 1 must add terminal
  idempotency (schema-level: at most one Finished per child id, first
  wins; a later real finish after a synthetic one must be refused or
  reconciled explicitly).
- Settlement delivery: the notice is durable at append (必改④), but there
  is no consumption protocol and no re-delivery for C10. Exactly-once =
  (child_id) keyed: one Finished (schema), one notice (derived from the
  Finished append itself rather than a second bookkeeping path).

## 9. Side-effect safety invariant (frozen)

    Resuming a ChildSession NEVER replays an already-issued unsafe tool
    mutation merely because its model context is restored.

Already structurally supported: child tool calls are durable-before-effect
with agent attribution; dangling mutating calls block resume via
RecoveryConfirmationRequired; the acknowledge path writes OutcomeUnknown
truth, never fake success; replay is funnelled through one auditable entry
gated on replay_is_side_effect_free. Continuation with the SAME child_id
makes the existing (call_id, agent_id) pairing work FOR us: the resumed
activation can see its own dangling calls; a fresh UUID (today's
re-delegation) never can. This is the strongest technical argument for
identity continuity over re-spawn.

OutcomeUnknown discipline (§40) carries over: the recovery note must state
"prior mutations may exist; inspect, do not assume absent."

## 10. Recovery semantics (design)

Discovery (P2-analogous, discover ≠ execute):
- `unfinished_children()` already exists (Started without Finished). A
  read-only `list_unfinished_children(session)` needs only that plus the
  liveness qualifier "no live activation" (derivable: no live turn for the
  session, or the current drive's registry lacks the id). No mutation on
  discovery; no automatic resume at daemon startup.
- Reaper evolution: on restart, DO NOT synthesize a Finished for children
  of sessions whose goal still owes work — that is precisely the fact
  "resumable child" needs to remain visible. The ghost reconciler's
  synthetic finish becomes the CANCEL/ABANDON path, invoked when the
  parent settles around the child or policy abandons it, not a startup
  side effect. (Today it runs at next-turn-end regardless; V1 changes its
  trigger, not its shape.)

Who decides to resume: discovery automatic; execution explicit. Order of
preference matched to existing product semantics: the PARENT MODEL decides
during its resumed turn (it already receives the note listing unfinished
children and already owns re-delegation); the user can force via a future
command. No supervisor auto-resume in V1 (unattended goals continue to
re-delegate — today's behavior — until evidence shows auto-resume is
safe and valuable).

Resume context assembly (per §60):

    child system/persona (recomposed)
    + objective (SubAgentStarted.task verbatim)
    + latest durable child context snapshot (if any; else task-only)
    + durable delta: its own tool-call facts after the snapshot cursor
    + structured recovery note (§11 below)
    + current workspace/git truth (re-read)

Recovery note (structured, host-authored): interruption occurred; prior
side effects may exist (list dangling/unknown-outcome calls); write lease
NOT held — re-claim before mutating; findings already durable: {ids};
remaining objective. No model-generated apology, no raw CoT.

## 11. Parent interaction / caps / completion (decided defaults)

- Parent may keep working without resuming the child (today's semantics).
- A suspended child counts toward the TOTAL identity quota (durable count
  of SubAgentStarted per goal-epoch — fixing the per-drive reset bug is a
  prerequisite) but NOT toward live concurrency.
- Scope overlap: a suspended child blocks nothing (its lease is gone); if
  it resumes it must re-claim and may be denied — deny is nonterminal.
- Completion: a session with an unfinished (suspended) child must not
  reach Verified silently. Today's gate only sees the in-memory vec —
  restart makes ghosts invisible to it (verified gap). V1 rule: the
  completion gate consults durable unfinished_children; settling around a
  suspended child requires the explicit abandon path (synthetic finish +
  Worker-debt blocking finding where the role warrants it), recording the
  truth rather than forgetting it.
- Parent cancel / goal cancelled: suspended children are settled
  (cancelled, terminal) — they cannot outlive their goal. Cancel is
  terminal; activation loss is resumable. These two must get distinct
  durable terminal reasons (today all collapse to ok:false + prose).

## 12. Budget semantics (decided)

Per-child spent budget is not durable and V1 does NOT need cross-restart
budget carryover to be safe: a resumed activation receives fresh residual
limits computed from the PARENT's current durable epoch spend (which
already absorbed the child's pre-death spend at… no — absorption happens
at settlement only, so pre-death child spend is genuinely lost from
accounting). Accepted V1 simplification: resumed child gets fresh
residuals; the lost pre-death spend is an accounting gap, bounded by the
child wall cap (20 min) and the parent's own epoch budget. Recorded as a
known limitation, not a blocker. (Durable per-child spend would require
persisting child progress — same channel as the context snapshot if later
needed.)

## 13. Role rollout

- Explorer: structurally read-only, no lease, findings-only output —
  the safe first target; exercises the whole mechanism (identity,
  context persistence, single activation, idempotent settlement) with no
  mutation windows. V1.
- Worker: same mechanism + scope re-claim + C5/C6 reconciliation via the
  existing crash-window flow. V1.1 (second step inside the same train),
  because full closure without Worker is not closure (§67) — but landing
  the mechanism on Explorer first reduces risk without forking the
  architecture by role.
- Reviewer: rerun-from-fresh is cheaper and safer (20-round bound,
  stateless, duplicate-finding risk on partial resume). REVIEWER_CONTINUATION=DEFER.

## 14. Strategy comparison

| | A: existing child_id + activation + minimal persistence | B: child_sessions projection table | C: dedicated child Session abstraction |
|---|---|---|---|
| identity | reuse UUID child_id (already canonical everywhere) | same id + new table | new SessionId per child |
| new persistence | child context snapshots (attributed, into existing event log) + Finished dedupe index + (later) child ledger events | table + the same event needs anyway | sessions/turns rows per child — heavy |
| second-runtime risk | none | low | HIGH (child turns, child reaper, child snapshots = parallel machinery) |
| migration | additive events + one index | additive + table | schema surgery |
| discovery | existing unfinished_children() | table query | session queries |
| SubAgentProvider future | id + capability fields extend naturally | fine | over-committed |
| verdict | **CHOSEN** | fallback if projections get hot | rejected (violates no-second-runtime) |

## 15. Storage / event impact (design only, NOT implemented)

- No new table required for V1. One new INDEX (unique, partial:
  sub_agent_finished dedupe by child id) OR an application-level
  first-terminal-wins guard in the one writer choke point — decide at
  implementation with the events-table idiom in mind (0019 precedent
  argues schema-level).
- Event families: NO SubAgentActivationStarted/Lost/Resumed family. The
  needed facts: activation start ≈ existing turn machinery + a
  DelegationStage-style action ("child_resumed: {id}") suffices for
  audit; activation loss is derivable (Started, no Finished, no live
  turn). ONE genuinely new persistence path: the child's ContextSnapshot
  (and later EvidenceLedgerUpdated) routed durably with agent
  attribution — this is attribute-and-persist of an EXISTING event, not a
  new family. If implementation shows attribution needs a field,
  `agent_id: Option<String>` on ContextSnapshot is additive
  (serde-default), mirroring ToolCallStarted's precedent.
- Bounded cost note: child snapshots inherit the same O(rounds×context)
  concern as HCH-SIMP-7; child contexts are much smaller (residual-budget
  bounded) and snapshots can be emitted at fold boundaries only rather
  than per round. Set that policy at implementation.

## 16. Eval + dogfood plan (design only)

Eval suite E1-E10 as specified (Explorer mid-analysis kill; Worker kill
before claim / after claim pre-mutation / post-mutation / post-finding /
partial contribution; two-children kill-resume-one; double-activation
race; stale scope reclaim; parent completion with suspended child).
Metrics: identity preserved, zero duplicate side effects, zero duplicate
settlement, zero lost finding (post-Phase-1 persistence), zero lost
contribution, ownership violations 0, completion-truth violations 0,
ghosts 0, stale leases 0.
Dogfood: real repo, natural Worker fan-out, precise daemon SIGKILL
mid-child, restart, discovery visible, explicit resume, same child_id
continues, settles once, parent consumes once. Reviewer: rerun-fresh only.

## 17. Fixes found that are INDEPENDENT of continuability

The audit surfaced pre-existing truthfulness gaps worth their own small
hardening ticket (not implemented here, candidates for prioritization):

1. Total-children cap is per-drive: resets every turn/window, not only on
   restart; the durable SubAgentStarted count is never consulted (§26
   bug confirmed).
2. Completion gate blind to durable ghosts: after restart the
   outstanding-children vec starts empty and is cleared before the gate
   can see the log; a session can reach Verified with a started-never-
   finished child, and a Worker ghost's debt finding is never planted
   (synthetic finishes carry contribution:None and skip the Worker-debt
   path).
3. C9/C10 inconsistencies: ghost synthetic finish can contradict
   already-adopted findings; a settled child's result prose is lost if
   the notice append never happened; the lost-children note can assert
   "work NOT done" about a child whose Finished is durable (the fourth
   window).
4. Spawn's SubAgentStarted is queued, not flushed (reviewer path already
   does it right).
5. UnfinishedChild.turn_id dead field / ghost finish mis-attribution;
   reviewer events carry turn_id: None.

## 18. Beta classification rationale

Current behavior is safe, truthful, fail-closed: leases die with the
process (no stale authority), mutation uncertainty blocks resume behind
explicit human acknowledgement, lost children are reported honestly and
re-delegation works, Main Goal continuity (P3) preserves the parent's
work. The Beta promise — long tasks survive interruption with truthful
state — is met by Main-level continuity plus honest child loss. Child
loss costs re-exploration, mitigated partially (findings survive
in-process aborts; the settlement notice survives everything after
settlement). Delegation adoption is measured stochastic and child counts
per goal are low, so the real-world exposure of mid-child process death
is structurally small (frequency itself UNMEASURED). Therefore:
POST_BETA. The independent truthfulness gaps in §17 are the only items
worth considering for pre-Beta hardening, and they are small.
