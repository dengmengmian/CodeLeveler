import { describe, expect, it } from 'vitest';
import type { SessionView } from '../state/store';
import { completionTruth } from './completionTruth';

function session(over: Partial<SessionView> = {}): SessionView {
  return {
    id: 's1',
    title: 'Fix auth',
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

describe('completionTruth', () => {
  it('waiting beats a green lastTurn', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
        pendingApprovals: [{ id: 'a', tool: 'run_command', summary: 'rm', command: 'rm', risks: [] }],
      }),
    );
    expect(t?.kind).toBe('waiting');
    expect(t?.trust).toBe('pending');
    expect(t?.title).toBe('等待确认');
  });

  it('completed + verification passed is success/verified', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
        verification: { passed: true, checks: [{ name: 'cargo test', status: 'passed' }] },
        diff: { files: [{ path: 'a.rs', added: 2, removed: 1 }] },
      }),
    );
    expect(t?.kind).toBe('success');
    expect(t?.trust).toBe('verified');
    expect(t?.verify).toBe('passed');
    expect(t?.changesApplied).toBe(true);
    expect(t?.title).toBe('任务已完成');
    expect(t?.facts.some((f) => f.includes('1 files'))).toBe(true);
    expect(t?.facts).toContain('验证通过');
  });

  it('unverified is never success and never uses live tools', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'unverified', detail: 'verification unavailable', ms: 10 },
        tools: [{ id: 'c1', name: 'read_file', arguments: '{}', status: 'done', preview: null, durationMs: 1, parallel: false, seq: 1 }],
      }),
    );
    expect(t?.kind).toBe('warning');
    expect(t?.trust).toBe('unverified');
    expect(t?.title).toBe('已完成但未验证');
  });

  it('failed gate is verification failed + recovery', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'incomplete', detail: 'failed gate(s): cargo test', ms: 10 },
      }),
    );
    expect(t?.kind).toBe('warning');
    expect(t?.verify).toBe('failed');
    expect(t?.trust).toBe('failed');
    expect(t?.title).toBe('验证未通过');
  });

  it('failed outcome is failure with retry hint', () => {
    const t = completionTruth(session({ lastTurn: { outcome: 'failed', detail: 'boom', ms: 10 } }));
    expect(t?.kind).toBe('failure');
    expect(t?.recoveryHint).toBe('可重试上一轮');
    expect(t?.title).toBe('执行失败');
  });

  it('report-only files_changed does not invent +0 −0', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
        completionReport: {
          files_changed: 5,
          added: 0,
          removed: 0,
          checks_passed: 0,
          checks_total: 0,
          success: true,
        },
        diff: null,
      }),
    );
    expect(t?.changesApplied).toBe(true);
    expect(t?.artifacts.source).toBe('report');
    expect(t?.artifacts.files).toBe(5);
    expect(t?.artifacts.added).toBeNull();
    expect(t?.artifacts.removed).toBeNull();
    expect(t?.facts.join(' ')).toContain('5 files changed');
    expect(t?.facts.join(' ')).not.toMatch(/\+0/);
    expect(t?.facts.join(' ')).not.toMatch(/−0/);
  });

  it('a loaded empty diff is exact zero and does not fall back to the report', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
        completionReport: {
          files_changed: 5,
          added: 0,
          removed: 0,
          checks_passed: 0,
          checks_total: 0,
          success: true,
        },
        diff: { files: [] },
      }),
    );
    expect(t?.changesApplied).toBe(false);
    expect(t?.artifacts.source).toBe('none');
    expect(t?.facts).toContain('未改仓库');
    expect(t?.facts.join(' ')).not.toContain('5 files changed');
  });

  it('exact diff totals win when a real diff is present', () => {
    const t = completionTruth(
      session({
        lastTurn: { outcome: 'completed', detail: null, ms: 1000 },
        completionReport: {
          files_changed: 9,
          added: 10,
          removed: 4,
          checks_passed: 1,
          checks_total: 1,
          success: true,
        },
        diff: { files: [{ path: 'a.rs', added: 2, removed: 1 }] },
      }),
    );
    expect(t?.artifacts.source).toBe('diff');
    expect(t?.artifacts.files).toBe(1);
    expect(t?.artifacts.added).toBe(2);
    expect(t?.artifacts.removed).toBe(1);
    expect(t?.facts.some((f) => f.includes('1 files') && f.includes('+2') && f.includes('−1'))).toBe(true);
  });
});
