// Execution workspace: QueryObservability window, projected — not live tools.

import { useEffect } from 'react';
import { useAppDispatch, useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { linkedChangePath } from '../lib/changeFiles';
import { formatDuration, formatElapsed } from '../lib/format';
import { projectObservability, type ExecStep } from '../lib/observabilityView';

function glyph(step: ExecStep): string {
  if (step.status === 'ok') return '✓';
  if (step.status === 'fail') return '✗';
  if (step.status === 'running') return '●';
  return '·';
}

export function ExecutionView() {
  const { observation, observationStatus, current } = useAppState();
  const dispatch = useAppDispatch();
  const bridge = useBridge();
  const diffPaths = current?.diff?.files.map((f) => f.path) ?? [];

  useEffect(() => {
    if (current && observationStatus === 'idle') bridge.queryObservability(current.id);
  }, [bridge, current, observationStatus]);

  if (!current) {
    return (
      <div className="exec-placeholder" role="status">
        <div className="exec-kicker">Execution</div>
        <p>先进入一个会话。</p>
      </div>
    );
  }

  if (observationStatus === 'loading' && !observation) {
    return (
      <div className="exec-placeholder" role="status">
        <div className="exec-kicker">Execution</div>
        <p>查询 durable Runtime…</p>
      </div>
    );
  }

  if (!observation) {
    return (
      <div className="exec-placeholder" role="status">
        <div className="exec-kicker">Execution</div>
        <p>此会话还没有可展示的 durable 执行记录。</p>
      </div>
    );
  }

  const view = projectObservability(observation);
  const running = view.summary.status.toLowerCase().includes('run') || current.turnActive;

  return (
    <div className="exec-view">
      <div className="exec-kicker">Execution</div>
      <div className={`exec-summary ${running ? 'run' : view.summary.status === 'completed' ? 'ok' : ''}`}>
        {running ? '● Running' : view.summary.status}
        {view.summary.durationMs != null && (
          <span className="exec-dur">{formatElapsed(Math.round(view.summary.durationMs / 1000))}</span>
        )}
      </div>
      {view.groups.length === 0 && <p className="exec-empty">此窗口没有可展示的事件。</p>}
      {view.groups.map((group, gi) => (
        <section key={group.turnId ?? `g${gi}`} className="exec-turn">
          {group.turnId && <div className="exec-turn-h">Turn {group.turnId.slice(0, 8)}</div>}
          <ol className="exec-tl">
            {group.steps.map((step) => {
              const jump = linkedChangePath(step.detail, diffPaths);
              return (
                <li key={step.sequence} className={`exec-step ${step.status} kind-${step.kind}`}>
                  <span className="exec-g" aria-hidden="true">
                    {glyph(step)}
                  </span>
                  <span className="exec-time">{step.time}</span>
                  <span className="exec-kind">{step.kind}</span>
                  <span className="exec-title">
                    {step.agentLabel ? `${step.agentLabel} · ${step.title}` : step.title}
                  </span>
                  {jump ? (
                    <button
                      type="button"
                      className="exec-jump"
                      onClick={() => dispatch({ type: 'focus_diff', path: jump })}
                    >
                      {jump}
                    </button>
                  ) : (
                    step.detail && <span className="exec-detail">{step.detail}</span>
                  )}
                  {step.durationMs != null && (
                    <span className="exec-ms">{formatDuration(step.durationMs)}</span>
                  )}
                </li>
              );
            })}
          </ol>
        </section>
      ))}
    </div>
  );
}
