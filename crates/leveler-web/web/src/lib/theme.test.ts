import { describe, expect, it } from 'vitest';
import { THEME_OPTIONS } from './theme';

describe('theme identity', () => {
  it('keeps Paper / Graphite / Midnight as three distinct roles', () => {
    const byChoice = Object.fromEntries(THEME_OPTIONS.map((t) => [t.choice, t]));
    expect(byChoice.graphite.desc).toMatch(/炭黑|中性/);
    expect(byChoice.graphite.desc).not.toMatch(/蓝/);
    expect(byChoice.midnight.desc).toMatch(/蓝黑/);
    expect(byChoice.paper.desc).toMatch(/浅色|中性/);
  });
});
