// 状态层契约测试：Runtime 事件必须真实进入 web state ——
// memory / sub-agent / background task / 产品轴 / 终态保真。

import { describe, expect, it } from 'vitest';
import type { UiSessionSnapshot } from '../types/protocol';
import { headerWaitingCue, inspectorMode } from '../lib/inspectorModel';
import { initialState, reducer, type AppState } from './store';

function snapshot(over: Partial<UiSessionSnapshot> = {}): UiSessionSnapshot {
  return {
    id: 's1',
    repository: '/repo',
    goal: 'g',
    model: null,
    mode: 'assisted',
    branch: null,
    status: 'idle',
    messages: [],
    ...over,
  };
}

function stateWithSession(over: Partial<UiSessionSnapshot> = {}): AppState {
  const state: AppState = structuredClone(initialState);
  reducer(state, { type: 'snapshot', session: snapshot(over) });
  return state;
}

describe('product axes', () => {
  it('adopts axes from the snapshot (runtime is the source of truth)', () => {
    const state = stateWithSession({ work_profile: 'delivery', collaboration: 'goal' });
    expect(state.current?.workProfile).toBe('delivery');
    expect(state.current?.collaboration).toBe('goal');
  });

  it('defaults to balanced/chat when an old runtime omits the fields', () => {
    const state = stateWithSession();
    expect(state.current?.workProfile).toBe('balanced');
    expect(state.current?.collaboration).toBe('chat');
  });

  it('set_axes updates the local view (optimistic, confirmed by session_updated)', () => {
    const state = stateWithSession();
    reducer(state, { type: 'set_axes', workProfile: 'economy', collaboration: 'plan' });
    expect(state.current?.workProfile).toBe('economy');
    expect(state.current?.collaboration).toBe('plan');
  });

  it('exposes the runtime-resolved reasoning effort, never inventing one', () => {
    const state = stateWithSession({ reasoning: { effective: 'max' } });
    expect(state.current?.reasoningEffort).toBe('max');
    const none = stateWithSession({ reasoning: { effective: null } });
    expect(none.current?.reasoningEffort).toBeNull();
  });
});

describe('chrome / diff focus', () => {
  it('focus_diff opens the changes workspace on that file', () => {
    const state = stateWithSession();
    reducer(state, { type: 'focus_diff', path: 'src/auth.rs' });
    expect(state.stageView).toBe('diff');
    expect(state.diffFocus).toBe('src/auth.rs');
  });

  it('toggle_inspector flips the drawer', () => {
    const state = structuredClone(initialState);
    expect(state.inspectorOpen).toBe(true);
    reducer(state, { type: 'toggle_inspector' });
    expect(state.inspectorOpen).toBe(false);
  });

  it('pending approval with a closed inspector still surfaces the waiting cue; opening Inspector is the action entry', () => {
    const state = stateWithSession({
      pending_interactions: [
        {
          type: 'approval',
          request: {
            id: 'a1',
            tool: 'run_command',
            summary: 'run rm',
            command: 'rm -rf build/',
            risks: [],
          },
        },
      ],
    });
    reducer(state, { type: 'set_inspector', open: false });
    expect(state.inspectorOpen).toBe(false);
    expect(inspectorMode(state.current)).toBe('waiting');
    expect(headerWaitingCue(state.current)).toEqual({ glyph: '⚠', label: '等待确认' });
    reducer(state, { type: 'set_inspector', open: true });
    expect(state.inspectorOpen).toBe(true);
  });

  it('pending clarification with a closed inspector surfaces 需要回答; opening Inspector is the action entry', () => {
    const state = stateWithSession({
      pending_interactions: [
        { type: 'clarification', request: { id: 'c1', question: 'old API?', options: [] } },
      ],
    });
    reducer(state, { type: 'set_inspector', open: false });
    expect(state.inspectorOpen).toBe(false);
    expect(headerWaitingCue(state.current)).toEqual({ glyph: '⚠', label: '需要回答' });
    reducer(state, { type: 'set_inspector', open: true });
    expect(state.inspectorOpen).toBe(true);
  });
});

describe('turn terminal truth', () => {
  it('keeps the full outcome and detail on lastTurn', () => {
    const state = stateWithSession();
    reducer(state, { type: 'turn_terminal', outcome: 'unverified', detail: 'verification unavailable' });
    expect(state.current?.lastTurn?.outcome).toBe('unverified');
    expect(state.current?.lastTurn?.detail).toBe('verification unavailable');
    expect(state.current?.turnActive).toBe(false);
  });

  it('incomplete stays incomplete', () => {
    const state = stateWithSession();
    reducer(state, { type: 'turn_terminal', outcome: 'incomplete', detail: 'budget_exhausted' });
    expect(state.current?.lastTurn?.outcome).toBe('incomplete');
  });
});

describe('memory', () => {
  it('memory_list lands in state including pending (K36 consent gate)', () => {
    const state = stateWithSession();
    reducer(state, {
      type: 'memory_list',
      dir: '/repo/.leveler/memory',
      active: [{ id: 'a1', title: 'prefer nextest' }],
      archived: [],
      pending: [{ id: 'p1', title: 'rust workspace' }],
    });
    expect(state.current?.memory?.dir).toBe('/repo/.leveler/memory');
    expect(state.current?.memory?.active).toHaveLength(1);
    expect(state.current?.memory?.pending[0]?.id).toBe('p1');
  });
});

describe('sub-agents', () => {
  it('sub_agent_updated creates a running block and completes it in place', () => {
    const state = stateWithSession();
    reducer(state, {
      type: 'sub_agent_updated',
      id: 'ag1',
      nickname: 'Explorer',
      role: 'explorer',
      done: false,
      ok: false,
      detail: '搜索认证实现',
    });
    expect(state.current?.agents).toHaveLength(1);
    expect(state.current?.agents[0]?.status).toBe('run');

    reducer(state, {
      type: 'sub_agent_updated',
      id: 'ag1',
      nickname: 'Explorer',
      role: 'explorer',
      done: true,
      ok: true,
      detail: '找到 3 处实现',
    });
    expect(state.current?.agents).toHaveLength(1);
    expect(state.current?.agents[0]?.status).toBe('done');
    expect(state.current?.agents[0]?.detail).toBe('找到 3 处实现');
  });

  it('a failed child is fail, not done', () => {
    const state = stateWithSession();
    reducer(state, { type: 'sub_agent_updated', id: 'ag1', nickname: 'W', role: 'worker', done: true, ok: false, detail: 'x' });
    expect(state.current?.agents[0]?.status).toBe('fail');
  });

  it('sub_agent_progress merges token usage; sub_agent_activity records the latest step', () => {
    const state = stateWithSession();
    reducer(state, { type: 'sub_agent_updated', id: 'ag1', nickname: 'W', role: 'worker', done: false, ok: false, detail: 't' });
    reducer(state, { type: 'sub_agent_progress', id: 'ag1', active: true, input: 2400, output: 180, cached: 1200 });
    expect(state.current?.agents[0]?.tokens).toEqual({ input: 2400, output: 180, cached: 1200 });
    reducer(state, { type: 'sub_agent_activity', id: 'ag1', step: 'cargo test ✓' });
    expect(state.current?.agents[0]?.recentStep).toBe('cargo test ✓');
  });

  it('a new user turn clears the previous turn agents (like tools)', () => {
    const state = stateWithSession();
    reducer(state, { type: 'sub_agent_updated', id: 'ag1', nickname: 'W', role: 'worker', done: true, ok: true, detail: 'x' });
    reducer(state, { type: 'user_message', id: 'm9', text: 'next', time: '10:00:00' });
    expect(state.current?.agents).toHaveLength(0);
  });
});

describe('background tasks', () => {
  it('tracks start and exit with real status', () => {
    const state = stateWithSession();
    reducer(state, { type: 'background_started', taskId: 'bg1', program: 'cargo', args: ['test'] });
    expect(state.current?.backgroundTasks[0]?.status).toBe('run');
    reducer(state, { type: 'background_exited', taskId: 'bg1', exitCode: 1, durationMs: 84_000, ok: false });
    expect(state.current?.backgroundTasks[0]?.status).toBe('fail');
    expect(state.current?.backgroundTasks[0]?.exitCode).toBe(1);
  });
});

describe('context events', () => {
  it('context estimate is a placeholder only until real usage arrives', () => {
    const state = stateWithSession();
    reducer(state, { type: 'context_estimate', tokens: 1234 });
    expect(state.current?.contextTokens).toBe(1234);
    reducer(state, { type: 'token_usage', input: 9000, output: 500 });
    expect(state.current?.contextTokens).toBe(9500);
    // A later estimate must not clobber a live reading.
    reducer(state, { type: 'context_estimate', tokens: 42 });
    expect(state.current?.contextTokens).toBe(9500);
  });
});

describe('reasoning stream', () => {
  it('accumulates deltas and supersedes the thought when a tool starts', () => {
    const state = stateWithSession();
    reducer(state, { type: 'reasoning_delta', delta: '先看 auth' });
    reducer(state, { type: 'reasoning_delta', delta: ' 模块' });
    expect(state.current?.reasoning).toBe('先看 auth 模块');
    reducer(state, { type: 'tool_started', id: 't1', name: 'read_file', arguments: '{}', parallel: false });
    reducer(state, { type: 'reasoning_delta', delta: '新想法' });
    expect(state.current?.reasoning).toBe('新想法');
  });
});
