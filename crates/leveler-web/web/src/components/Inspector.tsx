// 右栏任务面板：任务 / 验证 / 检查点 / 记忆。
// 「任务」按状态换优先级：等待操作 > 运行中 > 终态 > 空闲。
// 不在这里铺完整工具列表或完整 Diff。

import { useEffect, useState } from 'react';
import { useAppDispatch, useAppState, type SessionView, type SubAgentView } from '../state/store';
import { useBridge } from '../state/bridge';
import { formatElapsed, formatTokens } from '../lib/format';
import { currentPlanProgress, inspectorMode } from '../lib/inspectorModel';
import { presentTurnEnd } from '../lib/turn';
import type { CheckState, UiApprovalRequest, UiClarificationRequest } from '../types/protocol';

type Tab = 'task' | 'verify' | 'ckpt' | 'memory';

const TABS: ReadonlyArray<readonly [Tab, string]> = [
  ['task', '任务'],
  ['verify', '验证'],
  ['ckpt', '检查点'],
  ['memory', '记忆'],
];

const CHECK_GLYPH: Record<CheckState, string> = {
  passed: '✓',
  running: '◍',
  failed: '✗',
  skipped: '·',
};

export function Inspector() {
  const [tab, setTab] = useState<Tab>('task');
  const state = useAppState();
  const current = state.current;
  const bridge = useBridge();
  const mode = inspectorMode(current);

  useEffect(() => {
    if (mode === 'waiting') setTab('task');
  }, [mode]);

  return (
    <aside className={`inspector${state.inspectorOpen ? '' : ' is-hidden'}`} aria-label="任务面板">
      <div className="insp-tabs" role="tablist">
        {TABS.map(([key, label]) => (
          <button
            key={key}
            role="tab"
            aria-selected={tab === key}
            className={`insp-tab${tab === key ? ' on' : ''}`}
            onClick={() => setTab(key)}
          >
            {label}
            {key === 'memory' && (current?.memory?.pending.length ?? 0) > 0 && (
              <i className="insp-dot" title="有待采纳的记忆" />
            )}
          </button>
        ))}
      </div>
      <div className="insp-body">
        {tab === 'task' && <TaskTab current={current} />}

        {tab === 'verify' && (
          <>
            {current?.verification && current.verification.checks.length > 0 ? (
              <>
                <div className="checks">
                  {current.verification.checks.map((c) => {
                    const cls =
                      c.status === 'passed' ? 'ok' : c.status === 'failed' ? 'bad' : 'wait';
                    return (
                      <div className={`check ${cls}`} key={c.name}>
                        <span className="st">{CHECK_GLYPH[c.status]}</span>
                        <span>{c.name}</span>
                      </div>
                    );
                  })}
                </div>
                {current.verification.checks
                  .filter((c) => c.evidence)
                  .map((c) => (
                    <div className="diff-patch" key={`${c.name}-ev`}>
                      {c.evidence}
                    </div>
                  ))}
                <dl className="kv" style={{ marginTop: 10 }}>
                  <dt>结果</dt>
                  <dd className={current.verification.passed ? 'good' : ''}>
                    {current.verification.passed === null
                      ? '进行中'
                      : current.verification.passed
                        ? '通过'
                        : '未通过'}
                  </dd>
                </dl>
              </>
            ) : (
              <div className="insp-empty">暂无验证结果 —— 任务执行验证步骤后展示。</div>
            )}
          </>
        )}

        {tab === 'ckpt' && (
          <>
            {current && current.checkpoints.length > 0 ? (
              current.checkpoints.map((c) => (
                <div className="ckpt" key={c.id}>
                  <span className="label">
                    #{c.ordinal} {c.label}
                  </span>
                  <button
                    className="restore"
                    title={`回滚到检查点 ${c.id}`}
                    onClick={() => bridge.restoreCheckpoint(c.id)}
                  >
                    回滚
                  </button>
                </div>
              ))
            ) : (
              <div className="insp-empty">暂无检查点。</div>
            )}
          </>
        )}

        {tab === 'memory' && <MemoryTab current={current} />}
      </div>
    </aside>
  );
}

function AgentRow({ agent }: { agent: SubAgentView }) {
  const glyph = agent.status === 'run' ? '●' : agent.status === 'done' ? '✓' : '✗';
  return (
    <div className={`agent-row ${agent.status}`}>
      <span className="ag-glyph">{glyph}</span>
      <span className="ag-main">
        <span className="ag-name">
          {agent.nickname}
          <span className="ag-role">{agent.role}</span>
        </span>
        <span className="ag-detail">{agent.detail}</span>
        {agent.status === 'run' && agent.recentStep && (
          <span className="ag-step">{agent.recentStep}</span>
        )}
      </span>
    </div>
  );
}

function ChangesJump({ current }: { current: SessionView }) {
  const dispatch = useAppDispatch();
  const files = current.diff?.files ?? [];
  const totalAdd = files.reduce((n, f) => n + f.added, 0);
  const totalDel = files.reduce((n, f) => n + f.removed, 0);
  if (files.length === 0) return null;
  return (
    <button
      className="changes-sum as-link"
      title="在中央区域查看完整 diff"
      onClick={() => dispatch({ type: 'stage_view', view: 'diff' })}
    >
      <span className="n">{files.length} files</span>
      <span className="add">+{totalAdd}</span>
      <span className="del">−{totalDel}</span>
      <span className="goto">→</span>
    </button>
  );
}

function ApprovalActions({ request }: { request: UiApprovalRequest }) {
  const bridge = useBridge();
  return (
    <div className="action-block" role="region" aria-label="需要确认">
      <div className="action-kicker">⚠ 需要确认</div>
      <div className="action-body">
        <div>{request.summary}</div>
        {request.command && <pre className="action-cmd">{request.command}</pre>}
      </div>
      <div className="action-ops">
        <button className="abtn primary" onClick={() => bridge.decideApproval(request.id, 'approve_once')}>
          允许一次
        </button>
        <button className="abtn" onClick={() => bridge.decideApproval(request.id, 'approve_session')}>
          本会话允许
        </button>
        <button className="abtn danger" onClick={() => bridge.decideApproval(request.id, 'deny')}>
          拒绝
        </button>
      </div>
    </div>
  );
}

function ClarificationActions({ request }: { request: UiClarificationRequest }) {
  const bridge = useBridge();
  const [answer, setAnswer] = useState('');
  return (
    <div className="action-block" role="region" aria-label="需要澄清">
      <div className="action-kicker">需要澄清</div>
      <div className="action-body">{request.question}</div>
      {request.options.length > 0 && (
        <div className="c-options">
          {request.options.map((opt) => (
            <button key={opt} className="abtn" onClick={() => bridge.answerClarification(request.id, opt)}>
              {opt}
            </button>
          ))}
        </div>
      )}
      <div className="c-input-row">
        <input
          value={answer}
          aria-label="回答"
          placeholder="输入回答"
          onChange={(e) => setAnswer(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') bridge.answerClarification(request.id, answer);
          }}
        />
        <button className="abtn primary" onClick={() => bridge.answerClarification(request.id, answer)}>
          回答
        </button>
      </div>
    </div>
  );
}

function TaskTab({ current }: { current: SessionView | null }) {
  const mode = inspectorMode(current);
  const elapsed = current?.turnStartedAt
    ? Math.max(0, Math.floor((Date.now() - current.turnStartedAt) / 1000))
    : 0;
  const plan = currentPlanProgress(current?.plan);
  const agents = current?.agents ?? [];
  const [, tick] = useState(0);
  useEffect(() => {
    if (mode !== 'running') return;
    const t = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(t);
  }, [mode]);

  if (!current) {
    return <div className="insp-empty">先进入一个会话。</div>;
  }

  if (mode === 'waiting') {
    return (
      <>
        {current.pendingApprovals.map((a) => (
          <ApprovalActions key={a.id} request={a} />
        ))}
        {current.pendingClarifications.map((c) => (
          <ClarificationActions key={c.id} request={c} />
        ))}
        <ChangesJump current={current} />
      </>
    );
  }

  if (mode === 'running') {
    return (
      <>
        <div className="task-card">
          <div className={`t-status run`}>
            <i className="dot" />
            {current.activity ?? '正在运行'}
          </div>
          <div className="t-elapsed">{formatElapsed(elapsed)}</div>
        </div>
        {plan && (
          <>
            <div className="insp-sec">当前步骤</div>
            <div className="plan-now">
              {plan.current} / {plan.total} {plan.description}
            </div>
          </>
        )}
        {agents.length > 0 && (
          <>
            <div className="insp-sec">Agents</div>
            <div className="agent-tree">
              {agents.map((a) => (
                <AgentRow key={a.id} agent={a} />
              ))}
            </div>
          </>
        )}
        <div className="insp-sec">Changes</div>
        <ChangesJump current={current} />
        <details className="insp-more">
          <summary>更多</summary>
          <MetaFooter current={current} />
        </details>
      </>
    );
  }

  if (mode === 'terminal' && current.lastTurn) {
    const p = presentTurnEnd(current.lastTurn);
    const sec = Math.round(current.lastTurn.ms / 1000);
    return (
      <>
        <div className={`task-card tone-${p.tone}`}>
          <div className={`t-status term ${p.tone}`}>
            <span aria-hidden="true">{p.glyph}</span>
            {p.label}
          </div>
          {sec > 0 && <div className="t-elapsed">{formatElapsed(sec)}</div>}
          {p.detail && <div className="t-detail">{p.detail}</div>}
        </div>
        {current.verification && current.verification.checks.length > 0 && (
          <>
            <div className="insp-sec">Verification</div>
            <div className="checks">
              {current.verification.checks.map((c) => {
                const cls = c.status === 'passed' ? 'ok' : c.status === 'failed' ? 'bad' : 'wait';
                return (
                  <div className={`check ${cls}`} key={c.name}>
                    <span className="st">{CHECK_GLYPH[c.status]}</span>
                    <span>{c.name}</span>
                  </div>
                );
              })}
            </div>
          </>
        )}
        <div className="insp-sec">Changes</div>
        <ChangesJump current={current} />
        {p.detail && (
          <>
            <div className="insp-sec">Outcome</div>
            <div className="t-detail">{p.detail}</div>
          </>
        )}
      </>
    );
  }

  return (
    <>
      <div className="task-card">
        <div className="t-goal">{current.title || '新任务'}</div>
        <div className="t-status idle">
          <i className="dot" />
          空闲
        </div>
      </div>
      <ChangesJump current={current} />
    </>
  );
}

function MetaFooter({ current }: { current: SessionView }) {
  const toolAgg = new Map<string, number>();
  for (const t of current.tools) {
    toolAgg.set(t.name, (toolAgg.get(t.name) ?? 0) + 1);
  }
  return (
    <div className="tool-agg">
      {[...toolAgg.entries()].map(([name, cnt]) => (
        <div className="row" key={name}>
          <span>{name}</span>
          <span className="cnt">× {cnt}</span>
        </div>
      ))}
      <div className="row" style={{ marginTop: 6 }}>
        <span>tokens</span>
        <span className="cnt">
          {formatTokens(current.tokens.input)} / {formatTokens(current.tokens.output)}
        </span>
      </div>
    </div>
  );
}

function MemoryTab({ current }: { current: SessionView | null }) {
  const bridge = useBridge();
  const memory = current?.memory ?? null;
  const sessionId = current?.id ?? null;

  useEffect(() => {
    if (sessionId) bridge.listMemory();
  }, [sessionId, bridge]);

  if (!current) return <div className="insp-empty">先进入一个会话。</div>;
  if (!memory) return <div className="insp-empty">读取项目记忆中…</div>;

  return (
    <>
      <dl className="kv">
        <dt>Active</dt>
        <dd>{memory.active.length}</dd>
        <dt>Pending</dt>
        <dd>{memory.pending.length}</dd>
        <dt>Archived</dt>
        <dd>{memory.archived.length}</dd>
      </dl>

      {memory.pending.length > 0 && (
        <>
          <div className="insp-sec">待采纳（需要你的同意）</div>
          {memory.pending.map((m) => (
            <div className="mem-row pending" key={m.id}>
              <span className="mem-title">○ {m.title}</span>
              <span className="mem-ops">
                <button className="mem-btn" onClick={() => bridge.acceptMemory(m.id)}>
                  接受
                </button>
                <button className="mem-btn ghost" onClick={() => bridge.forgetMemory(m.id)}>
                  忽略
                </button>
              </span>
            </div>
          ))}
        </>
      )}

      <div className="insp-sec">已生效</div>
      {memory.active.length > 0 ? (
        memory.active.map((m) => (
          <div className="mem-row" key={m.id}>
            <span className="mem-title">✓ {m.title}</span>
            <span className="mem-ops">
              <button
                className="mem-btn ghost"
                title="归档这条记忆（可从 archived 追溯）"
                onClick={() => bridge.forgetMemory(m.id)}
              >
                遗忘
              </button>
            </span>
          </div>
        ))
      ) : (
        <div className="insp-empty">暂无生效记忆。</div>
      )}

      {memory.archived.length > 0 && (
        <>
          <div className="insp-sec">已归档</div>
          {memory.archived.map((m) => (
            <div className="mem-row archived" key={m.id}>
              <span className="mem-title">· {m.title}</span>
            </div>
          ))}
        </>
      )}
    </>
  );
}
