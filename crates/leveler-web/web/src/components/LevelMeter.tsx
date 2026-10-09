// 台阶式上下文用量表：5 级台阶 = COMPACTION 口径的上下文占用率。
//
// 数据来自 runtime 的 ContextAccounting（`context_usage` 事件 / `query_context`
// 回答）：projected 输入 / 有效输入容量，soft 阈值与 fold 状态同源，UI 不重建
// 任何一条策略。没有 accounting 时只把 provider 实际用量画在“模型窗口”轴上并
// 明确命名——绝不当作压缩口径。布局归本组件，数字归 `contextMeter`。

import { useAppState } from '../state/store';
import { contextMeter } from '../lib/contextMeter';

const BARS = 5;

export function LevelMeter() {
  const current = useAppState().current;
  const meter = contextMeter({
    accounting: current?.contextUsage ?? null,
    providerInput: current?.tokens.input ?? 0,
    providerOutput: current?.tokens.output ?? 0,
    modelWindow: current?.contextWindow ?? null,
  });
  if (meter === null) return null;

  const bars = Array.from({ length: BARS }, (_, i) => {
    const full = i + 1 <= Math.floor(meter.fill);
    const half = !full && meter.fill - i >= 0.5;
    const cls = full ? 'on' : half ? 'on half' : '';
    return <i key={i} className={cls || undefined} />;
  });

  return (
    <span className="levelmeter" title={meter.title}>
      {bars}
      <span className="levelmeter-label">{meter.label}</span>
    </span>
  );
}
