# CodeLeveler 1.0.11

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This release makes the terminal's execution presentation a frozen contract with
one authority, and gives a session one axis on every transport. The terminal,
the Web client, the Desktop client and the mobile App now derive the same
semantic tree from the same runtime facts — the execution round, a truthful
round status, and the answer a turn actually committed — and the work /
bookkeeping classification behind that answer lives once on the wire instead of
being copied into four renderers.
It also aligns Auto permission with ordinary development: temporary files,
process and system inspection, relocated read-only Git and ordinary network use
run without asking, while the destructive operations still do.
A session opened interactively is a Chat session, whether it runs over the
daemon socket or in-process, and `leveler run` keeps driving a Goal.

## Added

- **The execution presentation contract, frozen as fixtures.** The semantic
  tree a turn projects to — optional assistant text, execution rounds with their
  membership and truthful statuses, an optional final answer, the turn terminal
  — is pinned by a language-neutral corpus in
  `testdata/execution_presentation/v1/` (C1..C14), and every surface checks
  itself against that same corpus: the terminal (the reference implementation,
  driven through the real reducer), the Web client, the Desktop client and the
  mobile App. A fixture may declare a live path, a reconnect snapshot and a
  durable-history replay; every declared path must project to the same tree.
- **Tool rows grouped by the real execution round.** The terminal groups a
  model response's calls under one round head instead of a flat list, keeps the
  observed concurrent burst as a batch inside the round, and labels the stage
  while a round is running.
- **A truthful round head.** "All ok" is only claimed when every visible call
  succeeded; a settled round with a cancelled, failed or unknown call says so,
  and a prose-free turn ends `no_final_answer` rather than a green "completed".
- **One answer lifecycle.** A committed answer survives bookkeeping
  (`update_plan`, `update_goal(complete)`), while real read / search / edit /
  shell work after it demotes the answer to progress. The classification has a
  single owner (`leveler_tools::acts_on_answer`) and reaches every surface as a
  stated wire fact (`answer_effect`); a surface never re-derives it from the
  tool name, and an unrecognized tool is treated as work.
- **`/btw` side questions have their own surface.** A side question's
  read-only tool activity is reported in the side surface and never becomes part
  of the main turn's transcript, its answer or its plan.
- **Auto permission covers ordinary development.** Writing temporary files
  (`/tmp`, `$TMPDIR`) no longer fails with `EPERM`; process and system
  inspection runs; a relocated read-only Git invocation (`git -C <dir> status`,
  `--git-dir`, `--work-tree`) stays a read instead of escalating to ASK; and
  Auto's ordinary network ALLOW reaches the sandbox, with a permission DENY
  still failing as the negative control. Destructive operations still ask.
- The Web client and the mobile App show the desktop-style round tree,
  including the command's own failure reason with the runtime's execution rows
  removed.

## Changed

- **`--permission` has no default value.** Omitted, a new session uses the
  project/default profile and a resumed session keeps the profile it persisted;
  supplied, it is an explicit override. Previously `assisted` was assumed when
  the flag was absent, which also meant a resume could not tell "unchanged" from
  "set to assisted".
- **An interactive session is a Chat session on every transport.**
  `leveler` and `leveler tui` used to record `chat` over the daemon socket but
  `goal` in-process, because the embedded path resolved the axis from the
  process default. The interactive axis is now stated once and both transports
  write it; `leveler run` keeps the Goal default, and resume keeps the axis the
  session was created with.
- **The client protocol is at minor 14** (major 1, unchanged). The additions are
  optional: a peer that omits them keeps its previous behaviour, and the Web,
  Desktop and App clients are unchanged in how they connect.
- The narrow composer chip reserves collaboration and permission before it
  shortens the model, so the axis and the permission mode are never the first
  things to disappear.
- Interim narration is visually subordinate to the answer, and a mixed-tool
  round gets one simplified label instead of a per-kind list.
- Raw model reasoning is no longer rendered as transcript content on the Web,
  Desktop or App surfaces; the live status still says the model is thinking.

## Fixed

- **A reconnect lost the running tool's execution round.** The live view
  dropped the round identity on its way into the reconnect snapshot, so a
  reconnected client re-guessed the round from tool kinds and timing and could
  weld a second round onto the first. The snapshot now states the round the
  runtime already knew.
- **A command's failure reason could be a runtime note.** A preview mixes what
  the command printed with the rows the runtime writes (`exit: N`, stream
  headers, `[execution policy] …`, timeouts). Those rows say how a command ran,
  so reading one as why it failed named the sandbox's confinement note as the
  reason a `git grep` with no match exited 1, and counted the note as output.
  They are now classified once and excluded from the reason, the expanded body
  and the output count; a timeout is stated as a timeout.
- A busy row wider than the status strip collapsed to a bare spinner and
  silently discarded its elapsed, tool and token parts.
- Prefixed text (`※ 回顾:`) was wrapped by character count, clipping the wide
  glyph at the end of the first line on a narrow terminal.
- The narrow status chip dropped collaboration and permission before it dropped
  the model, hiding the two controls that outrank it.
- **A protocol repair could reappear as a user message.** The goal closeout
  still reaches the model on resume, but replayed history and the session
  snapshot no longer project it as user-authored text; live clients already hid
  it.
- Approval state could outlive the profile that asked for it: pending approvals
  are now superseded when the permission profile changes, and the selected mode
  survives a resume.
- A completed user shell's output tail was dropped when live delivery dropped
  (mobile App).
- Closeout folding could fold a message that belonged to an earlier model
  round; it is now bounded to its own round.
- A runtime-host revival race could spawn a second runtime instead of adopting
  the one already serving.
- `leveler update` retried only a single transient exec failure while
  validating a downloaded release.

## Known limits

- Prebuilt binaries are not Apple- or Microsoft-signed. macOS or Windows may
  block the first run until you allow it
- Windows cannot deny network access per command. A command that requires that
  isolation is refused rather than run with the network open
- Windows has no local daemon socket. Sessions and `resume` still work
- A confined Windows `!command` prints its output when it finishes, not while
  it runs
- Durable-history replay is still incomplete on the non-terminal surfaces: the
  Web client and the App have no `query_session_history` consumer, and the
  Desktop client cannot rebuild a turn from history alone. The conformance tests
  assert each gap instead of hiding it

Install and usage: [README](../README.md).
Updates: [README § Updates](../README.md#updates).
How the system is layered: [Architecture](ARCHITECTURE.md).
