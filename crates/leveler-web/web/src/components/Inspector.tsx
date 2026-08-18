// 右栏任务面板：任务 / 验证 / 历史 / 记忆 四个 tab。
// 「任务」以执行进度为中心：当前任务与状态、Main/子 agent 树（multi-agent）、
// 执行计划、改动摘要（点击切中央 Changes 视图，不在右栏重复 diff 明细）、
// 待确认事项、工具调用统计；「记忆」承载 memory_list 的
// active/pending/archived 与 接受/遗忘（用户权威操作）。

import { useEffect, useState } from 'react';
import { useAppDispatch, useAppState, type SessionView, type SubAgentView } from '../state/store';
import { useBridge } from '../state/bridge';
import { formatTokens } from '../lib/format';
import type { CheckState, PlanStepStatus } from '../types/protocol';

type Tab = 'task' | 'verify' | 'ckpt' | 'memory';

const TABS: ReadonlyArray<readonly [Tab, string]> = [
  ['task', '任务'],
  ['verify', '验证'],
  ['ckpt', '历史'],
  ['memory', '记忆'],
];

const STEP_LABEL: Record<PlanStepStatus, string> = {
  done: '已完成',
  running: '进行中',
  failed: '失败',
  skipped: '跳过',
  pending: '待执行',
};

const CHECK_GLYPH: Record<CheckState, string> = {
  passed: '✓',
  running: '◍',
  failed: '✗',
  skipped: '·',
};

/** 任务状态：待确认 > 运行中 > 空闲 */
function taskStatus(current: SessionView | null): { cls: 'run' | 'wait' | 'idle'; label: string } {
  if (!current) return { cls: 'idle', label: '无会话' };
  if (current.pendingApprovals.length > 0 || current.pendingClarifications.length > 0) {
    return { cls: 'wait', label: '等待确认' };
  }
  if (current.turnActive) return { cls: 'run', label: '运行中' };
  return { cls: 'idle', label: '空闲' };
}

export function Inspector() {
  const [tab, setTab] = useState<Tab>('task');
  const current = useAppState().current;
  const bridge = useBridge();

  return (
    <aside className="inspector">
      <div className="insp-tabs">
        {TABS.map(([key, label]) => (
          <button
            key={key}
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

/** 子 agent 一行：状态 + 昵称/角色 + 当前活动/结果 + token 用量。 */
function AgentRow({ agent }: { agent: SubAgentView }) {
  const glyph = agent.status === 'run' ? '●' : agent.status === 'done' ? '✓' : '✗';
  const tokens = agent.tokens.input + agent.tokens.output;
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
      {tokens > 0 && <span className="ag-tokens">{formatTokens(tokens)} tok</span>}
    </div>
  );
}

function TaskTab({ current }: { current: SessionView | null }) {
  const status = taskStatus(current);
  const dispatch = useAppDispatch();
  const diffFiles = current?.diff?.files ?? [];
  const totalAdd = diffFiles.reduce((n, f) => n + f.added, 0);
  const totalDel = diffFiles.reduce((n, f) => n + f.removed, 0);
  const pending = current?.pendingApprovals ?? [];
  const pendingClar = current?.pendingClarifications ?? [];
  const agents = current?.agents ?? [];

  // 工具调用按名称聚合：read × 12
  const toolAgg = new Map<string, number>();
  for (const t of current?.tools ?? []) {
    toolAgg.set(t.name, (toolAgg.get(t.name) ?? 0) + 1);
  }

  return (
    <>
      <div className="task-card">
        <div className="t-goal">{current?.title ?? '新任务'}</div>
        <div className={`t-status ${status.cls}`}>
          <i className="dot" />
          {status.label}
        </div>
      </div>

      {agents.length > 0 && (
        <>
          <div className="insp-sec">子 Agent</div>
          <div className="agent-tree">
            {agents.map((a) => (
              <AgentRow key={a.id} agent={a} />
            ))}
          </div>
        </>
      )}

      <div className="insp-sec">执行计划</div>
      {current?.plan && current.plan.steps.length > 0 ? (
        <div>
          {current.plan.steps.map((step) => (
            <div key={step.index} className={`plan-step ${step.status}`}>
              <span className="idx">{step.index + 1}</span>
              <span className="desc">{step.description}</span>
              <span className="st-label">{STEP_LABEL[step.status]}</span>
            </div>
          ))}
        </div>
      ) : (
        <div className="insp-empty">暂无计划 —— 计划模式或编排运行后出现。</div>
      )}

      <div className="insp-sec">改动</div>
      {diffFiles.length > 0 ? (
        <button
          className="changes-sum as-link"
          title="在中央区域查看完整 diff"
          onClick={() => dispatch({ type: 'stage_view', view: 'diff' })}
        >
          <span className="n">{diffFiles.length} 个文件</span>
          <span className="add">+{totalAdd}</span>
          <span className="del">−{totalDel}</span>
          <span className="goto">→</span>
        </button>
      ) : (
        <div className="insp-empty">无改动。</div>
      )}

      <div className="insp-sec">待确认</div>
      {pending.length + pendingClar.length > 0 ? (
        <>
          {pending.map((a) => (
            <div className="confirm-item" key={a.id}>
              {a.summary}
            </div>
          ))}
          {pendingClar.map((c) => (
            <div className="confirm-item" key={c.id}>
              {c.question}
            </div>
          ))}
        </>
      ) : (
        <div className="insp-empty">无</div>
      )}

      <div className="insp-sec">工具调用</div>
      {toolAgg.size > 0 ? (
        <div className="tool-agg">
          {[...toolAgg.entries()].map(([name, cnt]) => (
            <div className="row" key={name}>
              <span>{name}</span>
              <span className="cnt">× {cnt}</span>
            </div>
          ))}
          <div className="row" style={{ marginTop: 6 }}>
            <span>tokens 输入/输出</span>
            <span className="cnt">
              {formatTokens(current?.tokens.input ?? 0)} / {formatTokens(current?.tokens.output ?? 0)}
            </span>
          </div>
        </div>
      ) : (
        <div className="insp-empty">本回合暂无工具调用。</div>
      )}
    </>
  );
}

function MemoryTab({ current }: { current: SessionView | null }) {
  const bridge = useBridge();
  const memory = current?.memory ?? null;
  const sessionId = current?.id ?? null;

  // 打开 tab 时拉一次列表（含 archived）；之后靠操作后的主动刷新。
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
