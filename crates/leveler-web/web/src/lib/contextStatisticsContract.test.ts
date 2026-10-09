// Cross-surface contract for context statistics — Web conformance.
//
// Reads the ONE corpus the terminal, Web and desktop share
// (`testdata/context_statistics/v1/`) and drives the real meter model. The
// expectation is semantic: compaction utilization is the runtime's projected
// input over its effective input capacity, the soft threshold and hard capacity
// are the runtime's own numbers, the fold state is the classifier's, and the
// model-window axis is used only when no capacity was resolved — never as a
// compaction percentage, and never with provider actual usage spliced in.

import { describe, expect, it } from 'vitest';
import { contextMeter } from './contextMeter';
import { formatTokens } from './format';
import type { ContextAccounting } from '../types/protocol';

import cs1 from '../../../testdata/context_statistics/v1/CS1.json?raw';
import cs2 from '../../../testdata/context_statistics/v1/CS2.json?raw';
import cs3 from '../../../testdata/context_statistics/v1/CS3.json?raw';
import cs4 from '../../../testdata/context_statistics/v1/CS4.json?raw';
import cs5 from '../../../testdata/context_statistics/v1/CS5.json?raw';
import cs6 from '../../../testdata/context_statistics/v1/CS6.json?raw';
import cs7 from '../../../testdata/context_statistics/v1/CS7.json?raw';
import cs8 from '../../../testdata/context_statistics/v1/CS8.json?raw';
import cs9 from '../../../testdata/context_statistics/v1/CS9.json?raw';

interface Render {
  web_label?: string | null;
  web_axis_word?: string | null;
  web_names_provider?: boolean;
  web_unavailable_note?: boolean;
}

interface Case {
  id: string;
  invariant: string;
  accounting: ContextAccounting | null;
  provider: { input: number; output: number };
  model_window: number | null;
  expect: {
    axis: string;
    used_tokens: number | null;
    input_capacity_tokens: number | null;
    compact_at_tokens: number | null;
    hard_capacity: number | null;
    fold_state: string | null;
    provider_actual: number | null;
    render: Render;
  };
}

const cases: Case[] = [cs1, cs2, cs3, cs4, cs5, cs6, cs7, cs8, cs9].map(
  (raw) => JSON.parse(raw) as Case,
);

describe('context statistics contract v1 (web)', () => {
  it('has a non-empty corpus with unique ids', () => {
    expect(cases.length).toBeGreaterThan(0);
    expect(new Set(cases.map((c) => c.id)).size).toBe(cases.length);
  });

  for (const testCase of cases) {
    it(`${testCase.id} — ${testCase.invariant}`, () => {
      const meter = contextMeter({
        accounting: testCase.accounting,
        providerInput: testCase.provider.input,
        providerOutput: testCase.provider.output,
        modelWindow: testCase.model_window,
      });
      const render = testCase.expect.render;

      if (render.web_label == null) {
        expect(meter, `${testCase.id}: the meter must be shown by the provider number`).not.toBeNull();
        return;
      }
      expect(meter).not.toBeNull();
      expect(meter?.label).toBe(render.web_label);
      if (render.web_axis_word) {
        expect(meter?.title, `${testCase.id}: axis name`).toContain(render.web_axis_word);
      }

      if (testCase.expect.axis === 'compaction') {
        // The runtime's projected input and effective capacity: the same
        // numbers on every surface, never the window and never provider usage.
        expect(meter?.title).toContain('有效输入容量');
        expect(meter?.title).not.toContain('模型窗口占用');
        const used = testCase.expect.used_tokens;
        const capacity = testCase.expect.input_capacity_tokens;
        if (used != null && capacity != null) {
          expect(meter?.label).toBe(`${formatTokens(used)} / ${formatTokens(capacity)}`);
        }
        if (testCase.expect.compact_at_tokens != null) {
          expect(meter?.title).toContain(formatTokens(testCase.expect.compact_at_tokens));
        }
      } else if (testCase.expect.axis === 'window' || testCase.expect.axis === 'window_provider') {
        // No capacity resolved: the model window is its own named axis.
        expect(meter?.title).toContain('模型窗口占用');
        expect(meter?.title).not.toContain('有效输入容量');
      } else {
        expect(meter?.title).not.toContain('有效输入容量');
        expect(meter?.title).not.toContain('模型窗口占用');
      }
      if (render.web_names_provider) {
        expect(meter?.title).toContain('provider 实际输入');
        expect(meter?.title).toContain(
          `${testCase.provider.input.toLocaleString()} tokens`,
        );
      }
      if (render.web_unavailable_note) {
        expect(meter?.title).toContain('压缩口径统计不可用');
        expect(meter?.title).toContain('provider 实际用量');
      }
    });
  }
});
