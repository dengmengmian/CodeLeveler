# Multi-Agent MA3 — Product UX Closure

Status: **PASS** (CI §6). Scope from
`docs/MULTI_AGENT_MA0_REALITY_AND_GAP_AUDIT.md` §6.3 / §9 (REAL_MA3_SCOPE).
Base: MA2 PASS at `8cc63b6`. Mobile is MA5.

## 1. Contract

Clients render a child from the runtime's typed facts, never from prose or
from their own clock:

| Fact | Source | Carried by |
|---|---|---|
| lifecycle state `running \| interrupted \| settled` | `SubAgentStarted` / `SubAgentInterrupted` / `SubAgentResumed` / `SubAgentFinished` | live `SubAgentUpdated`, `SubAgentStateChanged`; snapshot `UiSessionSnapshot.children` |
| outcome and stop | MA1 typed terminal (`ChildStatus`, `ChildStop`) | `SubAgentUpdated.outcome/stop`, `UiChildAgent` |
| profile, read-only, background, scope, resumes, tokens, cost | durable child record, `model_requests` | `UiChildAgent`; `SubAgentUpdated` at start |
| runtime notice (settled / re-delivered / resumed / lost) | exact registered first line (`RUNTIME_NOTICE_HEADERS`) | `UiMessage.kind = runtime_notice` |
| stop one child | `ClientCommand::CancelChild{child_id}` (G8, MA1) | TUI `x`, Web 取消 |

A terminal is final: no live event or snapshot moves a settled child back to
running. Without a live turn an open child is reported `interrupted`.

## 2. Changes

| Defect (MA0 §6.3) | Change | Commit |
|---|---|---|
| U3 notices render as user input after reload | `UiMessageKind::RuntimeNotice`, classified by exact header; TUI renders a note, Web a collapsed runtime note; a test pins every notice to a registered header | `c65ba21`, `e18dd3a`, `0f5f3ac` |
| U5 reconnect loses / keeps stale children | `UiSessionSnapshot.children` from the durable record; TUI and Web restore open children from it | `c65ba21`, `e18dd3a`, `0f5f3ac` |
| U1 duplicate start entries (TUI) | `spawn_agent` is `Silent`: an accepted spawn is one child block; a refused spawn still shows (Failed is visible under Silent) | `e18dd3a` |
| U2 settled team stuck on the surface | idle clock reset no longer saturates the settle window; team cleared on session switch | `e18dd3a` |
| U4 UI-derived child state | TUI wording from typed `stop` (prose detector deleted); a child with no terminal at turn end reads "no terminal received", not Failed; Web late terminal no longer reopens a finished turn; observability lists interrupted as interrupted | `e18dd3a`, `0f5f3ac`, `6fc0ad6` |
| Web children | Inspector labels from typed facts ("部分结果 · 预算耗尽", "无结果 · 已取消", "已中断"), bounds, tokens, cost, profile, read-only; interrupted children listed with running ones under "Open" | `0f5f3ac`, `6fc0ad6` |
| Cancel one child | TUI Activity detail `x` (only while running/waiting); Web 取消 on a running child; parent turn continues | `32d0bc9`, `0f5f3ac` |
| U6 background children absent from the wait line | **Not a defect.** The wait line names what the parent is blocked on; a background child does not block it. It is visible in the live child line and the team surface while the parent works (frame at 3.2 s in §3.1: `● Euclid · Read internal/sink/sink.go …` beside `● 主 Agent 正在工作`). No change. | — |

Schemas and `protocol.gen.ts` regenerated in `c65ba21`.

## 3. Real acceptance

Release binary built at the MA3 head, `deepseek/deepseek-v4-flash`, isolated
`LEVELER_HOME`, fresh `navsvc` fixture copy. Driver scripts read the real
screen (PTY + `pyte`) and the real browser (Playwright over installed
Chrome); they are not committed.

### 3.1 TUI, background child (`leveler tui --in-process`)

Task: one background explorer; the parent reads README.md meanwhile and
answers from the settled report. Session `6fb310d5…`.

| Check | Result |
|---|---|
| child visible while running | 35 frames |
| frames with a separate `spawn_agent` cell next to the child | **0** |
| task completed | yes, answer names `For` from the child's report |
| settled team leaves the surface after idle | yes |
| durable record | 1 settlement notice in transcript, 1 child terminal |
| reopened with `--session`: notice visible | yes, as `◆ ## Background sub-agent settled` note |
| reopened: notice rendered as a user turn | **no** |

### 3.2 TUI, foreground child

Same fixture, `run_in_background=false`. Session `0b1c0afe…`. One child
block (`✓ Euclid · 已完成 · 1 项发现`), no `spawn_agent` cell in any frame,
task completed in 10 s, 1 child terminal. This also closes the review's
unverified question about a foreground spawn leaving a failed tool cell: an
admitted spawn emits no tool-call event (it is deferred to the child batch
before the generic `ToolCall` emission in `drive.rs`), so there is no cell to
finalize as failed.

### 3.3 Web (`leveler web`, real browser)

The session from §3.1 opened in the Web UI:

| Check | Result |
|---|---|
| `.runtime-notice` elements | 1, titled "▸ Background sub-agent settled", collapsed |
| user turns containing the notice | **0** |

Web cancel and the Inspector labels are covered by vitest
(`lib/controller.test.ts`, `lib/inspectorModel.test.ts`, `state/store`
tests), not by a live cancel in the browser.

## 4. Independent review

A fresh-context review of `8cc63b6..32d0bc9` found no high-severity issue and
no wider remote exposure (the tunnel already forwarded every field the
snapshot now carries). Four medium and six low findings, all fixed test-first
in `6fc0ad6`:

| # | Finding | Fix |
|---|---|---|
| M1 | TUI: a stale snapshot applied after a child's terminal reopened it | restore never regresses a settled child |
| M2 | Web: same race replaced a settled child; a child started after the snapshot was dropped | merge over the snapshot; keep unseen open children |
| M3 | Web: a new user turn cleared an interrupted child that the turn then resumed | clear only settled children; `sub_agent_state_changed` refreshes observability |
| M4 | TUI: hiding the spawn cell turned the prose before a spawn into a second final answer | a child start counts as work acting on that prose |
| L1 | a row settled before `outcome` was typed restored as failed | `UiChildAgent.ok` |
| L2 | a terminal claimed `background=false` | `SubAgentUpdated.background: Option<bool>` |
| L3 | a child with no terminal read "interrupted" and offered `x` | "no terminal received", no `x` |
| L4 | switching session flooded the team with settled history | only open children join the live team |
| L5 | snapshot hard-fails on a corrupt child row | kept on purpose, commented: a guessed child list would be a false fact |
| L6 | interrupted children under a "Running" heading | heading "Open" |

## 5. Residuals

- Snapshot child usage aggregates `model_requests` rows in memory; a long
  session pays this on open / reconnect / resync only. A `GROUP BY agent_id`
  query is the fix if it shows up.
- `children` is unbounded and includes settled history after compaction; the
  remote device receives it. Low impact; a cap or open-only remote view is a
  later choice.
- `turn_live` assumes one runtime process per session. A second process
  writing the same session (for example a concurrent `leveler run`) would
  show its running children as interrupted.
- No live browser cancel was exercised; Web cancel is unit-tested.

## 6. CI and gates

Exact main CI on `6fc0ad6` (the MA3 code head), run `34809805167`,
attempt 1: Linux, macOS and Windows `fmt · clippy · test` success; web
UI contract · typecheck · test · build success; mobile analyze · test
success; deny · audit success.

```text
U1_DUPLICATE_ENTRIES=FIXED
U2_STUCK_COLLABORATION=FIXED
U3_NOTICE_AS_USER=FIXED
U4_UI_DERIVED_STATE=FIXED
U5_RECONNECT_CHILDREN=FIXED
U6_WAIT_LINE=NOT_A_DEFECT
TUI_MULTI_AGENT=PASS
WEB_MULTI_AGENT=PASS
CANCEL_ONE_CHILD=PASS
BACKGROUND_FIRST_UX=PASS

MA3_PRODUCT_UX=PASS
```
