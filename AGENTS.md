# Working on CodeLeveler

Notes for an AI agent (or a new contributor) changing this repository.

This file is two things and nothing else: the **architecture constitution**
that every change is judged against, and an **index** of the working rules
that live elsewhere. Where a rule already lives in code or in another
document, the pointer is the authority and this file is not.

The full architecture is `docs/ARCHITECTURE.md` (Chinese:
`docs/ARCHITECTURE.zh-CN.md`). This file does not duplicate it.

## Read first

| Document | Covers |
| --- | --- |
| `docs/ARCHITECTURE.md` | Layers, boundaries, dependency direction, known debt — the canonical architecture |
| `CONTRIBUTING.md` | PR expectations, the exact check commands, error-type and dependency-direction rules |
| `evals/README.md` | Required before adding evaluation cases |

The mobile client is frozen at tag `mobile-beta-mvp` — no Push / fleet / voice /
rewrite until real Beta users.

`CONTRIBUTING.md` already states the three rules most often gotten wrong:
library crates expose typed errors, `anyhow` stays at application boundaries
(`leveler-cli` / `leveler-app` only), and no lower-level crate may take a
dependency edge back to a user-facing layer. Those are not repeated here.

---

# Architecture constitution

CodeLeveler is not "a coding agent and everything under it". It is a
foundation, with the coding agent as its first product on top:

```text
                     Leveler Foundation
                            │
          ┌─────────────────┼─────────────────┐
          ▼                 ▼                 ▼
    Coding Harness     Review Harness    Future Harness
          ▼                 ▼                 ▼
      CodeLeveler       Review Product     Future Product
```

The Foundation provides reusable agent-runtime capability. A Harness defines
domain semantics. A Product defines experience, composition and delivery.

## The seven sentences

```text
The Kernel does not know the product.

The Harness defines domain semantics.

The Harness exposes capability; it does not emulate intelligence.

The Engine owns lifecycle, not agent intelligence.

The Host Authority exclusively owns controlled side effects.

Every durable fact has one authoritative owner.

Mechanical Truth is not Semantic Satisfaction and is not User Acceptance.
```

## The twelve rules

1. **The kernel is product-neutral.** `leveler-agent-core` owns the model↔tool
   loop, streaming, retry, rounds, budgets, usage, deadlines, cancellation and
   stop reasons. It owns no coding, review, repository, prompt, permission,
   persistence or UI concept.
2. **Harnesses are siblings.** A future `leveler-review` sits beside
   `leveler-agent`, not under it. A harness never depends on another harness.
3. **Tools are adapters; capabilities are implementations.** A tool renders a
   capability to the model. It does not implement the capability, and it does
   not own policy. See the tool rules below.
4. **Host side effects have one authority.** `leveler-execution` owns workspace
   path enforcement, permission, approval, process execution, sandboxing and
   cancellation. An agent *requests* a side effect; the host authority
   *performs* it. Do not build a second path to the filesystem or to a process.
5. **The engine owns runtime mechanics, not product semantics.** Task/turn
   lifecycle, event ordering, the append-only log, checkpoint, resume,
   recovery, ownership, reaping. Not prompts, not tool selection, not what
   "done" means.
6. **Mechanical Truth ≠ Semantic Satisfaction ≠ User Acceptance.** The runtime
   proves what ran and what changed. The model interprets whether the goal is
   met. The user accepts. A green check is not proof that a request was
   satisfied, and observing a changed tree is not proof that the work is done.
7. **Every durable fact has one authoritative owner.** No parallel source of
   truth for task status, turn status, evidence, ownership, usage or artifacts.
8. **UI and protocol layers project truth; they do not invent it.** A tool
   returning `Ok` does not let a client decide the task is complete.
9. **Dependency direction is part of correctness.** Foundation ← capabilities
   and runtime ← harnesses ← products. No lower layer may depend on a
   user-facing one.
10. **New abstractions require evidence.** See the next section.
11. **Foundation changes carry a higher bar than product changes.** Extend
    above the foundation before you modify it.
12. **A second product harness must not require modifying the agent kernel.**
    It builds its own tool host and its own tool surface over the same kernel;
    reusing the Coding tool plumbing is not required. This is the *Second
    Harness Test*, defined in `docs/ARCHITECTURE.md`.

## Model capability policy

```text
Model intelligence is not a Harness responsibility.
```

The Harness exposes deterministic capability and product semantics. It does
not emulate reasoning, compensate for a weak model, or grow a second behaviour
whose purpose is to normalize model intelligence across models. CodeLeveler
accepts model intelligence as an input, not a deficiency the runtime must
correct.

The line is ownership, not effort:

```text
ENGINEERING FAILURE   → the Runtime handles it.
MODEL CAPABILITY LIMIT → the model owns it.
```

Runtime reliability and provider compatibility are separate concerns and stay
fully mandatory. `persist-before-forward`, F7 Grounded Authority, ToolHost
admission, permission, approval, the ownership fence, sandboxing, path safety,
CAS, stale-write protection, atomic mutation, rollback, crash recovery,
cancellation and the EvidenceLedger compensate for machines, concurrency and
attackers — never for reasoning. Provider capability differences (tool calling,
streaming, reasoning transport, structured output, context and output limits,
wire format) are protocol facts owned by `leveler-model` / `leveler-protocol` /
`leveler-provider`; negotiate and report them, do not grow a second behavioural
path from them.

"a weaker model needs this" is not a reason to add or keep anything.
`docs/ARCHITECTURE.md` §1.1 carries the detail and the decision test.

## The tool rules

```text
Tools are adapters, not runtimes.

The Foundation tool boundary is leveler-agent-core::ToolRuntime.

Tool dependencies are explicit; tools do not receive a universal service
locator.

ToolHost admits; Host Execution performs.

Policy belongs to the Harness and the ToolHost, not to ToolRegistry or to
individual tools.

Capability complexity belongs to the capability owner.

Authoritative runtime facts do not hide in arbitrary JSON metadata.

Do not introduce a generic Tool Core without demonstrated need.
```

Three more, on what the model sees:

- **The harness decides the tool surface.** Expose the smallest surface that
  preserves capability. Many capabilities is not many tools per round.
- **A tool earns its place by expressing a distinct model intent**, exposing a
  distinct capability, having deterministic semantics, or removing round trips
  a competent model would otherwise pay. Not by rescuing a weak one.
- **Architecture correctness is settled by mechanical evidence; product-surface
  value is settled by evaluation.** A proven implementation defect — wrong
  ownership, non-deterministic semantics, a duplicate runtime path, platform
  divergence, runtime capability living in a tool adapter — is fixed directly.
  Removing a *capability* the product may depend on is where measurement is
  owed.

`docs/ARCHITECTURE.md` §5 and §6 carry the detail. Do not copy it here.

## Anti-overengineering rule

Do not add a crate, trait, manager, coordinator, supervisor, factory, adapter
layer or generic framework because of "future extensibility", "clean
architecture", "maybe useful later" or "more generic".

A new abstraction needs at least one of:

- two real implementations that need it;
- a real dependency-inversion boundary;
- an observed coupling or ownership defect;
- an independent protocol, security, persistence or runtime boundary.

Otherwise prefer the concrete implementation. In particular: do **not**
pre-build interfaces for a Review or multi-agent product that does not exist
yet. The architecture must *allow* them to appear. That is not the same as
implementing them now.

Two proposals are already rejected, not deferred. Do not revive either without
new evidence: a generic `leveler-tool-core` crate (the foundation tool
boundary already exists, `docs/ARCHITECTURE.md` §5.1), and a replacement
container for `ToolContext` designed before the capability extraction it is
meant to serve (§5.5).

Simplify by moving complexity to its correct owner. Never by deleting
reliability: `persist-before-forward`, the ownership fence, compare-and-swap
edits, stale-write protection, sandboxing, approval and crash recovery all
survive every move intact.

## Architecture decision test

Before changing anything in the foundation, answer:

- Why does this belong in this layer?
- Is this runtime mechanism, or product semantics?
- Does this introduce a new source of truth?
- Does this create a reverse dependency?
- Could it stay in the harness instead?
- Would a Review harness need this too?
- Would implementing Review require changing the agent kernel?
- Is this abstracting a real duplication, or an imagined future?

---

# Working rules

## Documentation ownership

One rule, one canonical owner. Everywhere else links or summarizes.

| Concern | Owner |
| --- | --- |
| Architecture | `docs/ARCHITECTURE.md` |
| Agent / contributor constitution | this file |
| Contribution workflow and checks | `CONTRIBUTING.md` |
| Released changes | `CHANGELOG.md` |
| Security policy | `SECURITY.md` |
| Evaluation | `evals/README.md` |

Do not copy a rule set from one of these into another. Two documents stating
the same rule is how they start disagreeing.

## The unsafe policy has exactly one exception

Every crate but one carries `#![forbid(unsafe_code)]`. `leveler-execution` is
deliberately different: `crates/leveler-execution/src/lib.rs` uses
`#![deny(unsafe_code)]`, because `forbid` cannot be relaxed by a scoped
`allow`, and this crate needs exactly one — the Linux `PR_SET_PDEATHSIG`
pre-exec hook in `command.rs` / `background.rs`, the only way to guarantee
grandchildren die when the parent is force-killed.

**Do not "fix" that `deny` into a `forbid`.** It will not compile, and the
reason is documented at the declaration. In the production crates covered by
these crate-level attributes, a new `unsafe` block still fails the build unless
it is explicitly allowed and justified the same way.

## Language conventions

- **Code comments and doc comments: English.**
- **User-visible strings and TUI copy: Chinese is expected** where the product
  surface is Chinese (see `crates/leveler-execution/src/risk.rs`, where
  permission-profile names carry their Chinese label).
- **Test fixtures: either**, as the case requires.
- Top-level docs that need a Chinese version use the `.zh-CN.md` suffix
  alongside the English original, not a mixed-language file.

## Tests

- Integration tests go in `crates/<crate>/tests/`.
- Unit tests are inline `#[cfg(test)] mod tests` in the file under test. Keep a
  test next to what it covers rather than inventing a parallel structure.
- A test needing an OS capability that may be absent must use the project's
  existing capability checks, not assume the feature exists.

## Before you propose a refactor

This codebase has several files over 2000 lines, and they attract mechanical
"split this up" suggestions. Two failure modes to avoid:

1. **Do not infer a file's contents from its line count.** Command
   classification lives in `approval.rs`, shell-AST danger analysis in
   `shell_ast.rs`, risk vocabulary in `risk.rs` — not in `command.rs`, which
   is what a line-count-driven reading tends to assume. Read the file.
2. **Line counts include inline tests.** `command.rs` measures 2649 lines when
   ~1501 of them are `mod tests`. Compare production code, not totals.

Also note: in Rust 2018+, `foo.rs` and a `foo/` directory coexist. Adding a
submodule does not require renaming the parent to `foo/mod.rs`.

Splitting a file inside one crate does **not** reduce incremental compile time
— rustc's unit is the crate. Split for readability, and say so; do not claim a
build-time benefit that will not materialize.

## Verifying a change

The commands are in `CONTRIBUTING.md`. Run them; do not report a change as
working on the strength of a successful `cargo build` alone. When a test fails
for an environment reason rather than the change, say which test and why —
silently skipping is worse than a documented gap.
