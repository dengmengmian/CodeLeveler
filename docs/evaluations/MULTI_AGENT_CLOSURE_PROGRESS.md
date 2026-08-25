# Multi-Agent Closure — progress

**Subject:** `v0.2.0-beta.1` · **Updated:** 2026-08-24

Where multi-agent productization stands, what is proven, and what is still
blocked. One rule throughout: a capability is not closed until something
measured it.

## State

| Item | State | Evidence |
| --- | --- | --- |
| Runtime | ✅ | Beta capability closure |
| Event pipeline | ✅ | Beta capability closure |
| Spawn reliability | ✅ | `spawn_reliability_gate.rs` |
| Explorer value | ✅ | [MA-VALUE-A-FINAL](MA-VALUE-A-FINAL.md) — +16 % relative, 2.5× time |
| Structured child result | ✅ | `ChildResultProjection` |
| Child profile | ✅ | [CHILD_PROFILE](../design/CHILD_PROFILE.md) |
| Reviewer eval framework | ✅ | [MA-VALUE-REVIEWER-PILOT](MA-VALUE-REVIEWER-PILOT.md) |
| Reviewer pilot | ⚠️ halted | [MA-VALUE-REVIEWER-FINAL](MA-VALUE-REVIEWER-FINAL.md) |
| **Reviewer contribution trace** | ✅ | this batch — verified on real runs |
| **Reviewer task suite** | 📋 designed | [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md) |
| **Reviewer formal protocol** | 📋 written | [MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md) |
| **Multi-agent UX** | 📋 designed | [MULTI_AGENT_UX](../design/MULTI_AGENT_UX.md) |
| **UI implementation plan** | 📋 written | [MULTI_AGENT_UI_IMPLEMENTATION_PLAN](../design/MULTI_AGENT_UI_IMPLEMENTATION_PLAN.md) |
| Reviewer formal eval | ❌ blocked | G4 — no case set with headroom |

## 1 · Completed this batch

### Contribution trace closure (code)

The reviewer pilot could not score its treatment arm: every
`sub_agent_finished` on the independent-review path carried
`contribution: null`.

The stated reason was architectural — *"the review stage runs outside the
executor's ledger"*. **That was wrong.** `run_review` already adopts its
findings into the parent ledger and persists the snapshot. The projection was
unavailable only because `ledger` was bound inside the
`if !result.findings.is_empty()` block and out of scope at the finish event one
block below.

Changes:

| File | Change |
| --- | --- |
| `leveler-lifecycle/src/findings.rs` | `ContributionSource` enum; `with_source` |
| `leveler-engine/src/turn.rs` | hold the ledger across finish; always project |
| `leveler-agent/src/executor/drive.rs` | stamp `ExecutorChild` source |
| `leveler-engine/tests/direct_test.rs` | 2 tests locking the trace |

`ContributionSource` has three variants because three producers exist:
`SelfReported` (`record_finding`), `ExecutorChild` (`spawn_agent`),
`IndependentReviewer` (`run_review`). The ledger cannot distinguish the last
two on its own — both write `source_child` + `role` — so the settlement site,
which knows, stamps it.

It is on `ChildResultProjection`, not on `FindingRecord`: the question is
answered once per child at settlement, and `FindingRecord` is in every
persisted ledger snapshot. No migration, no second finding model.

### Observer fixes (eval)

| Defect | Fix |
| --- | --- |
| Independent `expect` computed, then discarded | `load_expect_verdicts()`, joined into the run record |
| `contribution: null` read as zero findings | `contribution_unmeasured` reported separately |
| Reports printed fabricated `zero-finding reviewers: 5` | explicit "not observable" block |

The second defect is the one worth remembering: the runtime said "not
measured", the observer wrote `0`, and the report stated as fact that five
reviewers found nothing — when all five had reported. **A null read as a zero
is a fabricated measurement.**

### Documents

- [MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md) — case set design
- [MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md) — formal protocol
- [MULTI_AGENT_UX](../design/MULTI_AGENT_UX.md) — UX design
- [MULTI_AGENT_UI_IMPLEMENTATION_PLAN](../design/MULTI_AGENT_UI_IMPLEMENTATION_PLAN.md)

## 2 · Evidence

### Contribution trace, before and after

Same five cases, same model, same arm config. Only the binary differs.

```
before                                after
────────────────────────────          ────────────────────────────
reviewer spawned            5/5       reviewer spawned            5/5
contribution UNMEASURED     5/5       contribution UNMEASURED     0/5
zero-finding reviewers        5  ✗    zero-finding reviewers        4  ✓
noise                         0  ✗    noise                         1  ✓
```

The `before` column's last two numbers were fabricated. `after` reports a
measured zero for four reviewers and one real noise event: a finding adopted
at `Acknowledged` that the parent never judged — neither accepted nor
rejected. That is the protocol's definition of noise, and the product could
not previously report it.

Every projection now carries `profile_id`, `capabilities`, and
`source: {kind: independent_reviewer, review_id: …}`.

n=1 per case. This is infrastructure verification, **not** a Reviewer result.

### Control-arm saturation

Every recorded baseline for `deepseek/deepseek-v4-flash` is at ceiling:

| Baseline | Result |
| --- | ---: |
| `icg-integrated` (icg-1…5) | 15/15 |
| `c4-self-healing` (r1…r8) | 24/24 |
| `c2-bv2-navigation` | 24/24 |
| `c3-edit-reliability` | 24/24 |
| `icg6r-honest-failure` | 1/3 |

Source: `evals/baselines/*.json`, 3 repetitions each.

The only sub-ceiling case is an unsolvable-task honesty probe, where failing is
the intended outcome. **The existing corpus cannot host the Reviewer
experiment**, and selecting a different subset does not help.

### Tests

- `python3 -m unittest discover -s eval/tests` → **114 passed**
- `cargo test --workspace` → see the run log; result recorded below

A first workspace run reported `explorer_profile_cannot_write` failing. It was
a harness artifact, not a regression: two `cargo test --workspace` invocations
were running concurrently, and `child_profile_spawn.rs`'s `tmp()` builds a
**fixed** path (`leveler-child-profile-{tag}-{salt}`, no pid, no randomness),
so one run's `remove_dir_all` deleted a file the other was reading. The test
passes in isolation and in a single clean run.

That fixed path is a real test-isolation weakness — two concurrent test runs on
one machine will keep colliding. Not fixed here: out of this batch's scope.

## 3 · Remaining blockers

### G4 — no case set with headroom (blocking the formal eval)

Needs 6–8 cases with a **measured** control pass rate of 50–70 %. Requires real
model calibration runs; cannot be satisfied by writing code.

Design is done ([MA-VALUE-REVIEWER-TASKS](MA-VALUE-REVIEWER-TASKS.md)),
including the four categories, the trap requirement, and the schema. What is
not done is authoring and calibrating the cases.

### Finding → fix causality has no data source

The UX Timeline's central claim — *this change happened because of that
finding* — is not recorded. `FindingState::Addressed` says a finding was
addressed; it does not name the mutation.

Changes runtime data. Documented in the UI plan, not implemented, per the
freeze.

### Contribution never reaches a client

`event_bridge.rs:507` drops `contribution` via a `..` rest pattern and blanks
`role` on the finish branch. Every UX screen is blocked on this. It is a small
additive fix and is Step 1 of the UI plan.

## 4 · Frozen, and still frozen

Reviewer prompt · reviewer tools · reviewer permissions (read-only) · trigger
policy · spawn runtime · Marketplace · remote agents · ACP · Cloud Worker.

The runtime change in this batch is observability: a projection that was
already computed and then dropped is now emitted. No agent behaves differently.

## 5 · Next step

**Author and calibrate three Reviewer cases** — one concurrency, one security,
one refactoring — and run control-only at n=3 each. Keep what lands in 50–70 %.

Calibrate three before authoring eight. The pilot's case set was authored
without calibration, and that is why it was saturated.

After G4 clears: run [MA-VALUE-REVIEWER-FORMAL](MA-VALUE-REVIEWER-FORMAL.md).

Independently and safely in parallel: Steps 1–2 of the UI plan (get
`contribution` and `profile` onto the wire). They change no agent behavior and
they also unblock the formal experiment's secondary metrics.

Do **not** start UI rendering (Steps 3–6) before the Reviewer verdict. If the
Reviewer does not earn its cost, a UI showcasing Reviewer contribution
showcases a feature that needs re-scoping.

## Route

```
Reviewer Contribution Trace     ✅ closed, verified on real runs
        ↓
Reviewer Task Suite             📋 designed · ❌ not calibrated   ← next
        ↓
Reviewer Formal Eval            📋 protocol ready · blocked on G4
        ↓
Multi-Agent UX                  📋 designed
        ↓
UI Implementation               📋 planned · Steps 1–2 unblocked
        ↓
1.0 Beta Gate
```
