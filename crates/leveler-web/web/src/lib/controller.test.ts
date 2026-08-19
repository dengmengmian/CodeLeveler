// 控制面契约测试：web 发出的命令必须是真实协议变体
// （set_product_axes / accept_memory / forget_memory），事件层不再丢弃
// memory / sub-agent / progress 事件。

import { beforeEach, describe, expect, it } from 'vitest';
import type { Action, AppState } from '../state/store';
import { initialState, reducer } from '../state/store';
import type { ClientCommand, RuntimeEvent, UpFrame } from '../types/protocol';
import { RuntimeBridge } from './controller';

// getToken 需要 window/sessionStorage；node 环境下补最小桩。
beforeEach(() => {
  const g = globalThis as Record<string, unknown>;
  g.window = {
    location: { href: 'http://localhost/', protocol: 'http:', host: 'localhost' },
    history: { replaceState: () => {} },
  };
  g.sessionStorage = {
    getItem: () => '',
    setItem: () => {},
    removeItem: () => {},
  };
});

interface Harness {
  bridge: RuntimeBridge;
  state: AppState;
  sent: ClientCommand[];
  apply: (ev: RuntimeEvent) => void;
}

function harness(): Harness {
  const state: AppState = structuredClone(initialState);
  const dispatch = (action: Action) => reducer(state, action);
  const bridge = new RuntimeBridge(dispatch, () => state);
  const sent: ClientCommand[] = [];
  // 拦截 WS 出站帧：只关心命令语义，不建真连接。
  (bridge as unknown as { ws: { send: (f: UpFrame) => boolean } }).ws = {
    send: (frame: UpFrame) => {
      if (frame.type === 'deliver') sent.push(frame.command);
      return true;
    },
  };
  reducer(state, {
    type: 'snapshot',
    session: {
      id: 's1',
      repository: '/repo',
      goal: 'g',
      model: null,
      mode: 'assisted',
      branch: null,
      status: 'idle',
      messages: [],
    },
  });
  const apply = (ev: RuntimeEvent) =>
    (bridge as unknown as { applyEvent: (ev: RuntimeEvent) => void }).applyEvent(ev);
  return { bridge, state, sent, apply };
}

describe('product axes commands', () => {
  it('setAxes sends set_product_axes (the real protocol variant)', () => {
    const { bridge, sent, state } = harness();
    bridge.setAxes('delivery', 'goal');
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({
      type: 'set_product_axes',
      session_id: 's1',
      work_profile: 'delivery',
      collaboration: 'goal',
    });
    expect(state.current?.workProfile).toBe('delivery');
    expect(state.current?.collaboration).toBe('goal');
  });

  it('axes cannot change mid-turn (idle-only, TUI parity)', () => {
    const { bridge, sent, state } = harness();
    reducer(state, { type: 'turn_active', value: true });
    bridge.setAxes('economy', 'chat');
    expect(sent).toHaveLength(0);
    expect(state.current?.workProfile).toBe('balanced');
  });

  it('slash /work-mode and /collab drive the axes', () => {
    const { bridge, sent } = harness();
    bridge.runSlash('/work-mode delivery');
    bridge.runSlash('/collab goal');
    expect(sent.map((c) => c.type)).toEqual(['set_product_axes', 'set_product_axes']);
    expect(sent[1]).toMatchObject({ work_profile: 'delivery', collaboration: 'goal' });
  });
});

describe('memory commands', () => {
  it('accept/forget send the user-authoritative variants then refresh the list', () => {
    const { bridge, sent } = harness();
    bridge.acceptMemory('p1');
    bridge.forgetMemory('a1');
    expect(sent.map((c) => c.type)).toEqual([
      'accept_memory',
      'list_memory',
      'forget_memory',
      'list_memory',
    ]);
  });
});

describe('query observability', () => {
  it('sends query_observability and stores ObservabilityLoaded off live tools', () => {
    const { bridge, sent, apply, state } = harness();
    bridge.queryObservability('s1');
    expect(sent[sent.length - 1]).toEqual({
      type: 'query_observability',
      session_id: 's1',
      before: 0,
      after: 80,
    });
    apply({
      type: 'observability_loaded',
      observation: {
        agents: [],
        recovery: { interrupted_turns: 0, repair_attempts: 0, workspace_snapshots: 0, review_stages: [] },
        requests: [],
        tools: [{ name: 'read_file', class: 'read', calls: 40, succeeded: 40, failed: 0, unfinished: 0 }],
        window: [],
        window_from: 1,
        window_to: 2,
        session: {
          session_id: 's1',
          goal: 'fix auth',
          repository: '/repo',
          created_at: 't',
          updated_at: 't',
          status: 'completed',
          model: 'deepseek/v4',
          work_profile: 'balanced',
          collaboration: 'chat',
          request_count: 3,
          input_tokens: 10,
          output_tokens: 2,
          request_failures: 0,
          request_retries: 0,
          tool_started: 21,
          tool_finished: 21,
          verification_runs: 1,
          compact_count: 0,
          subagent_started: 0,
          repair_started: 0,
        },
      },
    });
    expect(state.observation?.session.tool_started).toBe(21);
    expect(state.current?.tools).toEqual([]);
  });
});

describe('event closure', () => {
  it('memory_list reaches state (was silently dropped)', () => {
    const { apply, state } = harness();
    apply({
      type: 'memory_list',
      memory_dir: '/m',
      active: [{ id: 'a', title: 't' }],
      archived: [],
      pending: [{ id: 'p', title: 'q' }],
    });
    expect(state.current?.memory?.pending).toHaveLength(1);
  });

  it('sub_agent events reach state', () => {
    const { apply, state } = harness();
    apply({ type: 'sub_agent_updated', id: 'ag', nickname: 'W', role: 'worker', done: false, ok: false, detail: 'task' });
    apply({ type: 'sub_agent_progress', id: 'ag', active: true, input_tokens: 10, output_tokens: 2, cached_input_tokens: 0 });
    apply({ type: 'sub_agent_activity', id: 'ag', phase: 'tool_finished', tool: 'cargo test', preview: '', is_error: false });
    expect(state.current?.agents[0]?.tokens.input).toBe(10);
    expect(state.current?.agents[0]?.recentStep).toBe('cargo test ✓');
  });

  it('turn_incomplete does NOT surface as completed', () => {
    const { apply, state } = harness();
    apply({ type: 'turn_incomplete', reason: 'budget' });
    expect(state.current?.lastTurn?.outcome).toBe('incomplete');
    expect(state.current?.lastTurn?.detail).toBe('budget');
  });

  it('command_progress / turn_progress land in the activity slot', () => {
    const { apply, state } = harness();
    apply({ type: 'command_progress', label: 'cargo test', elapsed_ms: 61_000 });
    expect(state.current?.activity).toBe('运行 cargo test · 01:01');
    apply({ type: 'turn_progress', phase: 'verification', closing: true, no_progress_streak: 0, closeout_deny_rounds: 0 });
    expect(state.current?.activity).toBe('收口中 · verification');
  });
});
