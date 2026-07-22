// 对话内 Agent 运行状态：一段轻量状态流（无卡片、无停止按钮——停止由顶部全局状态栏负责）。
// 运行中：当前动作（唯一主要文字）+ 最近完成 ≤2 条 + 「查看执行过程 · N」展开入口；
// 展开后原地变为紧凑工具明细列表，不再重复显示展开入口。
// 终态：压缩成一两行聚合摘要（N 次操作 · 读取 N 个文件 · …），工具明细仍可按需展开。
// 状态由 deriveRunState 从现有会话视图派生；不展示右侧已有的计划步骤/全局进度。

import { useEffect, useState } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { deriveRunState, type AgentRunState } from '../lib/runstate';
import { formatSeconds, statsLine, summarizeTools } from '../lib/toolstats';
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

/** 最近完成的工具（最多 2 条），折叠态下的次级信息。 */
const RECENT_DONE = 2;

export function AgentRunBlock() {
  const current = useAppState().current;
  const bridge = useBridge();
  const [expanded, setExpanded] = useState(false);

  const run = current ? deriveRunState(current) : null;
  const elapsed = useElapsedSeconds(current?.turnStartedAt ?? null, current?.turnActive ?? false);

  if (!current || !run) return null;

  const tools = current.tools;
  const stats = summarizeTools(tools);
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

  const recentDone = tools.filter((t) => t.status !== 'run').slice(-RECENT_DONE);

  // 终态（完成）：一至两行聚合摘要 + 按需展开的执行明细
  if (run.state === 'completed') {
    return (
      <div className="run-summary r-completed">
        <div className="rs-head">
          <span className="rs-icon">{glyph}</span>
          <span className="rs-primary">{run.primary}</span>
        </div>
        {tools.length > 0 && <div className="rs-sub">{statsLine(stats)}</div>}
        {tools.length > 0 && (
          <button className="rs-toggle" onClick={() => setExpanded((v) => !v)}>
            {expanded ? '收起执行过程' : '查看执行过程'}
          </button>
        )}
        {expanded && (
          <div className="rs-tools">
            {tools.map((t) => (
              <ToolCallRow key={t.id} tool={t} />
            ))}
          </div>
        )}
      </div>
    );
  }

  return (
    <div className={`run-summary r-${run.state}`}>
      <div className="rs-head">
        <span className="rs-icon">{spinning ? <span className="rs-spin" /> : glyph}</span>
        <span className="rs-primary">{run.primary}</span>
        {current.turnActive && !run.terminal && (
          <span className="rs-time">{formatSeconds(elapsed)}</span>
        )}
        {tools.length > 0 && !run.terminal && (
          <button className="rs-toggle" onClick={() => setExpanded((v) => !v)}>
            {expanded ? '收起' : `查看执行过程 · ${tools.length}`}
          </button>
        )}
      </div>

      {run.detail && <div className="rs-detail">{run.detail}</div>}

      {!expanded && !run.terminal && recentDone.length > 0 && (
        <div className="rs-recent">
          {recentDone.map((t) => (
            <ToolCallRow key={t.id} tool={t} />
          ))}
        </div>
      )}

      {expanded && (
        <div className="rs-tools">
          {tools.map((t) => (
            <ToolCallRow key={t.id} tool={t} />
          ))}
        </div>
      )}

      {(run.state === 'failed' || run.state === 'cancelled') && (
        <div className="rs-actions">
          <button className="rs-btn" onClick={() => bridge.rerunLast()}>
            {run.state === 'failed' ? '重试' : '重新运行'}
          </button>
        </div>
      )}
    </div>
  );
}
