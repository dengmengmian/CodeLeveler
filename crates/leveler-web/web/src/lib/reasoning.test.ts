import { describe, expect, it } from 'vitest';
import {
  MAX_REASONING_LINES,
  capReasoning,
  reasoningLines,
  reasoningRemainderLabel,
  reasoningToggleLabel,
} from './reasoning';

describe('reasoning presentation', () => {
  it('hides when empty', () => {
    expect(reasoningLines('')).toEqual([]);
    expect(reasoningLines('   \n  \n')).toEqual([]);
  });

  it('drops blank lines like the TUI', () => {
    expect(reasoningLines('先看 workspace\n\n再读 README')).toEqual(['先看 workspace', '再读 README']);
  });

  it('caps expanded body at 24 lines and reports the remainder', () => {
    const lines = Array.from({ length: 30 }, (_, i) => `line ${i + 1}`);
    const { visible, remainder } = capReasoning(lines);
    expect(visible).toHaveLength(MAX_REASONING_LINES);
    expect(visible[0]).toBe('line 1');
    expect(visible[23]).toBe('line 24');
    expect(remainder).toBe(6);
  });

  it('does not cap a short thought', () => {
    const { visible, remainder } = capReasoning(['a', 'b']);
    expect(visible).toEqual(['a', 'b']);
    expect(remainder).toBe(0);
  });

  it('uses the TUI-aligned Chinese disclosure label', () => {
    expect(reasoningToggleLabel(4)).toBe('思考 · 4 行');
  });

  it('reports remainder in Chinese, not raw JSON', () => {
    expect(reasoningRemainderLabel(6)).toBe('… (+6 行)');
  });
});

