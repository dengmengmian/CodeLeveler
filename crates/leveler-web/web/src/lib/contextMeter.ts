// The context level meter's model.
//
// Compaction utilization is the RUNTIME's `ContextAccounting`: projected input
// (`used_tokens`) over the effective input capacity (`input_capacity_tokens`),
// with the soft threshold and fold state taken from the same snapshot. The
// meter must not derive a second policy: provider `input + output` over the
// model window is a DIFFERENT metric (actual usage, window axis) and is only
// shown, named, when the runtime has no accounting.
//
// Kept as a pure function so the numbers are testable without a DOM, and so
// the component is only layout.

import type { ContextAccounting } from '../types/protocol';
import { formatTokens } from './format';

const BARS = 5;

export interface ContextMeterModel {
  /** 0..BARS fraction the bars are filled with. */
  fill: number;
  /** The compact label beside the bars. */
  label: string;
  /** Hover text. Always names the axis AND the usage source. */
  title: string;
}

export interface ContextMeterInput {
  /** The runtime's accounting of the session's next request, when it has one. */
  accounting: ContextAccounting | null;
  /** Provider-reported prompt tokens of the last request (actual usage). */
  providerInput: number;
  /** Provider-reported completion tokens of the last request (actual usage). */
  providerOutput: number;
  /** Declared model window from SessionBootstrap; null when unknown. */
  modelWindow: number | null;
}

/** Percent of `part` in `whole`; null with no denominator, never clamped up
 *  to 100 (an over-capacity request really is over 100%). */
function percent(part: number, whole: number | null | undefined): number | null {
  if (whole === null || whole === undefined || !(whole > 0) || !(part > 0)) return null;
  return (part / whole) * 100;
}

function fillOf(pct: number | null): number {
  if (pct === null) return 0;
  return Math.max(0, Math.min(BARS, (pct / 100) * BARS));
}

export function contextMeter(input: ContextMeterInput): ContextMeterModel | null {
  const { accounting } = input;
  const providerNote =
    input.providerInput > 0
      ? ` · provider 实际输入 ${input.providerInput.toLocaleString()} tokens`
      : '';

  if (accounting) {
    const used = accounting.used_tokens;
    if (!(used > 0)) return null;
    const capacity = accounting.input_capacity_tokens ?? null;
    const window_ = accounting.context_window_tokens ?? null;
    // Compaction utilization: projected input over the effective input capacity.
    if (capacity !== null && capacity > 0) {
      const pct = percent(used, capacity) ?? 0;
      return {
        fill: fillOf(pct),
        label: `${formatTokens(used)} / ${formatTokens(capacity)}`,
        title:
          `压缩口径 ${Math.round(pct)}%（projected ${used.toLocaleString()} / ` +
          `有效输入容量 ${capacity.toLocaleString()} tokens）` +
          (accounting.compact_at_tokens != null
            ? ` · 软压缩阈值 ${formatTokens(accounting.compact_at_tokens)}`
            : '') +
          providerNote,
      };
    }
    // No capacity resolved: the model window is its OWN named axis.
    if (window_ !== null && window_ > 0) {
      const pct = percent(used, window_) ?? 0;
      return {
        fill: fillOf(pct),
        label: `${formatTokens(used)} / ${formatTokens(window_)}`,
        title:
          `模型窗口占用 ${Math.round(pct)}%（projected ${used.toLocaleString()} / ` +
          `模型窗口 ${window_.toLocaleString()} tokens；压缩容量未解析）${providerNote}`,
      };
    }
    return {
      fill: 0,
      label: formatTokens(used),
      title: `projected 输入 ${used.toLocaleString()} tokens（窗口与容量未解析）${providerNote}`,
    };
  }

  // No runtime accounting: provider ACTUAL usage over the declared window, on
  // the model-window axis, named as such. Never presented as compaction.
  const used = input.providerInput + input.providerOutput;
  if (!(used > 0)) return null;
  const window_ = input.modelWindow ?? null;
  if (window_ !== null && window_ > 0) {
    const pct = percent(used, window_) ?? 0;
    return {
      fill: fillOf(pct),
      label: `窗口 ${formatTokens(used)} / ${formatTokens(window_)}`,
      title:
        `模型窗口占用 ${Math.round(pct)}%（provider 实际用量 ${used.toLocaleString()} / ` +
        `模型窗口 ${window_.toLocaleString()} tokens；压缩口径统计不可用）`,
    };
  }
  return {
    fill: 0,
    label: formatTokens(used),
    title: `provider 实际用量 ${used.toLocaleString()} tokens（模型窗口未知；压缩口径统计不可用）`,
  };
}
