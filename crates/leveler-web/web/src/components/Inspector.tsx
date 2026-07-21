// 右栏任务面板：任务 / 改动 / 验证 / 历史 四个 tab。
// 「任务」以执行进度为中心：当前任务与状态、执行计划、改动摘要、
// 待确认事项、工具调用统计；其余 tab 渲染对应 snapshot 数据。

import { useState } from 'react';
import { useAppState, type SessionView } from '../state/store';
import { useBridge } from '../state/bridge';
import { formatTokens } from '../lib/format';
import type { CheckState, PlanStepStatus } from '../types/protocol';

type Tab = 'task' | 'diff' | 'verify' | 'ckpt';

const TABS: ReadonlyArray<readonly [Tab, string]> = [
  ['task', '任务'],
  ['diff', '改动'],
  ['verify', '验证'],
  ['ckpt', '历史'],
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

        {tab === 'diff' && (
          <>
            {current?.diff && current.diff.files.length > 0 ? (
              <DiffFiles />
            ) : (
              <div className="insp-empty">工作区干净，暂无变更。</div>
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
      </div>
    </aside>
  );
}

function TaskTab({ current }: { current: SessionView | null }) {
  const status = taskStatus(current);
  const diffFiles = current?.diff?.files ?? [];
  const totalAdd = diffFiles.reduce((n, f) => n + f.added, 0);
  const totalDel = diffFiles.reduce((n, f) => n + f.removed, 0);
  const pending = current?.pendingApprovals ?? [];
  const pendingClar = current?.pendingClarifications ?? [];

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
        <div className="changes-sum">
          <span className="n">{diffFiles.length} 个文件</span>
          <span className="add">+{totalAdd}</span>
          <span className="del">−{totalDel}</span>
        </div>
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

function DiffFiles() {
  const current = useAppState().current;
  const [openPath, setOpenPath] = useState<string | null>(null);
  if (!current?.diff) return null;
  const totalAdd = current.diff.files.reduce((n, f) => n + f.added, 0);
  const totalDel = current.diff.files.reduce((n, f) => n + f.removed, 0);

  return (
    <>
      <div className="changes-sum" style={{ marginBottom: 10 }}>
        <span className="n">{current.diff.files.length} 个文件</span>
        <span className="add">+{totalAdd}</span>
        <span className="del">−{totalDel}</span>
      </div>
      {current.diff.files.map((f) => (
        <div key={f.path}>
          <button
            className="diff-file"
            onClick={() => f.patch && setOpenPath(openPath === f.path ? null : f.path)}
          >
            <span className="p">{f.path}</span>
            <span className="add">+{f.added}</span>
            <span className="del">−{f.removed}</span>
          </button>
          {openPath === f.path && f.patch && <div className="diff-patch">{f.patch}</div>}
        </div>
      ))}
    </>
  );
}
