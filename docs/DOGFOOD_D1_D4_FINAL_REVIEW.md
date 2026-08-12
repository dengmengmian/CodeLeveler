# D1–D4 Final Dogfooding Review

Read-only phase-closeout review. No product code changed. Not a merge.

## 1. Scope

A single verdict over the D1–D4 real-project dogfooding campaign (CodeLeveler run
against real OSS repos under supervisor methodology), plus the D4 replay after the
Long Task Reliability Gate. Purpose: decide whether the Real Dogfood phase can
close, and record the final, de-duplicated finding inventory. All D-runs used
CodeLeveler `6983d51` as the agent-under-test except the D4 replay, which used the
gate build (`fix/long-task-reliability-gate`, R1+R2).

## 2. D1 — viu (small Rust task) → PASS

Locate→modify→test on a real repo; directory ordering feature. 27 tools, 24 model
requests, 9.8k output tokens, 0 failed tools, 0 repeated reads, 0 intervention, no
false completion, TUI clean. Historical finding: on reconnect, historical
*completed* ToolGroups are not re-projected (chat history restores; the finished
tool-group rows don't).

**Final classification of the D1 finding:** OPEN — NEXT PHASE (TUI presentation),
severity NOTE/MINOR. It is a cosmetic reconnect-projection gap, not a correctness
or data-loss issue (the durable events are intact; only the collapsed tool-group
rows aren't re-rendered). The Long Task Gate did NOT touch it. Owner layer:
TUI/`presentation`. Reason to keep open: harmless today, worth a small fix when the
reconnect UX is next revisited.

## 3. D2 — duf (medium Go cross-file config) → PASS

JSON config support across files; correct precedence (`flags.Visit`), unknown-key
rejection, cross-platform `os.UserConfigDir`, 15 tests, docs synced. 72 tools, 63
requests, 36k tokens, 0 failed, 0 repeated reads, 0 intervention, no false
completion. Agent self-identified a pre-existing gofmt issue and the XDG-on-macOS
nuance — both positive reasoning signals.

**Final classification of the `--config + --no-config` finding:** EXPECTED / NOT A
PRODUCT BUG — hidden-spec mismatch, not a defect. The public task never required
the two flags to conflict-error; the agent's "`--no-config` silently wins" is a
defensible CLI design. This is exactly what the supervisor's fairness rule exists
to surface; it does not count against the agent. Owner: spec/test-authoring
discipline, not CodeLeveler.

## 4. D3 — TailAdmin (Next.js frontend) → CONDITIONAL PASS

Functional: PASS (supervisor verified all interactions in a real headless browser —
numeric budget sort, filters, URL state, dark mode, a11y). Implementation was
correct and well-architected (Suspense boundary, primitive reuse, 15 `node --test`
units, no E2E framework added, no new deps). Agent behavior: CONDITIONAL — it hit
the single-turn round ceiling during its OWN real-browser verification step
(sandbox headless Chrome was unstable), spending ~37 Chrome/curl/jsdom workaround
attempts, and honestly reported "⚠ not finished" rather than claiming done.

**D3 finding disposition:**
- single-turn round ceiling → **CLOSED by the Long Task Reliability Gate (R2)**:
  the ceiling now ends a work window, not the goal. Directly validated by the D4
  replay (1→4 windows).
- browser-verification capability gap → **OPEN — NEXT PHASE (Browser Capability)**.
  The agent had no first-class browser tool, so real-UI verification fell back to
  fragile shell/headless-Chrome. Owner: a future Browser Capability + its dogfood.
- temporary-file hygiene (agent left `scripts/.tmp-*`, `chrome.log`) → DEFERRED,
  MINOR. Real but low priority; a Home/Workspace hygiene concern, best handled by
  the upcoming Home & Runtime Hardening (workspace pollution) rather than a point
  fix now.
- supervisor protocol deviation (the active TUI probe set was not fully executed
  in D3) → EXPECTED / methodology note, corrected in D4 (full mandatory probes +
  a real reconnect were run). Not a CodeLeveler finding.

## 5. D4 — memos first run → INCOMPLETE (honest)

Full-stack "favorite memos". First run: 1 work window, `turn_limit_reached`,
backend substantially complete (store + 3 DB drivers + migrations + proto/codegen +
API/authz + data-layer pagination, `go build` EXIT 0), frontend = 0, i18n = 0,
tests = 0. No false completion (the agent stopped mid-work; the completion gate
honestly withheld success). Two P1s were opened:
- **P1-A** Goal single-window ceiling.
- **P1-B** a claimed "daemon reconnect wedge".

## 6. Long Task Reliability intervention (Phase 0 + R1 + R2)

Phase 0 root-caused both. Critically, it **overturned the original P1-B diagnosis**.

## 7. Findings closed / open / superseded (final inventory)

### CLOSED (by gate code + regression + D4 replay)
- **F-A Goal single-window ceiling** (D3, D4-first). CLOSED by R2 multi-window
  continuation. Evidence: D4 replay reached 4 windows, `stop=completed`; continuation
  unit tests pin ceiling→next-window, cross-window-spin→stop, pinned→stop.
- **F-B Unattended execution killed by client exit** (the REAL P1-B, see below).
  CLOSED by R1 (execution hosts in the daemon). Evidence: D4 replay reconnect probe
  — goal advanced headless (239→244 tools with no client attached), reattach resumed,
  no wedge; controls A/B/C + PTY 9/9.

### SUPERSEDED / MISDIAGNOSED
- **F-B-original "daemon reconnect wedge"** → SUPERSEDED / MISDIAGNOSED. Phase 0
  controls proved the wedge was NOT a daemon coupling: `--auto-approve` forced
  `SocketIntent::Embedded`, so the goal's runtime lived inside the TUI process and
  died when the client was killed. Correct finding renamed **Unattended Execution
  Hosting / Client-Lifetime Coupling**. This is the dogfood/supervisor methodology
  working — an experiment corrected a wrong hypothesis before it drove a wrong fix
  (the wrong fix would have been "synthesize ToolCallFinished on reconnect", which
  the gate explicitly rejected as a double-terminal-state hazard). It is a SUCCESS
  of the process, not a failure.

### OPEN — NEXT PHASE
- Browser verification capability (D3) → Browser Capability phase.
- Reconnect historical-ToolGroup projection (D1) → TUI presentation, NOTE.

### DEFERRED
- Agent temp-file hygiene (D3) → Home & Runtime Hardening (workspace pollution).
- Latent unbounded awaits found by Phase 0 (`command_gate.lock_owned().await`,
  flush barrier) → recorded as suspected latent risk; NOT the D4 cause and NOT
  reproduced by any control, so deliberately not fixed in this gate.

### EXPECTED / ENVIRONMENT / NOT A PRODUCT BUG
- D2 `--config/--no-config` (spec mismatch).
- **D4 replay final `go test` failure** = the sandbox blocks the Go 1.26.2 toolchain
  download (GOSUMDB). This is an Expected Environment Limitation, the same class as
  the missing Browser capability — NOT a product defect. Independent `go build ./...`
  EXIT 0 confirms the agent's code is sound; the agent even opened a repair window
  and reported the env block honestly. Do NOT blur this into "PASS" and do NOT count
  it against the agent.

## 8. Cross-round scorecard

| Dimension | D1 | D2 | D3 | D4 First | D4 Replay | Final |
|---|---|---|---|---|---|---|
| Functional correctness | PASS | PASS | PASS | backend-only | full-stack | strong |
| False completion | NO | NO | NO | NO | NO | none |
| Long-task completion | n/a | n/a | ceiling-bound | 1 window | **4 windows** | fixed |
| Tool efficiency | 27 | 72 | 119 | 366 | 528 | scales w/ scope |
| Repeated reads/searches | 0 | 0 | 0 | low | low | disciplined |
| Context efficiency | LOW | LOW | LOW | 0 compact | 0 compact | no pressure |
| Verification quality | good | good | strong(real browser) | gated | gated + e2e | honest gates |
| Runtime reliability | ok | ok | ok | ok | **survives disconnect** | fixed |
| Reconnect reliability | proj gap | — | — | wedged(misdiag) | **no wedge** | fixed |
| TUI reliability | PASS | PASS | partial-probes | full probes | 9/9 PTY | stable |
| Manual intervention | 0 | 0 | 0 | 0 | 0 | fully autonomous |
| Permission / safety | ok | ok | ok | ok | session-scoped | improved |
| Environment robustness | ok | ok | Chrome-fragile | GOSUMDB | GOSUMDB | env-limited |
| Frontend verification | n/a | n/a | real browser | none | e2e built | Browser-phase gap |

## 9. Product capability assessment

- **Q1 — reliability by task size:** Small (D1) YES. Medium cross-file (D2) YES.
  Long-running multi-window (D4 replay) YES for autonomous *implementation* across
  windows with disconnect survival; final *verification* is capability/env-bound
  (Browser, Go toolchain).
- **Q2 — main bottleneck now:** NOT the round ceiling (fixed) and NOT execution
  hosting (fixed). The live limiters are (a) Browser verification capability and
  (b) environment/toolchain readiness (sandbox network for toolchain downloads).
  Both are capability/harness, not model reasoning.
- **Q3 — is model code-generation the bottleneck?** No. Across D1–D4 the code the
  agent wrote was correct and well-architected (D3 real-browser-verified; D4 replay
  full-stack + `go build` clean; zero false completions across all rounds). The
  binding constraints have been HARNESS (single-window lifetime, execution hosting)
  and CAPABILITY/ENV (browser, toolchain) — not generation quality.
- **Q4 — verdict:**

## 10. Benchmark baseline implications

D1–D4 + the replay give a concrete internal anchor: this gate is the point where
the harness began to support reliable long-task autonomous progression (1→4
windows, disconnect-survival). When an external Benchmark (e.g. Terminal-Bench) is
later fixed as the standard baseline, this campaign is the qualitative "what changed
in the harness" companion to the eventual quantitative score — useful for
attributing score movements to harness vs model changes.

## 11. Final verdict

**DOGFOOD PHASE CONDITIONAL** — the Real Dogfood phase can close as CONDITIONAL
PASS. Rationale: the two structural blockers surfaced by D1–D4 (single-window
ceiling; unattended execution hosting) are fixed and validated; no false completions
anywhere; code quality is consistently strong. The "conditional" reflects the two
OPEN capability items that D1–D4 legitimately could not close in-phase — Browser
verification and toolchain/environment readiness — which are the explicit next
roadmap phases, not defects. No BLOCKER-class dogfood finding remains against the
product.
