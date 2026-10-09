# CodeLeveler 1.0.13

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

This patch focuses on CLI/TUI and Agent Core reliability and terminal performance.
Web and Desktop are outside this release's qualification scope.

## Terminal

- Code blocks remain fully accessible through transcript scrolling, including long blocks, resize and resumed history. Tool output is reachable from the keyboard, and Ctrl+O expands the visible exploration receipt rather than only its hidden members.
- Streaming syntax highlighting reuses completed-line Syntect state. An unfinished line is highlighted again from its saved state, preserving cross-line syntax and the complete code text.
- The bounded highlight cache retains 640 entries instead of 512, addressing the measured 574-block history workload. The existing source-byte budget and LRU eviction remain in place.
- Failed turns and durable cancellation are shown as terminal outcomes. Repeated cancellation waits instead of promising a force operation.

## Runtime and context

- Interactive command acknowledgements follow durable application; cancellation is persisted before acknowledgement and reconciled after interruption.
- Resuming a Goal recovers interrupted lineage from fenced durable turn facts, and ordinary continuation goes through the existing runtime owner.
- Soft compaction uses a percentage of the effective input capacity, with a 95% default. Terminal context utilization follows runtime accounting on resume and after policy changes.

## Validation and limits

Full-code visibility has regression coverage across resize and history replay. Controlled Agent fixtures now verify Goal closeout and distinguish continuity from a completed task. The independent performance comparison includes matching code fixtures, terminal sizes, real PTY runs and a fresh 600-second stress test.

Incremental highlighting reduces repeated work during streaming; it does not remove whole-document Markdown parsing. A cold 1MB syntax highlight still costs approximately one second on the measured machine. Physical terminal compositor flicker is not qualified by the PTY tests.
