# Working on CodeLeveler

Rules for an AI agent or contributor changing this repository.

This file is an **execution guide**, not a second architecture document.

The architecture authority is:

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Chinese: [`docs/ARCHITECTURE.zh-CN.md`](docs/ARCHITECTURE.zh-CN.md)

The two architecture documents are intended to be semantically equivalent. If this file and the architecture disagree, the architecture wins and this file must be corrected.

---

## Read First

| Document | Authority |
| --- | --- |
| `docs/ARCHITECTURE.md` | System architecture, ownership boundaries, invariants, allowed / forbidden architecture changes, evolution direction |
| `CONTRIBUTING.md` | Contribution workflow, required checks, error and dependency conventions |
| `evals/README.md` | Evaluation design and adding evaluation cases |
| `SECURITY.md` | Security policy |
| `CHANGELOG.md` | Released changes |

Do not start a foundation or runtime refactor from filenames, line counts, or architectural taste alone. Read the relevant code and identify the current owner first.

---

# Architecture Gate

Every change must preserve the architecture defined in `docs/ARCHITECTURE.md`.

The shortest possible model is:

```text
Model owns intelligence.
Harness owns domain semantics.
Runtime owns lifecycle and mechanical correctness.
Capability owns reusable ability.
Host Authority owns controlled real side effects.
Every persistent fact has one authoritative owner.
Product projects Runtime truth; it does not create it.
```

And the central lifecycle rule is:

```text
Engine owns lifecycle, not agent intelligence.
```

## What You May Do

You may:

- add product- or domain-specific behavior in the owning Harness;
- add domain-neutral lifecycle or mechanical mechanisms to Runtime;
- add reusable Capabilities with clear ownership;
- expose a Capability through a thin model-facing Tool adapter;
- add Provider / Protocol support without changing domain semantics;
- add new Products, Clients, Harnesses, or execution backends above the existing boundaries;
- introduce an abstraction when a real boundary or real second consumer demonstrates the need;
- strengthen reliability, authority, persistence, recovery, cancellation, and mechanical correctness.

For the complete allowed design space, read **Architecture Guardrails: Allowed and Forbidden** in `docs/ARCHITECTURE.md`.

## What You Must Not Do

Do not:

- put Coding, Review, repository workflow, or product completion semantics into the Agent Kernel, Engine, or Foundation;
- make Runtime or Harness secretly reason for a weak model, choose its next investigative step, or change behavior by model intelligence class;
- create a second filesystem, process, permission, approval, sandbox, or workspace-write authority outside Host Authority;
- turn Tools into service locators, global policy owners, process-lifecycle owners, or persistent-state owners;
- create multiple authoritative sources for the same persistent fact;
- introduce reverse dependencies from general layers to product-specific layers;
- use a fallback that changes the meaning of an operation while pretending the original capability succeeded;
- change mechanical capability boundaries because a task looks hard or a model looks weak;
- pre-build a crate, trait, manager, registry, framework, or abstraction only for hypothetical future extensibility;
- let a Child Agent, Reviewer, or Worker create a second lifecycle / permission / ownership / persistence system beside Runtime;
- encode local-machine assumptions into core Agent semantics.

If a proposed change needs one of these, redesign it at the correct owner instead.

---

# Architecture Decision Check

Before changing Foundation, Runtime, Capability, or authority code, answer all of these:

```text
Who owns this responsibility?

Is it domain semantics or mechanical mechanism?

Can it remain in the Harness?

Would a semantically different Harness need it too?

Does it create a new Authority?

Does it create a second source of persistent truth?

Does it introduce a reverse dependency?

Does it change the meaning of an existing capability?

Is the abstraction solving a real boundary or an imagined future?
```

If ownership is unclear, stop the refactor and establish ownership first.

If a new Harness would require teaching the Agent Kernel that Harness's vocabulary, the boundary is wrong.

---

# Anti-Overengineering Rule

Do not add a crate, trait, manager, coordinator, supervisor, registry, factory, adapter layer, or generic framework because of:

```text
future extensibility
clean architecture aesthetics
maybe useful later
more generic
symmetry with another component
```

A new abstraction needs at least one real driver:

- two real implementations or consumers;
- a real dependency-inversion boundary;
- an independent security boundary;
- an independent protocol boundary;
- an independent persistence boundary;
- an independent lifecycle or remote-deployment boundary;
- an observed ownership or coupling defect.

Otherwise prefer the concrete implementation.

Architecture must **allow** future Review, Multi-Agent, Remote, and Cloud evolution. That is not the same as implementing those futures before a real product or runtime requirement exists.

Simplification means moving complexity to the correct owner. It never means weakening reliability, authority, persistence, cancellation, recovery, or safety.

---

# Working Rules

## Documentation Ownership

One concern has one canonical owner. Other documents link or summarize.

| Concern | Owner |
| --- | --- |
| Architecture | `docs/ARCHITECTURE.md` + synchronized `docs/ARCHITECTURE.zh-CN.md` |
| Agent / contributor execution guide | `AGENTS.md` |
| Contribution workflow and checks | `CONTRIBUTING.md` |
| Evaluation | `evals/README.md` |
| Security | `SECURITY.md` |
| Released changes | `CHANGELOG.md` |

Do not copy large rule sets between documents. If a detailed rule already belongs to Architecture, link to it rather than creating a second detailed version here.

---

## Unsafe Policy

Production crates are expected to reject unreviewed `unsafe` code.

`leveler-execution` is intentionally the exceptional authority boundary where narrowly scoped OS-level `unsafe` may be required for host process semantics. Do not mechanically tighten or loosen crate-level unsafe policy without reading the justification around the affected code and preserving the required host guarantee.

A new `unsafe` block requires a concrete OS/runtime need and a narrowly scoped justification. Convenience is not sufficient.

---

## Language Conventions

- Code comments and doc comments: **English**.
- User-visible strings may be Chinese where the product surface is Chinese.
- Test fixtures may use either language as the case requires.
- Top-level bilingual documentation uses separate English and `.zh-CN.md` files rather than mixed-language prose.

When one architecture language version changes semantically, update the other in the same work item.

---

## Tests

- Integration tests belong in `crates/<crate>/tests/`.
- Unit tests stay next to the implementation under `#[cfg(test)] mod tests` when appropriate.
- Tests that require an optional OS capability must use the project's capability detection rather than assume the capability exists.
- Architecture boundaries should be mechanically protected when a stable dependency or ownership invariant can be tested.

Do not add tests merely to mirror file structure. Test behavior, contracts, ownership, and boundaries.

---

## Before Proposing a Refactor

Do not infer architecture from line count or filename.

Before proposing a split or extraction:

1. Read the production code in the relevant module.
2. Separate production lines from inline tests.
3. Identify the current responsibility owner.
4. Identify the actual coupling or maintenance problem.
5. Decide whether the change is within one crate or creates a real architectural boundary.
6. Do not claim an incremental compile-time improvement from splitting a Rust source file inside the same crate; rustc compiles the crate as the unit.

In Rust 2018+, `foo.rs` and a `foo/` submodule directory can coexist. A submodule does not require mechanically renaming the parent to `foo/mod.rs`.

Split for readability when readability is the real problem. Extract a crate only when a crate boundary is the real solution.

---

## Provider and Model Changes

Provider differences are protocol facts, not intelligence policy.

When adding or changing a Provider:

- represent its real mechanical capabilities honestly;
- adapt wire-format differences in the Provider / Protocol layer;
- negotiate capability explicitly;
- disable dependent features when a required mechanical capability is absent;
- do not invent a second domain behavior to make providers appear equivalent.

A model being weaker than another model is not a Runtime bug and is not a valid reason to add hidden reasoning behavior.

---

## Tool and Capability Changes

Before adding a Tool, ask whether the system needs a new **Capability** or merely a new model-facing **intent**.

A Tool should remain an adapter:

```text
model intent
   ↓
Tool
   ↓
Capability
   ↓
Host Authority when a controlled side effect is required
```

Do not duplicate a reusable runtime inside a Tool.

Do not give every Tool a universal context containing unrelated services.

Do not move permission, approval, global policy, or persistent truth into Tool dispatch for convenience.

---

## Multi-Agent Changes

Multi-Agent work must extend the shared Runtime rather than build a parallel agent system.

Roles such as Explorer, Worker, Reviewer, or Specialist may have different Harness semantics and capability sets, but they must share the Runtime's mechanical rules for:

```text
lifecycle
persistence
ownership
authority
cancellation
settlement
```

Do not implement role semantics in the generic Engine.

---

## Verifying a Change

The canonical verification commands live in `CONTRIBUTING.md`.

Run the checks required there. Do not report a change as working because `cargo build` alone succeeded.

When a check cannot run because an environment capability is unavailable, report the exact check and reason. Do not silently skip it.

For architectural changes, verification should cover both behavior and the ownership/dependency boundary that the change claims to preserve.
