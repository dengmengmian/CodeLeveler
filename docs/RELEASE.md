# CodeLeveler 1.0.14

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This patch turns the terminal into a primitives-first editor surface — input
editing, history and help search, local terminal diagnostics and an
evidence-bearing clipboard route — and reworks transcript presentation so it
stops repeating what the conversation already shows.
Web and Desktop are outside this release's qualification scope.

## Terminal

- The composer has a bounded edit history. Ctrl+Z undoes, Ctrl+Shift+Z / Alt+Z redoes, and Ctrl+Y restores recently killed text. A mixed paste, its file references and its image attachment bindings are restored together, and submitting or changing sessions ends the draft's history. Each stack holds at most 100 entries and 8 MiB.
- Ctrl+R searches submitted input history. Enter puts the selected entry back in the draft for editing without sending it; Esc or Ctrl+C closes the search and keeps the original draft.
- `/help` is searchable. Typing filters commands, shortcuts and descriptions, ↑/↓ select, and the selected row stays reachable at narrow widths.
- `/doctor` opens local terminal diagnostics, and `leveler doctor` prints the same observations: terminal, SSH/tmux, color hints, newline keys and clipboard route. Diagnostics neither read nor write the clipboard and never change terminal settings; a capability without evidence stays unknown.
- Copying selected conversation text prefers the native clipboard with readback verification, only on a local host outside tmux. SSH skips the remote native clipboard, and tmux skips native writes as well because an inherited server environment cannot establish the current attached client; both write the tmux paste buffer and send OSC 52 instead. A buffer write or forwarded sequence is reported as sent with local delivery unverified, and an OSC 52 payload above 64 KiB is refused explicitly.
- A command keeps its full identity in the conversation, where it expands to the complete argument text wrapped to terminal width; the status strip reports the phase and the command's own elapsed instead of repeating the command. A command waiting without output says so.
- The Background Tasks page keeps its task list above the detail pane: identity and stop controls stay fixed while only command/log content scrolls, mouse clicks select tasks, the wheel follows the region under the pointer, and each task retains its own scroll and follow state.

## Presentation

- The footer context bar is presentation only. Percentage, denominator, bar length and color come from the runtime projection; provider usage is only the no-snapshot fallback, and a snapshot whose window is unknown has no gauge. The eight-cell bar uses info below 70%, warning at 70–89% and error at 90% or above. Narrow rows drop the bar, then the cache, then the capacity chips, while the clock and the permission / collaboration controls retain priority.
- `/context` names input capacity, model window and output reservation separately. Compaction policy and thresholds stay on the input-capacity axis, while the runtime snapshot's `free_tokens` remains model-window free; the inspector derives its grid and its free remainder from the axis it selected.
- A running command's status reports the phase and the command's own elapsed, so the command name is stated once, in its conversation row.
- Thoughts use bounded previews. A live Thought shows its header and at most the latest three wrapped body rows; a completed standalone Thought occupies no history row until it is explicitly opened, and settled Thoughts with blank bodies stay hidden. Manually expanded Thoughts stay open through later events, and Ctrl+O opens the latest tool group or, without tools, the latest Thought.
- Exploration receipts fold independently at each level: opening a receipt restores tool headers in real chronology without showing completed Thoughts, closing a parent hides every child while retaining explicit reader choices, and each tool reuses its own expanded state.
- Real transcripts start at the viewport top, so streaming and Thought folding never move the first message; only the empty-session splash is centered. Manual folding, and a runtime completion or interruption that folds a Thought or regroups receipts, keep the line at the viewport's top edge in place.
- Command details remain reachable in place, and background commands settle their row when the process exits.

## Validation and limits

The editing, search, diagnostics and clipboard paths carry deterministic coverage, including bounded history, grapheme-safe restore and the routing receipt. Transcript presentation is covered across resize, history replay and PTY-driven dogfood fixtures, and the context-statistics fixture corpus pins the new footer axis.

Clipboard route evidence is deliberately narrow: only a native write followed by matching readback on a local, non-tmux host is verified, tmux configuration is never silently changed, and DCS passthrough support remains unknown. Physical terminal compositor flicker and real remote-terminal clipboard delivery are not qualified by these tests.
