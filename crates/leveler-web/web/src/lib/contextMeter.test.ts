// Compaction utilization is the RUNTIME's ContextAccounting, on every surface.
//
// These tests pin the Web meter's model: projected input over the effective
// input capacity, the soft threshold and fold state taken from the snapshot,
// and the model-window axis only as its OWN named metric. Provider actual
// usage is a different measurement and must never be labelled compaction.

import { describe, expect, it } from 'vitest';
import { contextMeter } from './contextMeter';
import type { ContextAccounting } from '../types/protocol';

function accounting(over: Partial<ContextAccounting> = {}): ContextAccounting {
  return {
    model: { provider: 'mock', model: 'a' },
    context_window_tokens: 128_000,
    compact_at_tokens: 91_200,
    output_reservation_tokens: 32_000,
    headroom_tokens: 0,
    input_capacity_tokens: 96_000,
    fold_state: 'none',
    used_tokens: 10_000,
    free_tokens: 118_000,
    token_count_kind: 'estimated',
    pressure: 'normal',
    categories: [],
    last_compaction: null,
    reasoning_projection: null,
    ...over,
  };
}

const base = { providerInput: 0, providerOutput: 0, modelWindow: 128_000 };

describe('context meter', () => {
  it('compaction utilization is projected input over the effective input capacity', () => {
    const m = contextMeter({ ...base, accounting: accounting() });
    expect(m?.label).toBe('10.0k / 96.0k');
    expect(m?.fill).toBeCloseTo((10_000 / 96_000) * 5, 5);
    expect(m?.title).toContain('压缩口径');
    expect(m?.title).toContain('有效输入容量');
  });

  it('never shows provider input+output over the window as compaction', () => {
    const m = contextMeter({
      ...base,
      accounting: accounting(),
      providerInput: 40_000,
      providerOutput: 8_000,
    });
    expect(m?.label).toBe('10.0k / 96.0k');
    // The provider's own number may be named beside it, never blended in.
    expect(m?.title).toContain('provider 实际输入');
    expect(m?.title).not.toContain('48.0k');
  });

  it('takes the soft threshold from the snapshot, not a re-derived percentage', () => {
    const m = contextMeter({ ...base, accounting: accounting({ compact_at_tokens: 60_000 }) });
    expect(m?.title).toContain('60.0k');
  });

  it('with no capacity the model window is its own named axis', () => {
    const m = contextMeter({ ...base, accounting: accounting({ input_capacity_tokens: null }) });
    expect(m?.label).toBe('10.0k / 128.0k');
    expect(m?.title).toContain('模型窗口占用');
    expect(m?.title).not.toContain('压缩口径');
  });

  it('without an accounting the provider usage is explicitly the window axis', () => {
    const m = contextMeter({
      ...base,
      accounting: null,
      providerInput: 41_181,
      providerOutput: 500,
    });
    expect(m?.label).toContain('窗口');
    expect(m?.label).toContain('41.7k');
    expect(m?.title).toContain('provider 实际用量');
    expect(m?.title).toContain('压缩口径统计不可用');
  });

  it('is hidden when no usage and no accounting are known', () => {
    expect(
      contextMeter({ accounting: null, providerInput: 0, providerOutput: 0, modelWindow: null }),
    ).toBeNull();
  });

  it('a zero-token accounting is not a gauge', () => {
    expect(contextMeter({ ...base, accounting: accounting({ used_tokens: 0 }) })).toBeNull();
  });
});
