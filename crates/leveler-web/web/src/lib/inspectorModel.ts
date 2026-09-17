// Inspector 任务面板的状态优先级（纯函数，便于单测）。
// 用户语言：等待操作 / 运行中 / 终态 / 空闲。不暴露 RuntimeEvent。

import type { SessionView } from '../state/store';
import type { ChildOutcome, ChildStop, UiChildState, UiPlan } from '../types/protocol';
import { presentTurnEnd, type TurnEnd, type TurnTone } from './turn';

export type InspectorMode = 'waiting' | 'running' | 'terminal' | 'idle';

const OUTCOME_LABEL: Record<ChildOutcome, string> = {
  completed_with_findings: '完成',
  completed_no_findings: '完成 · 无发现',
  incomplete_partial: '部分结果',
  incomplete_no_result: '无结果',
};

const STOP_LABEL: Partial<Record<ChildStop, string>> = {
  budget: '预算耗尽',
  cancelled: '已取消',
  failed: '失败',
  lost: '已丢失',
};

/** One child's state in user words, from the runtime's typed facts only.
 * "No result" and "no findings" are opposite facts and never share a label. */
export function childStateLabel(a: {
  state: UiChildState;
  ok: boolean;
  outcome: ChildOutcome | null;
  stop: ChildStop | null;
}): string {
  if (a.state === 'running') return '运行中';
  if (a.state === 'interrupted') return '已中断';
  const outcome = a.outcome ? OUTCOME_LABEL[a.outcome] : a.ok ? '完成' : '未完成';
  const stop = a.stop ? STOP_LABEL[a.stop] : undefined;
  return stop ? `${outcome} · ${stop}` : outcome;
}

export function inspectorMode(s: SessionView | null): InspectorMode {
  if (!s) return 'idle';
  if (s.pendingApprovals.length > 0 || s.pendingClarifications.length > 0) return 'waiting';
  if (s.turnActive) return 'running';
  if (s.lastTurn) return 'terminal';
  return 'idle';
}

/** Header cue while the user must act. Click opens Inspector — never a second modal. */
export function headerWaitingCue(
  s: SessionView | null,
): { glyph: '⚠'; label: '等待确认' | '需要回答' } | null {
  if (inspectorMode(s) !== 'waiting' || !s) return null;
  if (s.pendingApprovals.length > 0) return { glyph: '⚠', label: '等待确认' };
  return { glyph: '⚠', label: '需要回答' };
}

export function inspectorTerminalTone(end: TurnEnd): TurnTone {
  return presentTurnEnd(end).tone;
}

export type InspectorSection =
  | 'action'
  | 'task'
  | 'result'
  | 'plan'
  | 'verification'
  | 'changes'
  | 'agents'
  | 'runtime'
  | 'more';

/** Contextual Inspector: only sections with content. `more` is always last. */
export function inspectorVisibleSections(
  s: SessionView | null,
  extras: { observation?: boolean; delegatedAgents?: boolean } = {},
): InspectorSection[] {
  if (!s) return [];
  const mode = inspectorMode(s);
  const sections: InspectorSection[] = [];
  if (mode === 'waiting') sections.push('action');
  if (mode === 'running' || mode === 'idle') sections.push('task');
  if (mode === 'terminal') sections.push('result');
  if (planProgress(s.plan, s.turnActive)) sections.push('plan');
  if (s.verification && s.verification.checks.length > 0) sections.push('verification');
  if ((s.diff?.files.length ?? 0) > 0) sections.push('changes');
  if (s.agents.length > 0 || extras.delegatedAgents) sections.push('agents');
  if (extras.observation) sections.push('runtime');
  sections.push('more');
  return sections;
}

/**
 * The plan is the agent's declared progress: how much it declared done, and —
 * only while a turn runs — the step it declared in progress. No declared step
 * means none is shown; a pending step is never promoted to "current". Outside
 * a running turn the plan is the last record of what was declared.
 */
export function planProgress(
  plan: UiPlan | null | undefined,
  live: boolean,
): { done: number; total: number; live: boolean; active: string | null } | null {
  if (!plan || plan.steps.length === 0) return null;
  const done = plan.steps.filter((s) => s.status === 'done' || s.status === 'skipped').length;
  const running = live ? plan.steps.find((s) => s.status === 'running') : undefined;
  return { done, total: plan.steps.length, live, active: running?.description ?? null };
}
