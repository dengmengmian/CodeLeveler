# CodeLeveler 1.0.20

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

The terminal's local shell becomes a complete surface: `!` opens a real Shell
Mode, every command keeps its full output, and a running command can move to
the background without losing its process. The user's own turn also settles
into one full-width background band.

## Shell Mode

- `!` on an empty composer is a mode marker, never command text. The mode has its own prompt and accent.
- Offline Tab completion covers files, directories and PATH executables. Ambiguous matches open a scrolling candidate menu driven by keyboard, pointer and wheel; accepting a candidate never executes it.
- Each command is one inline record with a status rail, a `$ command` line, stdout/stderr provenance, a live tail and a measured duration. A finished long result keeps its named fold and opens fully on demand.
- Stopping is available from both the record and the live activity row. `Ctrl+C` cancels exactly that execution; `Esc` and `Ctrl+B` never do.

## Complete output

- The complete stdout/stderr and the arrival-order combined stream are retained, not a lossy 1 MiB tail.
- `/output` opens the complete result in a windowed page; `/copy output` copies it cleanly. A large result costs the same per frame as a small one.
- Rows can be selected and copied with tabs preserved and soft wraps rejoined.

## Background handoff

- `Ctrl+B` hands a running command to the existing background registry with the same execution id, process and process group. Nothing is re-spawned or re-run: output, exit code and duration stay continuous.
- The transfer is durable. A cold restore reports a completed execution, or an orphan without a terminal fact as unknown — never as still running.

## Conversation band

- The user's own turn is one full-width background band. The dark theme uses the existing quiet `#171A1E` step above the `#111315` canvas; the light theme uses `#FFFFFF`, high contrast `#1A1A1A`.
- The soft-blue `❯` opens the first row, the body keeps primary ink, and short and long messages share the same left and right edges. The band is exactly as tall as the message; there is no identity label, border, divider or extra vertical padding.
- Selecting the band still copies the authored source. The background, marker, status rails and other painted glyphs never reach the clipboard.

## Fixes

- The background-wait row's marker animates with the shared 120 ms spinner while the wait target is running; reduced motion keeps it static.
