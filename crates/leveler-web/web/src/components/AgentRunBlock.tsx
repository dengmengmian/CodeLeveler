// 对话内 Agent 运行状态块：在最后一条消息下方展示当前回合的实时状态
// （阶段 + 耗时 + 当前动作 + 停止 + 查看过程），终态转为完成/失败/取消。
// 状态由 deriveRunState 从现有会话视图派生；顶部全局状态栏继续保留。

import { useEffect, useState } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { deriveRunState, type AgentRunState } from '../lib/runstate';
import { ToolCallRow } from './ToolCallRow';

/** 每秒重渲染以刷新耗时；active=false 时停走。 */
export function useElapsedSeconds(startedAt: number | null, active: boolean): number {
  const [, tick] = useState(0);
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, [active]);
  return startedAt ? Math.max(0, Math.floor((Date.now() - startedAt) / 1000)) : 0;
}

export function formatElapsed(sec: number): string {
  const m = Math.floor(sec / 60);
  const s = sec % 60;
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

/** 需要转圈动画的进行中状态。 */
const SPINNING: ReadonlySet<AgentRunState> = new Set([
  'queued',
  'thinking',
  'planning',
  'searching',
  'reading',
  'tool_running',
  'generating',
]);

export function AgentRunBlock() {
  const current = useAppState().current;
  const bridge = useBridge();
  const [showProcess, setShowProcess] = useState(false);

  const run = current ? deriveRunState(current) : null;
  const elapsed = useElapsedSeconds(current?.turnStartedAt ?? null, current?.turnActive ?? false);

  if (!current || !run) return null;

  const spinning = SPINNING.has(run.state);
  const glyph =
    run.state === 'waiting_approval'
      ? '⏸'
      : run.state === 'failed'
        ? '✕'
        : run.state === 'cancelled'
          ? '■'
          : run.state === 'completed'
            ? '✓'
            : null;

  return (
    <div className={`agent-run r-${run.state}`}>
      <div className="ar-head">
        <span className="ar-icon">{spinning ? <span className="ar-spin" /> : glyph}</span>
        <span className="ar-primary">{run.primary}</span>
        {current.turnActive && !run.terminal && <span className="ar-time">{formatElapsed(elapsed)}</span>}
        {current.turnActive && (
          <button className="ar-stop" title="取消当前回合 (Esc)" onClick={() => bridge.cancelTurn()}>
            停止
          </button>
        )}
      </div>

      {run.detail && <div className="ar-detail">{run.detail}</div>}

      {current.tools.length > 0 && !run.terminal && (
        <div className="ar-process">
          <button className="ar-toggle" onClick={() => setShowProcess((v) => !v)}>
            {showProcess ? '收起过程' : `查看过程 · ${current.tools.length}`}
          </button>
          {showProcess && (
            <div className="tools">
              {current.tools.map((t) => (
                <ToolCallRow key={t.id} tool={t} />
              ))}
            </div>
          )}
        </div>
      )}

      {(run.state === 'failed' || run.state === 'cancelled') && (
        <div className="ar-actions">
          <button className="ar-btn" onClick={() => bridge.rerunLast()}>
            {run.state === 'failed' ? '重试' : '重新运行'}
          </button>
        </div>
      )}
    </div>
  );
}
