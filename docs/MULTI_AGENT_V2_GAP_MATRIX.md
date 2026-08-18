# Multi-Agent V2 — semantic gap matrix

Columns: dsh = DeepSeek Harness @99f6f02 · CL = CodeLeveler @a7ee9b2. ACTION targets Beta unless marked post-Beta.

| Row | dsh | CL today | Beta target | Action |
| --- | --- | --- | --- | --- |
| Main coordinator role | implicit via tool prompt section | absent (executor-only framing) | explicit coordinator+integrator policy, one canonical home | **BUILD** (policy text) |
| Delegation policy | neutral offload-independent-work + imperative background scheduling | neutral one-shot decision point only | delegation-preferred for independent bounded work; scheduling imperative | **BUILD** |
| Small-task KEEP | implied | proven (controls) | keep | KEEP |
| Independent-work delegation | first-class | possible, never chosen (rationally: blocking) | rational under background runtime | **BUILD** (runtime) |
| Multi-child fanout | one message, unlimited bg | same-batch concurrent (≤4), blocking | same-batch + background, caps kept | EXTEND |
| Background-first | runtime default resolution | absent (fully synchronous) | `run_in_background` param, runtime-resolved default=true | **BUILD** |
| Parent continues work | explicit policy + runtime | impossible (round blocks) | parent free during bg children; write-fence vs child scopes | **BUILD** |
| Parent wait semantics | fg only when next action depends | n/a | same criterion, fg = today's sync path | **BUILD** |
| Duplicate-work avoidance | model's responsibility | n/a (parent frozen) | STRUCTURAL: parent writes inside active child scope denied | **BUILD (stronger than dsh)** |
| Fresh child | yes | yes (self-contained task) | keep | KEEP |
| Context-fork child | yes | no | post-Beta | DEFER |
| Durable child identity | durable subagent id | events only, not model-usable | id in spawn result + settlement notice | **BUILD** |
| Child continuation | continuable sessions | no | post-Beta | DEFER |
| Parent→child message | send_message | no | post-Beta | DEFER |
| Child→parent report | report tool + obligations | report_finding (typed, durable) + final text | keep local; add early-partial-report wording | ADAPT |
| Runtime settlement | unconditional notice, idle-wake/busy-batch | in-round only | round-boundary settlement injection, unconditional, partial-preserving | **BUILD** |
| Child cancellation | interrupt tool | parent token propagation | keep (no interrupt tool for Beta) | KEEP/DEFER |
| Child listing | list_agents (anti-poll) | no | not needed at ≤6 children with notices | DEFER |
| Child provider abstraction | pluggable providers | in-process only | post-Beta (Codex/Claude/ACP) | DEFER |
| Worker write authority | none (shared cwd) | exclusive scope + allowlist | keep local | ALREADY_STRONGER |
| Explicit scope | none | required for Worker (file/dir) | keep | ALREADY_STRONGER |
| Scope overlap | model coordinates | same-batch refusal | extend refusal to ACTIVE background workers | **EXTEND** |
| Child failure truth | stopReason + partial text | ChildResult 4-way truth | keep; settlement carries it | ALREADY_STRONGER |
| Findings | none structured | typed lifecycle + blocking gate | keep | ALREADY_STRONGER |
| Reviewer | none (Ralph explicitly lacks independent evaluation) | harness-launched, diff-injected (revalidation pending) | production revalidation in this closure | VALIDATE |
| Completion Truth | worker self-report | verification-gated | keep; add outstanding-children completion gate | **EXTEND** |
| Recovery | cold resume for continuables | proven for main runtime; bg children are in-process | truthful lost-child note on resume (ledger-backed) | **BUILD** |
| UI observability | (n/a here) | SubAgent events wired to TUI | settlement path reuses existing events | KEEP |
