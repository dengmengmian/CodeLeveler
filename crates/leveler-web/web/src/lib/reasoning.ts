// Live model reasoning for Conversation. TUI parity
// (crates/leveler-tui/src/conversation/build.rs): empty → hidden,
// collapsed by default, expand body capped at 24 lines.

export const MAX_REASONING_LINES = 24;

export function reasoningLines(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trimEnd())
    .filter((line) => line.trim() !== '');
}

export function capReasoning(lines: readonly string[]): { visible: string[]; remainder: number } {
  if (lines.length <= MAX_REASONING_LINES) {
    return { visible: [...lines], remainder: 0 };
  }
  return {
    visible: lines.slice(0, MAX_REASONING_LINES),
    remainder: lines.length - MAX_REASONING_LINES,
  };
}

export function reasoningToggleLabel(n: number): string {
  return `思考 · ${n} 行`;
}

export function reasoningRemainderLabel(n: number): string {
  return `… (+${n} 行)`;
}

