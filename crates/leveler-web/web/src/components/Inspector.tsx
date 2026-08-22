// Contextual Inspector: waiting > running > terminal > idle.
// Sections appear only when they have content. Checkpoints / Memory live under More.

import { ArrowUp, CircleHelp, ShieldAlert } from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { choiceOrdinal, splitChoiceOption } from '../lib/choiceLabel';
import { CTRL_ICON } from '../lib/icons';
import { completionTruth, trustLabel, type ArtifactFacts } from '../lib/completionTruth';
import { formatElapsed } from '../lib/format';
import { currentPlanProgress, inspectorMode, inspectorVisibleSections } from '../lib/inspectorModel';
import {
  projectObservability,
  type AgentDelegationStatus,
  type AgentDelegationView,
} from '../lib/observabilityView';
import { presentTurnEnd } from '../lib/turn';
import { useBridge } from '../state/bridge';
import { useAppDispatch, useAppState, type SessionView, type SubAgentView } from '../state/store';
import type { CheckState, UiApprovalRequest, UiClarificationRequest } from '../types/protocol';

const CHECK_GLYPH: Record<CheckState, string> = {
  passed: '✓',
  running: '◍',
  failed: '✗',
  skipped: '·',
};

export function Inspector() {
  const state = useAppState();
  const current = state.current;
  const observation = state.observation;
  const delegated = observation ? projectObservability(observation).agents.length > 0 : false;
  const sections = inspectorVisibleSections(current, {
    observation: Boolean(observation),
    delegatedAgents: delegated,
  });

  return (
    <aside className={`inspector${state.inspectorOpen ? '' : ' is-hidden'}`} aria-label="任务面板">
      <div className="insp-body">
        {!current && <div className="insp-empty">先进入一个会话。</div>}
        {current &&
          sections.map((id) => {
            switch (id) {
              case 'action':
                return <ActionSection key="action" current={current} />;
              case 'task':
                return <TaskSection key="task" current={current} />;
              case 'result':
                return <ResultCard key="result" current={current} />;
              case 'plan':
                return <PlanSection key="plan" current={current} />;
              case 'verification':
                return <VerificationSection key="verification" current={current} />;
              case 'changes':
                return (
                  <InspectorBlock key="changes" title="CHANGES">
                    <ChangesJump current={current} />
                  </InspectorBlock>
                );
              case 'agents':
                return <AgentsSection key="agents" live={current.agents} />;
              case 'runtime':
                return <RuntimeSection key="runtime" />;
              case 'more':
                return <MoreSection key="more" current={current} />;
              default:
                return null;
            }
          })}
      </div>
    </aside>
  );
}

function InspectorBlock({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="insp-block">
      <h2 className="insp-sec">{title}</h2>
      {children}
    </section>
  );
}

function ActionSection({ current }: { current: SessionView }) {
  return (
    <section className="insp-block" aria-label="需要操作">
      <h2 className="insp-sec">ACTION REQUIRED</h2>
      {current.pendingApprovals.map((a) => (
        <ApprovalActions key={a.id} request={a} />
      ))}
      {current.pendingClarifications.map((c) => (
        <ClarificationActions key={c.id} request={c} />
      ))}
    </section>
  );
}

function TaskSection({ current }: { current: SessionView }) {
  const mode = inspectorMode(current);
  const elapsed = current.turnStartedAt
    ? Math.max(0, Math.floor((Date.now() - current.turnStartedAt) / 1000))
    : 0;
  const [, tick] = useState(0);
  useEffect(() => {
    if (mode !== 'running') return;
    const t = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(t);
  }, [mode]);

  return (
    <InspectorBlock title="TASK">
      <div className="task-card">
        {mode === 'running' ? (
          <>
            <div className="t-status run">
              <i className="dot" />
              {current.activity ?? '正在运行'}
            </div>
            <div className="t-elapsed">{formatElapsed(elapsed)}</div>
          </>
        ) : (
          <>
            <div className="t-goal">{current.title || '新任务'}</div>
            <div className="t-status idle">
              <i className="dot" />
              空闲
            </div>
          </>
        )}
      </div>
    </InspectorBlock>
  );
}

function ResultCard({ current }: { current: SessionView }) {
  if (!current.lastTurn) return null;
  const p = presentTurnEnd(current.lastTurn);
  const sec = Math.round(current.lastTurn.ms / 1000);
  const truth = completionTruth(current);
  return (
    <InspectorBlock title="RESULT">
      <div className={`task-card tone-${p.tone}`}>
        <div className={`t-status term ${p.tone}`}>
          <span aria-hidden="true">{p.glyph}</span>
          {p.label}
        </div>
        {sec > 0 && <div className="t-elapsed">{formatElapsed(sec)}</div>}
        {p.detail && <div className="t-detail">{p.detail}</div>}
      </div>
      {truth && truth.kind !== 'idle' && (
        <dl className="kv runtime-kv">
          <dt>Trust</dt>
          <dd>{trustLabel(truth.trust)}</dd>
          <dt>Artifacts</dt>
          <dd>{artifactLine(truth.artifacts)}</dd>
        </dl>
      )}
      {truth?.facts.map((f) => (
        <div className="t-detail" key={f}>
          {f}
        </div>
      ))}
      {truth?.recoveryHint && <div className="t-detail">{truth.recoveryHint}</div>}
    </InspectorBlock>
  );
}

function PlanSection({ current }: { current: SessionView }) {
  const plan = currentPlanProgress(current.plan);
  if (!plan) return null;
  return (
    <InspectorBlock title="PLAN">
      <div className="plan-now">
        {plan.current} / {plan.total} {plan.description}
      </div>
    </InspectorBlock>
  );
}

function VerificationSection({ current }: { current: SessionView }) {
  const verification = current.verification;
  if (!verification || verification.checks.length === 0) return null;
  return (
    <InspectorBlock title="VERIFICATION">
      <div className="checks">
        {verification.checks.map((c) => {
          const cls = c.status === 'passed' ? 'ok' : c.status === 'failed' ? 'bad' : 'wait';
          return (
            <div className={`check ${cls}`} key={c.name}>
              <span className="st">{CHECK_GLYPH[c.status]}</span>
              <span>{c.name}</span>
            </div>
          );
        })}
      </div>
      {verification.checks
        .filter((c) => c.evidence)
        .map((c) => (
          <div className="diff-patch" key={`${c.name}-ev`}>
            {c.evidence}
          </div>
        ))}
      <dl className="kv" style={{ marginTop: 8 }}>
        <dt>结果</dt>
        <dd className={verification.passed ? 'good' : ''}>
          {verification.passed === null
            ? '进行中'
            : verification.passed
              ? '通过'
              : '未通过'}
        </dd>
      </dl>
    </InspectorBlock>
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
      type="button"
      className="changes-sum as-link"
      title="在中央区域查看完整 diff"
      onClick={() => dispatch({ type: 'stage_view', view: 'diff' })}
    >
      <span className="n">{files.length} files</span>
      <span className="add">+{totalAdd}</span>
      <span className="del">−{totalDel}</span>
      <span className="goto">View changes →</span>
    </button>
  );
}

function ApprovalActions({ request }: { request: UiApprovalRequest }) {
  const bridge = useBridge();
  return (
    <div className="action-block approval" role="region" aria-label="需要你的授权">
      <div className="action-head">
        <ShieldAlert {...CTRL_ICON} aria-hidden="true" />
        需要你的授权
      </div>
      <div className="action-body">{request.summary}</div>
      {request.command && <pre className="action-cmd">{request.command}</pre>}
      <div className="action-ops">
        <button
          type="button"
          className="abtn primary"
          onClick={() => bridge.decideApproval(request.id, 'approve_once')}
        >
          允许一次
        </button>
        <button type="button" className="abtn" onClick={() => bridge.decideApproval(request.id, 'approve_session')}>
          本会话允许
        </button>
        <button type="button" className="abtn danger" onClick={() => bridge.decideApproval(request.id, 'deny')}>
          拒绝
        </button>
      </div>
    </div>
  );
}

function ClarificationActions({ request }: { request: UiClarificationRequest }) {
  const bridge = useBridge();
  const [answer, setAnswer] = useState('');
  const submit = () => {
    const v = answer.trim();
    if (!v) return;
    bridge.answerClarification(request.id, v);
  };
  return (
    <div className="action-block clarification" role="region" aria-label="需要你补充信息">
      <div className="action-head">
        <CircleHelp {...CTRL_ICON} aria-hidden="true" />
        需要你补充信息
      </div>
      <div className="action-body">{request.question}</div>
      {request.options.length > 0 && (
        <div className="choice-rows">
          {request.options.map((opt, i) => {
            const split = splitChoiceOption(opt);
            const ordinal = split.ordinal ?? choiceOrdinal(i);
            return (
              <button
                key={`${opt}-${i}`}
                type="button"
                className="choice-row"
                onClick={() => bridge.answerClarification(request.id, opt)}
              >
                <span className="choice-ord">{ordinal}</span>
                <span className="choice-body">{split.body}</span>
              </button>
            );
          })}
        </div>
      )}
      <div className="mini-composer-label">或者直接说明</div>
      <div className="mini-composer">
        <textarea
          value={answer}
          aria-label="补充说明"
          placeholder="输入你的说明…"
          rows={1}
          onChange={(e) => setAnswer(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <button type="button" className="mini-send" title="提交" aria-label="提交" onClick={submit}>
          <ArrowUp size={16} strokeWidth={2} aria-hidden="true" />
        </button>
      </div>
    </div>
  );
}

function artifactLine(a: ArtifactFacts): string {
  if (a.source === 'report') return `${a.files} files changed`;
  if (a.source === 'diff') return `${a.files} files  +${a.added} −${a.removed}`;
  return `${a.files} files`;
}

function delegationStatus(status: AgentDelegationStatus): string {
  if (status === 'running') return '● Running';
  if (status === 'completed') return '✓ Finished';
  return '✗ Failed';
}

function DelegationStar({ agents }: { agents: readonly AgentDelegationView[] }) {
  return (
    <div className="ag-star" aria-label="本会话委派">
      <div className="ag-star-main">Main</div>
      {agents.map((a, i) => {
        const last = i === agents.length - 1;
        return (
          <div key={a.id} className={`ag-star-row ${a.status}`}>
            <span className="ag-branch" aria-hidden="true">
              {last ? '└' : '├'}
            </span>
            <span className="ag-star-body">
              <span className="ag-name">
                {a.nickname}
                {a.role ? <span className="ag-role">{a.role}</span> : null}
              </span>
              <span className="ag-status">{delegationStatus(a.status)}</span>
              {(a.task || a.summary) && <span className="ag-detail">{a.task ?? a.summary}</span>}
            </span>
          </div>
        );
      })}
    </div>
  );
}

function AgentsSection({ live }: { live: readonly SubAgentView[] }) {
  const { observation } = useAppState();
  const running = live.filter((a) => a.status === 'run');
  const delegated = observation ? projectObservability(observation).agents : [];
  if (running.length === 0 && delegated.length === 0) return null;
  return (
    <InspectorBlock title="AGENTS">
      {running.length > 0 && (
        <>
          <div className="ag-kicker">Running</div>
          <div className="agent-tree">
            {running.map((a) => (
              <AgentRow key={a.id} agent={a} />
            ))}
          </div>
        </>
      )}
      {delegated.length > 0 && (
        <>
          <div className="ag-kicker">Delegated</div>
          <DelegationStar agents={delegated} />
        </>
      )}
    </InspectorBlock>
  );
}

function RuntimeSection() {
  const { observation, observationStatus } = useAppState();
  const dispatch = useAppDispatch();
  if (observationStatus === 'loading' && !observation) {
    return (
      <InspectorBlock title="RUNTIME">
        <div className="insp-empty">查询中…</div>
      </InspectorBlock>
    );
  }
  if (!observation) return null;
  const { summary } = projectObservability(observation);
  return (
    <InspectorBlock title="RUNTIME">
      <dl className="kv runtime-kv">
        <dt>Requests</dt>
        <dd>{summary.requestCount}</dd>
        <dt>Tools</dt>
        <dd>
          {summary.toolStarted}
          {summary.toolFinished !== summary.toolStarted ? ` · finished ${summary.toolFinished}` : ''}
        </dd>
        <dt>Agents</dt>
        <dd>{summary.delegatedAgents}</dd>
      </dl>
      <button
        type="button"
        className="insp-jump"
        onClick={() => dispatch({ type: 'stage_view', view: 'execution' })}
      >
        Open Execution
      </button>
    </InspectorBlock>
  );
}

function MoreSection({ current }: { current: SessionView }) {
  const pending = current.memory?.pending.length ?? 0;
  const moreOpen = useAppState().inspectorMore;
  const dispatch = useAppDispatch();
  return (
    <details
      className="insp-more"
      open={moreOpen}
      onToggle={(e) => dispatch({ type: 'set_inspector_more', open: e.currentTarget.open })}
    >
      <summary>
        More
        {pending > 0 && <i className="insp-dot" title="有待采纳的记忆" />}
      </summary>
      <div className="insp-sec">Checkpoints</div>
      {current.checkpoints.length > 0 ? (
        current.checkpoints.map((c) => (
          <CheckpointRow key={c.id} id={c.id} ordinal={c.ordinal} label={c.label} />
        ))
      ) : (
        <div className="insp-empty">暂无检查点。</div>
      )}
      <div className="insp-sec">Memory</div>
      <MemoryTab current={current} />
    </details>
  );
}

function CheckpointRow({ id, ordinal, label }: { id: string; ordinal: number; label: string }) {
  const bridge = useBridge();
  return (
    <div className="ckpt">
      <span className="label">
        #{ordinal} {label}
      </span>
      <button className="restore" title={`回滚到检查点 ${id}`} onClick={() => bridge.restoreCheckpoint(id)}>
        回滚
      </button>
    </div>
  );
}

function MemoryTab({ current }: { current: SessionView }) {
  const bridge = useBridge();
  const memory = current.memory ?? null;
  const sessionId = current.id;

  useEffect(() => {
    if (sessionId) bridge.listMemory();
  }, [sessionId, bridge]);

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
