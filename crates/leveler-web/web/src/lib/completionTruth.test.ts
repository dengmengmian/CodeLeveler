import { describe, expect, it } from 'vitest';
import type { SessionView } from '../state/store';
import { completionTruth } from './completionTruth';

function session(over: Partial<SessionView> = {}): SessionView {
  return {
    id: 's1', title: 'Fix auth', repository: '/repo', branch: 'main', status: 'idle',
    messages: [], tools: [], traces: [], agents: [], backgroundTasks: [],
    pendingApprovals: [], pendingClarifications: [], plan: null, diff: null,
    checkpoints: [], completionReport: null, memory: null, turnActive: false,
    activity: null, reasoning: '', reasoningSuperseded: false, turnStartedAt: null,
    lastTurn: null, model: null, availableModels: [], permission: 'assisted',
    workProfile: 'balanced', collaboration: 'chat', reasoningEffort: null,
    tokens: { input: 0, output: 0 }, contextTokens: 0, contextWindow: null,
    ...over,
  };
}

describe('completionTruth', () => {
  it('waiting beats a completed last turn', () => {
    const truth = completionTruth(session({
      lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
      pendingApprovals: [{ id: 'a', tool: 'run_command', summary: 'rm', command: 'rm', risks: [] }],
    }));
    expect(truth?.kind).toBe('waiting');
    expect(truth?.title).toBe('等待确认');
  });

  it('completed work reports artifacts without a verification conclusion', () => {
    const truth = completionTruth(session({
      lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
      diff: { files: [{ path: 'a.rs', added: 2, removed: 1 }] },
    }));
    expect(truth?.kind).toBe('success');
    expect(truth?.title).toBe('任务已完成');
    expect(truth?.verify).toBe('none');
    expect(truth?.facts).toEqual(['1 files  +2 −1']);
  });

  it('report-only file counts do not invent line totals', () => {
    const truth = completionTruth(session({
      lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
      completionReport: { files_changed: 5, added: 0, removed: 0, success: true },
    }));
    expect(truth?.artifacts).toEqual({ files: 5, added: null, removed: null, source: 'report' });
    expect(truth?.facts).toEqual(['5 files changed']);
  });

  it('failed work keeps its retry hint', () => {
    const truth = completionTruth(session({ lastTurn: { outcome: 'failed', detail: 'boom', ms: 10 } }));
    expect(truth?.kind).toBe('failure');
    expect(truth?.recoveryHint).toBe('可重试上一轮');
  });
});
