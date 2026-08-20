// Inspector 任务面板的状态优先级（纯函数，便于单测）。
// 用户语言：等待操作 / 运行中 / 终态 / 空闲。不暴露 RuntimeEvent。

import type { SessionView } from '../state/store';
import type { UiPlan } from '../types/protocol';
import { presentTurnEnd, type TurnEnd, type TurnTone } from './turn';

export type InspectorMode = 'waiting' | 'running' | 'terminal' | 'idle';

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
  if (currentPlanProgress(s.plan)) sections.push('plan');
  if (s.verification && s.verification.checks.length > 0) sections.push('verification');
  if ((s.diff?.files.length ?? 0) > 0) sections.push('changes');
  if (s.agents.length > 0 || extras.delegatedAgents) sections.push('agents');
  if (extras.observation) sections.push('runtime');
  sections.push('more');
  return sections;
}

export function currentPlanProgress(
  plan: UiPlan | null | undefined,
): { current: number; total: number; description: string } | null {
  if (!plan || plan.steps.length === 0) return null;
  const running = plan.steps.find((s) => s.status === 'running');
  const step = running ?? plan.steps.find((s) => s.status === 'pending') ?? plan.steps[plan.steps.length - 1];
  return {
    current: step.index + 1,
    total: plan.steps.length,
    description: step.description,
  };
}
