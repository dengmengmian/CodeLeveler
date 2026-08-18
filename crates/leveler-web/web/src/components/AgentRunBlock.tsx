// 对话内 Agent 运行状态：一段轻量状态流（无卡片、无停止按钮——停止由顶部全局状态栏负责）。
// 运行中：当前动作（唯一主要文字）+ 最近完成 ≤3 条 + 「查看执行过程 · N」展开入口；
// 展开后原地变为紧凑工具明细列表，不再重复显示展开入口。
// 后台任务（background_task_*）作为当前 Agent 的执行活动行显示，不建新页面。
// 终态：走 lib/turn.ts 的 Turn Truth 呈现 —— 7 个终态各有语气与 reason，
// incomplete/unverified 永远不显示成绿色完成。

import { useEffect, useState } from 'react';
import { useAppState, type BackgroundTaskView } from '../state/store';
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

/** 最近完成的工具（最多 3 条），折叠态下的次级信息。 */
const RECENT_DONE = 3;

function BackgroundTaskRow({ task }: { task: BackgroundTaskView }) {
  const running = task.status === 'run';
  const elapsed = useElapsedSeconds(task.startedAt, running);
  if (running) {
    return (
      <div className="rs-bg run">
        <span className="rs-spin" /> {task.program}
        <span className="rs-time">后台运行 · {formatSeconds(elapsed)}</span>
      </div>
    );
  }
  const dur = task.durationMs !== null ? formatSeconds(Math.round(task.durationMs / 1000)) : '';
  return (
    <div className={`rs-bg ${task.status === 'done' ? 'ok' : 'bad'}`}>
      {task.status === 'done' ? '✓' : '✕'} {task.program}
      <span className="rs-time">
        {task.status === 'done' ? dur : `exit ${task.exitCode ?? '?'}${dur ? ` · ${dur}` : ''}`}
      </span>
    </div>
  );
}

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
  const backgroundTasks = current.backgroundTasks;

  const recentDone = tools.filter((t) => t.status !== 'run').slice(-RECENT_DONE);

  // 终态：语气化标记（✓/◇/⚠/✕/■）+ reason + 聚合摘要 + 按需展开的执行明细
  if (run.terminal) {
    const tone = run.tone ?? 'muted';
    const retry =
      run.outcome === 'failed' || run.outcome === 'cancelled' || run.outcome === 'truncated';
    return (
      <div className={`run-summary r-terminal tone-${tone}`}>
        <div className="rs-head">
          <span className="rs-icon">{run.glyph}</span>
          <span className="rs-primary">{run.primary}</span>
        </div>
        {run.detail && <div className="rs-detail">原因：{run.detail}</div>}
        {tools.length > 0 && <div className="rs-sub">{statsLine(stats)}</div>}
        {backgroundTasks.map((t) => (
          <BackgroundTaskRow key={t.id} task={t} />
        ))}
        {tools.length > 0 && (
          <button className="rs-toggle" onClick={() => setExpanded((v) => !v)}>
            {expanded ? '收起执行过程' : `查看执行过程 · ${tools.length}`}
          </button>
        )}
        {expanded && (
          <div className="rs-tools">
            {tools.map((t) => (
              <ToolCallRow key={t.id} tool={t} />
            ))}
          </div>
        )}
        {retry && (
          <div className="rs-actions">
            <button className="rs-btn" onClick={() => bridge.rerunLast()}>
              {run.outcome === 'failed' ? '重试' : '重新运行'}
            </button>
          </div>
        )}
      </div>
    );
  }

  return (
    <div className={`run-summary r-${run.state}`}>
      <div className="rs-head">
        <span className="rs-icon">{spinning ? <span className="rs-spin" /> : '⏸'}</span>
        <span className="rs-primary">{run.primary}</span>
        {current.turnActive && <span className="rs-time">{formatSeconds(elapsed)}</span>}
        {tools.length > 0 && (
          <button className="rs-toggle" onClick={() => setExpanded((v) => !v)}>
            {expanded ? '收起' : `查看执行过程 · ${tools.length}`}
          </button>
        )}
      </div>

      {run.detail && <div className="rs-detail">{run.detail}</div>}

      {backgroundTasks.map((t) => (
        <BackgroundTaskRow key={t.id} task={t} />
      ))}

      {!expanded && recentDone.length > 0 && (
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
    </div>
  );
}
