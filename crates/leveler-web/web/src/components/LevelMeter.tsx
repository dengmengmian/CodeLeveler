// 台阶式上下文用量表：5 级台阶 = 上下文窗口占用率。
// 数据来自 token_usage 事件 + SessionBootstrap.context_window；
// 有窗口大小时显示 "13k / 56k"，否则只显示已用 token；悬停显示百分比。

import { useAppState } from '../state/store';
import { formatTokens } from '../lib/format';

const BARS = 5;

export function LevelMeter() {
  const current = useAppState().current;
  const used = (current?.tokens.input ?? 0) + (current?.tokens.output ?? 0);
  const window_ = current?.contextWindow ?? null;

  let pct = 0;
  if (window_ && window_ > 0) {
    pct = Math.max(0, Math.min(100, (used / window_) * 100));
  }
  const filled = (pct / 100) * BARS;

  const bars = Array.from({ length: BARS }, (_, i) => {
    const full = i + 1 <= Math.floor(filled);
    const half = !full && filled - i >= 0.5;
    const cls = full ? 'on' : half ? 'on half' : '';
    return <i key={i} className={cls || undefined} />;
  });

  const label = window_ ? `${formatTokens(used)} / ${formatTokens(window_)}` : formatTokens(used);

  return (
    <span className="levelmeter" title={`上下文用量 ${Math.round(pct)}%（${used.toLocaleString()} tokens）`}>
      {bars}
      <span className="levelmeter-label">{label}</span>
    </span>
  );
}
