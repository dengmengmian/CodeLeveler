import { describe, expect, it } from 'vitest';
import type { SessionView } from '../state/store';
import { inspectorMode, inspectorTerminalTone, currentPlanProgress } from './inspectorModel';
import type { TurnOutcome } from './turn';

function session(over: Partial<SessionView> = {}): SessionView {
  return {
    id: 's1',
    title: 'Refactor auth',
    repository: '/repo',
    branch: 'main',
    status: 'idle',
    messages: [],
    tools: [],
    agents: [],
    backgroundTasks: [],
    pendingApprovals: [],
    pendingClarifications: [],
    plan: null,
    verification: null,
    diff: null,
    checkpoints: [],
    completionReport: null,
    memory: null,
    turnActive: false,
    activity: null,
    reasoning: '',
    reasoningSuperseded: false,
    turnStartedAt: null,
    lastTurn: null,
    model: null,
    availableModels: [],
    permission: 'assisted',
    workProfile: 'balanced',
    collaboration: 'chat',
    reasoningEffort: null,
    tokens: { input: 0, output: 0 },
    contextTokens: 0,
    contextWindow: null,
    ...over,
  };
}

describe('inspectorMode', () => {
  it('waiting beats running', () => {
    const s = session({
      turnActive: true,
      pendingApprovals: [
        {
          id: 'a1',
          tool: 'run_command',
          summary: 'run rm',
          command: 'rm -rf build/',
          risks: [],
        },
      ],
    });
    expect(inspectorMode(s)).toBe('waiting');
  });

  it('waiting on clarification', () => {
    const s = session({
      turnActive: true,
      pendingClarifications: [{ id: 'c1', question: 'old API?', options: [] }],
    });
    expect(inspectorMode(s)).toBe('waiting');
  });

  it('running when turn is active and nothing is pending', () => {
    expect(inspectorMode(session({ turnActive: true, activity: 'cargo test' }))).toBe('running');
  });

  it('terminal after lastTurn', () => {
    expect(inspectorMode(session({ lastTurn: { outcome: 'completed', detail: null, ms: 4200 } }))).toBe(
      'terminal',
    );
  });

  it('idle when empty', () => {
    expect(inspectorMode(session())).toBe('idle');
    expect(inspectorMode(null)).toBe('idle');
  });
});

describe('inspectorTerminalTone', () => {
  const cases: Array<[TurnOutcome, string | null, 'success' | 'calm' | 'warn' | 'error' | 'muted']> = [
    ['completed', null, 'success'],
    ['unverified', 'no verification', 'warn'],
    ['incomplete', 'budget_exhausted', 'warn'],
    ['failed', 'boom', 'error'],
    ['cancelled', null, 'muted'],
  ];
  for (const [outcome, detail, tone] of cases) {
    it(`${outcome} maps to ${tone}, never fake-success for unverified`, () => {
      expect(inspectorTerminalTone({ outcome, detail })).toBe(tone);
    });
  }
});

describe('currentPlanProgress', () => {
  it('reports the running step', () => {
    const p = currentPlanProgress({
      steps: [
        { index: 0, description: 'explore', status: 'done' },
        { index: 1, description: 'implement', status: 'running' },
        { index: 2, description: 'verify', status: 'pending' },
      ],
    });
    expect(p).toEqual({ current: 2, total: 3, description: 'implement' });
  });

  it('returns null without a plan', () => {
    expect(currentPlanProgress(null)).toBeNull();
  });
});
