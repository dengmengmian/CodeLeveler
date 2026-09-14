# Multi-Agent MA0 — Current Reality & Gap Audit

Status: **audit only**. No product code changed.

```text
MA_BASE_HEAD=86bc214fb4f1f83aa148976443e1953d397498dd
FROZEN_SINGLE_AGENT_TAG=v0.2.0-beta.2
FROZEN_SINGLE_AGENT_HEAD=86bc214fb4f1f83aa148976443e1953d397498dd
AUDIT_DATE=2026-09-14
```

HEAD == origin/main == `v0.2.0-beta.2`; worktree clean at start.
File:line references are valid at this HEAD and drift with edits.

---

## 1. Method

- Full-tree search (`spawn_agent|sub.agent|ChildProfile|AgentId|settlement|
  ownership|lost_children|settled_children|reviewer|…`) over `crates`, `apps`,
  `services`, `evals`, `docs`.
- Code read of the spawn → admission → execution → settlement → restart chain
  (`leveler-agent/src/executor/drive.rs`, `executor/handlers.rs`, `executor.rs`,
  `sub_agent.rs`, `child_profile.rs`, `named_agent.rs`, `ownership.rs`,
  `coding/turn.rs`, `coding/run.rs`; `leveler-engine/src/turn.rs`, `log.rs`,
  `reaper.rs`, `event.rs`).
- Client surfaces: `leveler-client-protocol`, `leveler-app/src/event_bridge.rs`,
  `leveler-tui`, `leveler-web`, `leveler-cli`, `apps/leveler-mobile`,
  `leveler-remote-*`, `services/leveler-relay`.
- Eval: `crates/leveler-eval`, `leveler-cli/src/eval_cmd.rs`, `evals/`.
- History: multi-agent docs deleted in `e2e8938` / `d8fdc9b`, read with
  `git show d8fdc9b^:<path>`.
- Existing tests executed at this HEAD (`env -u NODE_OPTIONS`):

| Suite | Result |
|---|---|
| `leveler-agent --test child_profile_spawn` | 9 passed |
| `leveler-agent --test ma_restart_truth_test` | 8 passed |
| `leveler-agent --test multi_agent_test` | 71 passed |
| `leveler-agent --test spawn_reliability_gate` | 8 passed |
| `leveler-agent --lib sub_agent child_profile ownership` | 36 passed |
| `leveler-engine --test minimal_harness` | 5 passed |

Where a conclusion rests on code order rather than a test, the row says
**code-derived**. Those are the rows MA1 must pin with a red test first.

---

## 2. History the roadmap did not account for

Multi-Agent is not a prototype. Prior programmes, all on `main`:

| When | Commit | What it established |
|---|---|---|
| 08-17 | `0e25c2a` | First "multi-agent product closure" (profiles, findings lifecycle) |
| 08-18 | `ba70695` | V2 background-first delegation, settlement, parent write fence |
| 08-22 | `18d77c6` | **MA-WA1**: ~500 runs; delegation adoption 10–30 %, no CodeLeveler lever moves it; `BETA_DECISION=ACCEPT` opportunity-based delegation; forced delegation excluded |
| 08-23 | `4d70bd3` | Spawn reliability gate PASS: 0 lost children, caps 4 concurrent / 6 total |
| 08-24 | `ac1352b` | **MA-VALUE-A**: Explorer fan-out on a broad audit, n=10 pairs, 68 %→79 % (+2.8/26, 95 % CI [+1.05,+4.55]) at **2.5× wall**; 63/63 children settled |
| 08-25 | `0371144` | Child contribution projection + TUI contribution-first rendering |
| 08-27 | `5d546f9` | **Continuable SubAgent audit**: resume is feasible (existing child id + new activation) but needs child context persistence, adoption dedupe, terminal idempotency; classified POST_BETA |
| 08-28 | `2c3863d` | **MA-RT-1..4**: spawn flushed before child runs, durable total cap, ghost terminal at next turn, first-terminal-wins, settled-child re-delivery |
| 08-31 | `c9d20f9` | TUI Activity drill-down for child agents |
| 09-09 | `7cbccf6` | Restart-owes-a-lost-child test aligned with TaskOutcome/Verification split |
| 09-11 | `567d6c3` | Eval stops reporting per-child verification nobody measured |

Consequences for this programme:

1. Delegation **adoption** is a measured model property. MA2/MA4 must not try to
   raise it (standing constraint from MA-WA1).
2. Multi-Agent **value** is proven only for broad read-only exploration, at a
   2.5× wall cost. It is unproven for coding and for the Reviewer.
3. Child **resume** was designed once (08-27) and deliberately deferred. The
   restart truth that ships today is "honest lost", not "resumed".

---

## 3. Structural facts that bound every later stage

| Fact | Evidence |
|---|---|
| A child is an in-process tokio task, not a session | `drive.rs:61-69, 2509`; `SubAgentProgressSink::append` is a no-op (`executor.rs:708-711`) |
| A background child cannot outlive its parent **turn**: every normal exit drains | `drive.rs:548-565` (`drain_background_children`) |
| So "running child across restart" only happens on **process death mid-turn** | follows from the two rows above |
| Topology is a star; depth 1 is a product decision, not a prototype limit | `child_profile.rs:13-15`; enforced `drive.rs:210-214, 2290-2291`, `executor.rs:1322-1323` |
| Engine owns the ghost terminal; harness owns the lost-child voice and notes | `leveler-engine/src/turn.rs:379-395, 650-694`; `coding/turn.rs:332-415`; `minimal_harness.rs:389` |
| Delegation strategy is model-owned; the hint states mechanics only | `sub_agent.rs:23-37`, test `the_steer_hint_states_mechanics_not_strategy` |

`ENGINE_OWNS_MULTI_AGENT_LIFECYCLE=YES`, `ENGINE_OWNS_MULTI_AGENT_STRATEGY=NO`
already hold at this HEAD.

---

## 4. Capability matrix

Legend: Y = yes, P = partial, N = no, — = not applicable.
Classification: `ALREADY_PRODUCTIZED | PARTIAL | REAL_GAP | NOT_REQUIRED | DEFERRED`.

| # | Capability | Exists | Durable | Restart-safe | Productized | Tested | UX | Class | Evidence / gap |
|---|---|---|---|---|---|---|---|---|---|
| 1 | spawn child | Y | Y | Y | Y | Y | P | ALREADY_PRODUCTIZED | `drive.rs:2164-2556`; admission, caps, overlap refusal |
| 2 | durable child identity | Y | Y | Y | Y | Y | — | ALREADY_PRODUCTIZED | UUIDv4 `sub_agent.rs:159`; keyed on events, `model_requests.agent_id`, tool calls |
| 3 | parent-child relation | Y | P | P | P | Y | — | PARTIAL | Edge = `sub_agent_started` row (session+turn), flushed before the child runs (`drive.rs:2447-2451`, test `spawn_reliability_gate.rs:641,717`). No dedicated edge table (none needed). Gap **G4a** below |
| 4 | child session (transcript/context) | N | N | N | N | — | — | DEFERRED (pending decision D1) | sink no-op; child messages never stored |
| 5 | background execution | Y | P | P | Y | Y | P | ALREADY_PRODUCTIZED (runtime) | default `run_in_background=true` (`injected_tools.rs:222-227`) |
| 6 | foreground wait | Y | Y | — | Y | Y | P | ALREADY_PRODUCTIZED | `run_in_background=false`; result in the tool result. No `wait_agent` tool exists and none is needed |
| 7 | concurrent spawn | Y | — | — | Y | Y | P | ALREADY_PRODUCTIZED | same-message spawns via `FuturesUnordered`, semaphore 4 (`multi_agent_test.rs:225`) |
| 8 | child write scope | Y | N (lease in memory) | Y (fail-closed) | Y | Y | N | ALREADY_PRODUCTIZED (runtime); UX gap | `ownership.rs`; scope only inside task text on the wire |
| 9 | parent write fence | Y | — | Y | Y | Y | — | ALREADY_PRODUCTIZED | `multi_agent_test.rs:3996, 5422`; `ownership.rs:479,614,638` |
| 10 | cancellation | P | P | — | P | P | N | REAL_GAP | Parent/turn cancel cascades via child tokens (`drive.rs:2471`; tests `multi_agent_test.rs:4265`, `spawn_reliability_gate.rs:441`). **G5** abort path releases scope before the child stops; **G6** no release-on-cancel test; **G8** no per-child cancel primitive |
| 11 | child timeout / budget | Y | — | — | Y | Y | P | ALREADY_PRODUCTIZED | 20 min wall, residual minus 60 s reserve (`handlers.rs:388-402`), budget split (`drive.rs:3160-3226`) |
| 12 | child settlement | Y | Y | Y | P | Y | P | PARTIAL | Four-way `ChildStatus` exists in memory but **G2**: persisted/wire terminal is `ok: bool` + prose |
| 13 | partial result | Y | P | — | Y | Y | P | PARTIAL | `IncompletePartial` carries findings to the parent model; not typed on the wire (G2) |
| 14 | no-result distinction | Y | P | — | Y | Y | N | PARTIAL | `for_parent` distinguishes; clients cannot (G2) |
| 15 | settlement re-delivery | Y | Y | Y | Y | Y | — | ALREADY_PRODUCTIZED | `ma_restart_truth_test.rs:417`; note: at-least-once + consumed mark (`drive.rs:256-261`) — acceptable, repeats a note, never state |
| 16 | running-child restart | P | P | P | P | Y | N | REAL_GAP | Child dies; ghost terminal written only **at the next turn start** of that session (`turn.rs:379-395`). **G1** `reap_after_restart` does not close children (`reaper.rs:101-150`) → open edge until the session is resumed, forever if never |
| 17 | child resume | N | N | N | N | — | — | DEFERRED (decision D1) | design exists: deleted `docs/design/CONTINUABLE_SUBAGENT_AUDIT.md` |
| 18 | repeated restart | Y | Y | Y | Y | Y | — | ALREADY_PRODUCTIZED | reconciliation idempotent (`ma_restart_truth_test.rs:582`); cap across restart (`:771`, `spawn_reliability_gate.rs:800`) |
| 19 | orphan detection | P | Y | P | P | Y | N | PARTIAL | `child_reconciliation_view` (`log.rs:216-270`); observability `collect_agents` shows a lost child as `running` (`leveler-app/src/observability.rs:635`) — G1 |
| 20 | provider abstraction | Y | — | — | Y | P | — | NOT_REQUIRED (new abstraction) | `ProviderRegistry` routes by `ModelRef.provider` (`leveler-provider/src/registry.rs:186`); child shares runtime |
| 21 | child model selection | Y | — | — | P | P | N | PARTIAL | named agent `model:` (`named_agent.rs:52-55`, `drive.rs:2250-2297`). **G7a** child billed with the parent's pricing (`executor.rs:1301`); **G9b** model only syntax-checked at admission |
| 22 | capability profiles | Y | Y | — | Y | Y | P | ALREADY_PRODUCTIZED | Default/Explorer/Worker/Reviewer, structural registry subsets (`child_profile.rs:178-183`) |
| 23 | capability admission | Y | Y | — | Y | Y | — | ALREADY_PRODUCTIZED | `ChildProfile::admit_spawn`; reviewer/unknown profile/conflict refused (`child_profile_spawn.rs`) |
| 24 | capability negotiation | P | — | — | P | P | — | PARTIAL | **G9a** unknown `role` string silently becomes Default (a writer) (`child_profile.rs:34-40`); named agent `role: reviewer` silently becomes Default; G9b provider/model not checked at admission |
| 25 | role contract | Y | — | — | Y | Y | — | ALREADY_PRODUCTIZED | `validate()` + tests `the_builtins_are_the_documented_bounds` |
| 26 | reviewer | Y | Y | Y | Y | Y | P | ALREADY_PRODUCTIZED (runtime) | harness-only, 20-round bound, default `Off`; value unproven (pilot `insufficient_n`). Reviewer usage buffered until finish (`run.rs:1696-1747`) — G7b |
| 27 | child nesting | N | — | — | — | Y | — | NOT_REQUIRED | depth 1 = star topology by design; Residual Policy excludes graph |
| 28 | background UX | P | — | — | P | P | P | REAL_GAP | no `background` field anywhere on the wire; see §6 |
| 29 | parent timeline | P | — | — | P | P | P | REAL_GAP | settlement / lost / re-delivery notices stored as `User` messages, rendered as user input after reload (§6 U3) |
| 30 | child transcript access | N | N | — | N | — | N | DEFERRED (D1) | no child transcript exists to show; TUI detail shows ≤16 steps + result |
| 31 | token/cost accounting | Y | Y | P | P | Y | P | PARTIAL | per-child rows `model_requests.agent_id` + `reconcile_session` (`model_request_repo.rs:300-340`); G7a pricing, G7b reviewer buffered, G7c queued child records lost on crash |
| 32 | mobile visibility | P | — | N | P | N | P | REAL_GAP | §6; snapshot/`LiveSessionView` carries no children |
| 33 | push notification | N | N | N | N | N | N | REAL_GAP (greenfield, decision D2) | no APNs/FCM/web-push anywhere incl. `services/leveler-relay`; `RuntimeEvent::Notification` has no id |

---

## 5. Specific verifications (MA0.4 – MA0.8)

### 5.1 `MAX_SUB_AGENT_DEPTH=1`

Intentional product limit. Star topology is stated at the role definition,
triple-enforced, and no comment calls it provisional. `NOT_REQUIRED` to change.

### 5.2 Parent restart while a background child runs (mechanical)

| Question | Answer | Proof |
|---|---|---|
| Does the child die? | Yes. No code relaunches it | in-process task; `turn.rs:628-631` |
| Does the durable relationship remain? | Yes: `sub_agent_started` row | `ma_restart_truth_test.rs:701` (file DB reopen) |
| When is it closed? | **Only when the next turn of that session starts** | `turn.rs:379-395`; `reaper.rs:101-150` reaps turns only |
| Is ownership released? | Yes, but only because the lease was in memory; no release fact | `ownership.rs:68-86`, built per `Executor::new` |
| What does the parent model see? | "Delegations lost at restart" (from `outstanding_children`) or "re-delivered" for durably finished children | `drive.rs:262-289`; `multi_agent_test.rs:4205`; `ma_restart_truth_test.rs:417` |
| Terminal truth written | `SubAgentFinished{ok:false, summary:"…was lost…"}` on the origin turn, once, no invented findings | `ma_restart_truth_test.rs:330, 582, 701`; `minimal_harness.rs:389` |

The source comment "running background children do not survive restart" is
accurate. What is **not** true today: `NO_OPEN_ORPHAN_AFTER_RESTART`. Between
restart and the next turn — indefinitely for a session nobody resumes — the log
and every client show the child as running (G1).

### 5.3 Capability — what is actually missing

| Option | Verdict |
|---|---|
| A. negotiation result persistence | Already persisted: `SubAgentStarted{profile_id, profile_role, read_only}` |
| B. provider/model compatibility | **REAL_GAP (G9b)**: pinned model only parsed; unknown provider/unconfigured credentials fail at first request as a child failure instead of an admission denial |
| C. runtime capability discovery | NOT_REQUIRED: profiles are static built-ins; tools advertised per registry subset |
| D. UX presentation | REAL_GAP, folded into MA3 (profile / read_only dropped by web and mobile) |
| E. dynamic profile | NOT_REQUIRED: named agents already cover persona + tools + rounds + model |
| silent mapping | **REAL_GAP (G9a)**: `AgentRole::parse` maps unknown → Default |

### 5.4 SubAgentProvider

`SUB_AGENT_PROVIDER_NEW_ABSTRACTION_REQUIRED=NO`. Child creation is not coupled
to a concrete provider: the child executor shares `runtime`, and requests route
through `ProviderRegistry` by `ModelRef`. Named agents already select a
different `provider/model`. The only real defects are pricing (G7a) and
admission-time model validation (G9b).

### 5.5 Eval baseline

Existing, reusable: Rust `leveler eval run|compare|ablate|trend`
(capability cases, `expect` verifier, false-completion rate, tokens, cost) and
Python `evals/lib` (event-log scorer: task success, verification truth,
delegation offer/spawn/adoption with Wilson CI, per-profile value, reviewer
value, wall, turns). Arms are config keys (`agents.delegation`,
`agents.independent_review`). Missing for MA4:

| ID | Gap | Evidence |
|---|---|---|
| E1 | no per-child success / partial / no-result / lost grouping | `evals/lib/spawn_metric.py:95-104` stores raw outcome only |
| E2 | `safety.violations` hardcoded 0; no ownership-violation, orphan-open-edge or duplicate-settlement counter from the log | `evals/lib/eventlog.py:231-232`, `schema.py:194` |
| E3 | cached tokens, request count, cost not read on the Python path | `eventlog.py:76-89` |
| E4 | no first-class two-build A/B; `compare_value_arms` has no CLI caller | `evals/lib/value.py:160`; manual precedent `evals/baselines/repository-map-ab-3767bb6/` |
| E5 | multi-agent value tasks R005–R010 are pointers to a private repo; `MA-VALUE-001 --execute` refuses | `evals/runner/run.py:299-305` |
| E6 | useful-delegation is heuristic (`child_result_used` = parent acted after finish) | `value.py:39-100` |

No second eval framework is needed; E1–E6 extend the existing one.

---

## 6. UX reality

### 6.1 Wire

Engine → client bridge `leveler-app/src/event_bridge.rs:527-614`. Client events:
`SubAgentUpdated{id,nickname,role,done,ok,detail,profile_id?,profile_role?,
read_only,contribution?}`, `SubAgentProgress{…tokens}`, `SubAgentActivity`,
`ChildContributionLoaded`; observability `UiAgentObservation{status:
running|ok|fail}`, `UiLaneAccounting{main|children|total}`.
`protocol.gen.ts` is **not** stale for these types.

Not on the wire: four-way status, terminal reason (cancelled / timed out /
budget / lost), background flag, write scope, per-child elapsed, children in the
session snapshot or reconnect `LiveSessionView` (`live_view.rs:19-25`).

### 6.2 Surface matrix

No desktop shell exists (no tauri/electron); "Desktop" = Web layout.

| Capability | TUI | Web | CLI | Mobile |
|---|---|---|---|---|
| started / nickname / role | Y | Y (Inspector) | Y | Y |
| running state | Y | P (`active` not rendered) | Y (by id) | P (overwritten by first activity) |
| profile / read-only | P (inspector) | N (dropped `controller.ts:275-284`) | jsonl only | N |
| write scope | N | N | N | N |
| background | N | N | N | N |
| four-way settlement | P (contribution states) | N | prose | N |
| cancelled / timed out / lost | N (timeout detector matches a prefix the summary never has, `transcript_lines.rs:1061-1064`) | N | N | N |
| child detail | Y (≤16 steps, result, findings) | N | `trace` table | N |
| cancel child control | N | N | N | N |
| per-child tokens | Y | stored, not rendered | Y | N (`sub_agent_progress` ignored) |
| per-child cost | N | N | children lane in `trace` | N |
| parent running/settled counts | Y | total only | total only | N |

### 6.3 Defects (code-derived; MA3 pins each with a render/state test first)

| ID | Defect | Evidence |
|---|---|---|
| U1 | Duplicate start rows. Mobile: 3 rows per child (tool start, tool result, sub-agent row). TUI: `spawn_agent` taxonomy visibility `Important` renders a tool cell next to the SubAgent block | `session_state.dart:265-283,520,603`; `tool_taxonomy.rs:377-382` |
| U2 | Settled collaboration stuck in the active area. TUI: idle clock resets to 0 so `now.saturating_sub(settled_at) < 6` stays true; turn fail/cancel does not finalize `state.team`; `team` not reset on session switch. Web: `turn_terminal` does not finalize agents. Mobile: turn end ignores sub-agent rows | `multi_agent.rs:165-177`, `run.rs:313-317`, `runtime_apply.rs:1065-1078`; `store.tsx:586,847-863`; `session_state.dart:414-427` |
| U3 | Settlement / lost / re-delivery notices are `Role::User` transcript rows and render as user input after reload on all clients | `drive.rs:262-289, 529-538`; `interactive.rs:3280-3298`; `presentationKind.ts:27`; `session_state.dart:507-511` |
| U4 | UI derives child state: TUI turn end rewrites Running→Failed (`transcript.rs:629`); per-child elapsed from the turn clock (`multi_agent.rs:1091`); web late `sub_agent_updated` re-opens a finished turn (`store.tsx:706,417-423`); web observability maps unknown status → running (`observabilityView.ts:149`) |
| U5 | Parent finished, child shown running: lost children (G1) and reconnect (no children in snapshot; web/mobile lose them, TUI keeps a stale team) |
| U6 | Background children never appear in the TUI wait line (only in-flight `spawn_agent` calls do) | `wait_status.rs:148-155` |

---

## 7. Runtime gap register (MA1 / MA2 inputs)

| ID | Gap | Class | Proof status |
|---|---|---|---|
| G1 | Restart leaves child edges open until that session's next turn; observability reports them `running` | REAL_GAP | code-derived (`reaper.rs`, `observability.rs:635`); tests cover only the next-turn path |
| G2 | Terminal is `ok: bool` + prose on the event, the wire and in eval; four-way status and terminal reason (completed / cancelled / timed_out / budget / lost / failed) not typed | REAL_GAP | `event.rs:380-391`, `drive.rs:3095-3101` |
| G3 | Canonical settlement is first-terminal-wins **on read** only; no uniqueness at write | PARTIAL | `log.rs:218-219,256-265`; no constraint in migrations |
| G4a | `ProgressUpdated(outstanding_children)` rides the async pump while the ack is a direct write: a crash can leave Started + ack durable with no outstanding entry → engine writes a ghost terminal but the parent model gets no lost note | REAL_GAP | code-derived (`drive.rs:2510-2517`, `recorders.rs:73-90`, `turn.rs:164-180`) |
| G4b | `SubAgentFinished` rides the pump while the settlement notice is a direct write: a crash between them → transcript says settled, next turn writes "lost" | REAL_GAP | code-derived (`drive.rs:514-537`) |
| G5 | Drop/abort path calls `release_all` right after `abort()`; an admitted editor commit inside `spawn_blocking` can land after the scope is released | REAL_GAP | code-derived (`drive.rs:85-94`, `leveler-tools/src/workspace/editor.rs:82-99`) |
| G6 | No test: scope released on cancel; cancelled child cannot write afterwards; scope re-claimable after crash | REAL_GAP (test) | absence verified in `multi_agent_test.rs`, `ownership.rs` tests |
| G7a | Child with a pinned model is billed at the parent model's pricing | REAL_GAP | `executor.rs:1301` |
| G7b | Reviewer model-request records buffered until the reviewer finishes; crash loses them | REAL_GAP | `coding/run.rs:1696-1747` |
| G7c | Child model-request records queued in the progress channel are lost on process death | PARTIAL | `drive.rs:472-484`; bounded by one drain interval |
| G8 | No per-child cancel primitive (only whole-turn cancel) | REAL_GAP (needed only if MA3 ships a cancel-child control) | `command.rs:81` |
| G9a | Unknown role string → Default (writer) silently; named agent `role: reviewer` → Default silently | REAL_GAP | `child_profile.rs:34-40`, `drive.rs:2236-2242` |
| G9b | Pinned model not checked against configured providers at admission | REAL_GAP | `drive.rs:2292-2297` |
| G10 | Durable child session + resume | DEFERRED → decision D1 | §8 |

---

## 8. Decisions that are the user's, not the audit's

### D1 — What does "restart-safe running child" mean?

| Option | Scope | Risk |
|---|---|---|
| **A. Honest terminal at restart** (recommended for this programme) | Close G1: the engine closes open child edges during `reap_after_restart` with a typed `lost` terminal on the origin turn; fix G4a/G4b ordering; typed status (G2). Resume = model re-delegates (today's proven path) | small, additive; satisfies `NO_OPEN_ORPHAN_AFTER_RESTART` and "never silently completed" |
| B. Durable child session + resume | A **plus**: persist child context snapshots with agent attribution, adoption dedupe keyed `(child_id, finding_id)`, schema-level terminal uniqueness, activation discovery, Worker crash-window reconciliation (C4–C6), resumed-child recovery note, UI for suspended children | a multi-week train; 08-27 audit put Explorer first, Worker second, Reviewer never |

Evidence against B now: background children cannot outlive their turn, so
exposure is process death mid-turn only; its frequency has never been measured;
63/63 children settled in MA-VALUE-A. The task prompt names B as the
"preferred target". A is recommended; B should be justified by a measured
mid-child crash rate first.

### D2 — Push

Nothing exists: no APNs/FCM client, no device token registration, no relay
delivery path, no durable event id. It needs an Apple Developer APNs key
(and/or a Firebase project), a sender (relay or host), and a privacy decision
about what a notification payload may carry (remote policy already denies
`QueryChildContribution` because findings carry paths). Credentials and the
payload policy are the user's to provide.

### D3 — MA4 real-provider budget and task set

MA-VALUE-A took ~20 min per treatment run. A two-arm batch of 10–20 runs per arm
across 7 task categories is a real spend. The existing multi-agent value tasks
(R005–R010) live in a private repository (E5).

---

## 9. Implementation scope generated from the gap matrix

Anything not listed below is regression-acceptance only.

### REAL_MA1_SCOPE (under D1 = A)

1. G1 — engine closes open child edges at restart (typed lost terminal, origin turn, once); observability stops reporting them running.
2. G2 — typed terminal on `SubAgentFinished` (`status` four-way + `terminal_reason`), serde-default for old logs; carried through the client protocol.
3. G3 — write-side single canonical terminal (choose unique index vs. single-writer guard; red test for a double write first).
4. G4a / G4b — make `outstanding_children` and `SubAgentFinished` durable before the ack / notice they justify; crash-window tests.
5. G5 / G6 — no write after scope release on abort/cancel; release-on-cancel and reclaim tests.
6. G7a / G7b / G7c — child pricing from its own model profile; reviewer records persisted as they occur; bound the crash loss window or record it as an accepted limit.
7. G8 — per-child cancel primitive, **only if** MA3 ships the control.

Under D1 = B, add G10 (the 08-27 design, Explorer first).

### REAL_MA2_SCOPE

1. G9a — unknown role / reviewer-via-named-agent → honest denial, never a silent Default.
2. G9b — pinned model admitted only if its provider is configured; otherwise denial.
3. Regression acceptance (real provider): background settlement, foreground dependency, parallel explorers, parallel disjoint workers, partial on timeout, cancel, reviewer bounded, zero-delegation on simple tasks.
4. NOT in scope: new roles, provider trait, runtime delegation heuristics, raising adoption.

### REAL_MA3_SCOPE

1. U3 — settlement / lost / re-delivery notices get a non-user presentation kind (runtime fact, not prose sniffing).
2. U2 / U4 / U5 — child state finalized from runtime facts only (typed terminal from MA1); no Running→Failed rewrite; elapsed from the child's own start/finish.
3. U1 — one entry per child (hide the `spawn_agent` tool cell when a SubAgent block exists; mobile de-dup).
4. U6 — background children visible in the TUI status/wait area.
5. Snapshot/reconnect carries children (needed by Web and by MA5).
6. Web: render `active`, profile/read-only, four-way status, partial, tokens; finalize on `turn_terminal`.
7. Cancel-child control only with G8.

### REAL_MA4_SCOPE

1. E1 / E2 / E3 — per-child terminal grouping, ownership-violation / orphan-edge / duplicate-settlement counters from the event log, cached tokens / requests / cost.
2. E4 — two-binary A/B driver over the existing runner (baseline `v0.2.0-beta.2` binary vs treatment).
3. E5 — an in-repo or CONTROL_ROOT task set covering SIMPLE, MULTI_FILE, PARALLELIZABLE_RESEARCH, PARALLELIZABLE_IMPLEMENTATION, REVIEW_HEAVY, LONG_GOAL, RECOVERY (depends on D3).
4. Real batch + `docs/MULTI_AGENT_EVAL_AND_ACCEPTANCE.md`.

### REAL_MA5_SCOPE

1. Mobile: children section from the durable snapshot (needs MA3 item 5), typed status, de-dup, progress tokens, child detail, cancel if G8.
2. Push: greenfield — durable event id, delivery record, dedupe, sender, device registration (depends on D2).

---

## 9a. Decisions recorded (2026-09-14, user)

| Decision | Choice | Effect on scope |
|---|---|---|
| D1 | **B — durable child session + resume** | REAL_MA1_SCOPE = items 1–7 **plus G10**. G1 changes shape: at restart an open child becomes a typed, durable *interrupted* (resumable) state tied to its parent task, not a synthetic lost terminal; a lost terminal remains only for children that cannot be resumed or whose parent is closed/cancelled |
| D2 | **Push deferred**; MA5 = Mobile only | REAL_MA5_SCOPE item 2 removed. Final gate records `PUSH=DEFERRED_BY_USER_DECISION`, not PASS |
| D3 | **In-repo public task set, `deepseek-v4-flash`** | REAL_MA4_SCOPE item 3 builds the seven categories under `evals/`; arms interleaved baseline `v0.2.0-beta.2` vs treatment |

## 10. Gate

```text
EXISTING_CAPABILITIES=spawn, durable id, star topology, background-first,
  foreground, concurrency caps, profiles, admission, write scope + parent fence,
  four-way child result (in memory), settlement re-delivery, honest lost-at-next-turn,
  per-child request rows, TUI child detail, reliability gate, MA-VALUE-A evidence
REAL_GAPS=G1 G2 G3 G4a G4b G5 G6 G7a G7b G9a G9b; U1–U6; E1–E5; Push; mobile children
PARTIAL=G7c, parent-child edge, orphan detection, negotiation, accounting
NOT_REQUIRED=new SubAgentProvider trait, nesting/graph, new roles, dynamic profiles,
  wait_agent tool, dedicated edge table
DEFERRED=child session + resume (G10) pending D1
SUB_AGENT_PROVIDER_NEW_ABSTRACTION_REQUIRED=NO
UNKNOWN=0
BLOCKING_USER_DECISIONS=D1 (resume vs honest terminal), D2 (push credentials/payload), D3 (eval budget/task set)

MA0_REALITY_AUDIT=PASS
```
