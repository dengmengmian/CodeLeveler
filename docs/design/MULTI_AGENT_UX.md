# Multi-Agent UX

**Status:** design · **Opened:** 2026-08-24 · **No code**

How CodeLeveler shows multi-agent work. Design only — this document does not
change the runtime, the client protocol, or any renderer. Implementation
sequencing lives in
[MULTI_AGENT_UI_IMPLEMENTATION_PLAN](MULTI_AGENT_UI_IMPLEMENTATION_PLAN.md).

## The thesis

Today the UI reports **activity**. It should report **contribution**.

```
  ✗  3 agents running
  ✗  reviewer: working…
  ✗  ✓ reviewer finished

  ✓  Explorer   7 files found · 5 the work used
  ✓  Reviewer   3 risks · 2 fixed · 1 declined
  ✓  Worker     8 files changed · verification passed
```

The left column is true and useless. It answers "is something happening?" —
a question the spinner already answers. The right column answers "was this
worth it?", which is the only question a user asks about a delegated agent.

This is not a cosmetic preference. Spawning a child costs the user tokens and
wall time. A UI that shows only that it ran presents the cost and hides the
return.

## What makes this possible now

Three pieces landed in that order, and the third is what unlocks the UI:

| Piece | Gives |
| --- | --- |
| `FindingRecord` + `FindingState` | a finding has a lifecycle, durably |
| `ChildProfile` | a child has a declared capability contract |
| `ChildResultProjection` | **what the parent did with what the child found** |

`ChildResultProjection` is the data model of every screen below. The UI is
mostly a rendering of it.

```rust
ChildResultProjection {
    child_id, role,
    profile_id, profile_role, capabilities,
    source,                      // executor_child | independent_reviewer | self_reported
    findings_total,
    findings_acknowledged,       // reached the parent
    findings_accepted,           // parent judged relevant
    findings_verified,           // proven after mutation
    findings_rejected,           // parent declined, with a reason
    findings_open_blocking,      // still gating closure
}
```

**Gap:** the client protocol's `SubAgentUpdated` carries
`{id, nickname, role, done, ok, detail}`. It does not carry the projection or
the profile. Every screen here is blocked on that until Phase 5 lands it.

## Design rules

1. **Contribution over activity.** Every agent surface answers "what did this
   change?" before "what is it doing?".
2. **A rejection is a contribution.** The parent read it and decided. Render it
   as a resolved outcome, not as waste.
3. **Zero findings is a real answer.** "Reviewed, nothing to flag" is a result.
   Render it as one — not as an empty state, not as a failure.
4. **Unmeasured is not zero.** If no projection exists, say "not measured".
   Never print `0`. This exact conflation made the pilot report five
   zero-finding reviewers that had all reported.
5. **Cost is always visible next to contribution.** They are read together or
   not at all.
6. **Never a generic chat UI.** This is a coding agent. The unit is a change to
   a repository, not a message.

## Screens

### 1 · Task Overview

The always-visible header. Answers: what is being done, by whom, how far along.

```
┌ Add rate limiting to the login path ─────────────── 4m 12s ─┐
│                                                              │
│  Worker      ●  editing internal/auth/limit.go               │
│  Explorer    ✓  7 found · 5 used                             │
│  Reviewer    ●  reviewing 3 files                            │
│                                                              │
│  8 files changed · 2 findings open · gates not yet run       │
└──────────────────────────────────────────────────────────────┘
```

- One line per agent. Never a count ("3 agents") without the roles.
- A finished agent collapses to its contribution, not to a checkmark.
- The bottom line is the task's own state, not a sum of agents.
- **Open blocking findings appear here.** They gate closure; burying them in a
  panel means the user learns about the block at the end.

### 2 · Agent Card

One agent, expanded. Reached from the overview.

```
┌ Reviewer ───────────────────────────────────────────────────┐
│  profile      reviewer · read-only                          │
│  source       independent review (harness-launched)         │
│  purpose      review the login-path change for defects      │
│  status       completed                                     │
│                                                             │
│  Contribution                                               │
│    3 findings                                               │
│      ✓ 2 accepted → fixed → verified                        │
│      ⊘ 1 declined  "covered by the existing guard"          │
│                                                             │
│  Cost                                                       │
│    6 turns · 84k tokens · 41s · 12 tool calls               │
└─────────────────────────────────────────────────────────────┘
```

Fields, in this order — contribution before cost, always:

| Field | Source |
| --- | --- |
| profile | `profile_id` + `capabilities` |
| source | `ContributionSource` |
| purpose | `SubAgentStarted.task` |
| status | `done` / `ok` |
| contribution | `ChildResultProjection` |
| cost | `SubAgentProgress` |

Read-only is stated, not implied. A user asked to trust an agent that read
their codebase should see what it was allowed to do.

Empty and unmeasured states:

```
  Contribution
    Reviewed 3 files. Nothing to flag.          ← zero, measured

  Contribution
    Not measured.                                ← no projection
    This build does not report reviewer findings.
```

### 3 · Timeline

Causality, not chronology. The point is which agent's finding caused which
change.

```
  ●  Task started        add rate limiting to login
  │
  ├─ ●  Explorer         found internal/auth/limit.go, session.go  (+5)
  │
  ├─ ●  Worker           changed 8 files
  │
  ├─ ●  Reviewer         f-1  the limiter resets on 429
  │     │                f-2  no cap on the retry path
  │     │                f-3  style · declined
  │     │
  │     ├─ ●  Worker     fixed f-1, f-2                   ← caused by f-1/f-2
  │     │
  │     └─ ●  Verified   go test ./... passed             ← f-1, f-2 → verified
  │
  ●  Completed           verified
```

- A finding is a node with an id the user can follow.
- Indentation is causation: a fix nests under the finding that caused it.
- A verification that closes findings names them.

A flat event log cannot show this. The nesting is what makes review value
legible — the user sees that the reviewer's finding is why the code changed.

### 4 · Inspector

Deep dive on one agent or one finding. Where the raw truth lives.

**Agent inspector**

```
  Reviewer  reviewer-7f8c9758
  ├ profile        reviewer
  ├ capabilities   read_file · search · list_dir
  ├ tools used     read_file ×7 · search ×5
  ├ findings       f-1 verified · f-2 verified · f-3 rejected
  └ verification   go test ./...  passed  after f-1, f-2
```

**Finding inspector**

```
  f-2  no cap on the retry path
  ├ kind        correctness      ├ blocking   yes
  ├ file        internal/auth/limit.go
  ├ source      Reviewer (independent review)
  └ lifecycle
      created      by reviewer-7f8c9758
      acknowledged adopted into the parent ledger
      accepted     parent judged relevant
      addressed    limit.go:88 — cap applied to retries
      verified     go test ./... passed
```

The lifecycle rail is the same state machine as `FindingState`. It is not a
UI-side reinterpretation, and it must not become one.

## Anti-patterns

| Do not | Because |
| --- | --- |
| "3 agents running" | Activity without contribution |
| A progress bar per agent | Rounds are not progress; the user cannot act on it |
| Hide rejected findings | A rejection is a judgment the user should see |
| `0 findings` when unmeasured | Fabricates a measurement |
| Render children as chat messages | The unit is a repo change, not a turn |
| A generic "agent" with no profile | Profile is the capability contract; hiding it hides the permissions |
| Sum agents into one number | "12 findings" across roles is not a fact anyone needs |

## Surface differences

The model is one; the rendering differs by what the surface can hold.

| | TUI | Web |
| --- | --- | --- |
| Overview | Collapsed block above the composer | Persistent sidebar |
| Agent Card | Expand in place | Panel |
| Timeline | Indented transcript cells | Graph with collapsible branches |
| Inspector | Overlay | Detail route |

The TUI is the constraint that keeps the design honest: if a contribution
cannot be stated in one line of terminal text, it is not yet a clear enough
fact.

## Open questions for the owner

1. **Live findings.** Should a finding appear the moment the reviewer reports
   it, or only after the parent acknowledges it? Live is more responsive;
   post-acknowledgement matches the durable model and avoids showing findings
   that are then dropped. *Leaning: post-acknowledgement — the ledger is the
   truth and adoption is immediate anyway.*
2. **Cost placement.** Per-agent only, or a task-level roll-up too? A roll-up
   invites optimizing the wrong number. *Leaning: per-agent only.*
3. **Failed children.** A child that errors has no contribution. Show it in the
   overview or fold it into a diagnostics view? *Leaning: show it — a spend
   with no return is exactly what the user needs to see.*

Recorded, not decided. Each affects runtime data and is out of scope while
Reviewer evaluation is frozen.

## Related

- [CHILD_PROFILE](CHILD_PROFILE.md) — the capability contract
- [MULTI_AGENT_UI_IMPLEMENTATION_PLAN](MULTI_AGENT_UI_IMPLEMENTATION_PLAN.md)
- [MA-VALUE-REVIEWER-FORMAL](../evaluations/MA-VALUE-REVIEWER-FORMAL.md)
