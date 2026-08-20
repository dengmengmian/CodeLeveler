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
  wsSession: { id: string | null };
}

function harness(): Harness {
  const state: AppState = structuredClone(initialState);
  const dispatch = (action: Action) => reducer(state, action);
  const bridge = new RuntimeBridge(dispatch, () => state);
  const sent: ClientCommand[] = [];
  const wsSession = { id: 's1' as string | null };
  // 拦截 WS 出站帧：只关心命令语义，不建真连接。
  (bridge as unknown as {
    ws: {
      send: (f: UpFrame) => boolean;
      setSession: (id: string | null) => void;
    };
  }).ws = {
    send: (frame: UpFrame) => {
      if (frame.type === 'deliver') sent.push(frame.command);
      return true;
    },
    setSession: (id: string | null) => {
      wsSession.id = id;
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
  return { bridge, state, sent, apply, wsSession };
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

describe('new session', () => {
  it('newDraft without an argument targets the selected project', () => {
    const { bridge, state } = harness();
    reducer(state, { type: 'select_project', path: '/A' });
    bridge.newDraft();
    expect(state.draft).toBe(true);
    expect(state.draftProject).toBe('/A');
    expect(state.selectedProject).toBe('/A');
  });
});

describe('project switch isolation', () => {
  it('leaving a project unsubscribes the previous session websocket', () => {
    const { bridge, state, wsSession } = harness();
    expect(state.current?.repository).toBe('/repo');
    bridge.selectProject('/B');
    expect(state.current).toBeNull();
    expect(state.draft).toBe(true);
    expect(wsSession.id).toBeNull();
  });

  it('keeps the websocket on the open session when re-selecting its project', () => {
    const { bridge, state, wsSession } = harness();
    bridge.selectProject('/repo');
    expect(state.current?.id).toBe('s1');
    expect(wsSession.id).toBe('s1');
  });

  it('newDraft unsubscribes so the draft is not still bound to the left session', () => {
    const { bridge, wsSession } = harness();
    bridge.newDraft('/A');
    expect(wsSession.id).toBeNull();
  });

  it('drops late events from the left session after a project switch', () => {
    const { bridge, state, apply } = harness();
    bridge.selectProject('/B');
    apply({ type: 'assistant_text_delta', message_id: 'm1', delta: 'leak from A' });
    expect(state.current).toBeNull();
    expect(state.observation).toBeNull();
  });

  it('does not adopt a late snapshot of the left session while drafting on another project', () => {
    const { bridge, state } = harness();
    bridge.selectProject('/B');
    (
      bridge as unknown as {
        applySnapshot: (snap: {
          id: string;
          repository: string;
          goal: string;
          model: null;
          mode: 'assisted';
          branch: null;
          status: string;
          messages: Array<{ id: string; role: 'assistant'; text: string }>;
        }) => void;
      }
    ).applySnapshot({
      id: 's1',
      repository: '/repo',
      goal: 'g',
      model: null,
      mode: 'assisted',
      branch: null,
      status: 'running',
      messages: [{ id: 'm', role: 'assistant', text: 'leaked' }],
    });
    expect(state.current).toBeNull();
    expect(state.draft).toBe(true);
  });

  it('session mutations stay ClientCommand on the session id', () => {
    const { bridge, sent } = harness();
    bridge.renameSession('s1', 'new title');
    bridge.archiveSession('s1');
    bridge.forkSession('s1');
    bridge.deleteSession('s1');
    expect(sent.map((c) => c.type)).toEqual([
      'rename_session',
      'request_session_list',
      'archive_session',
      'request_session_list',
      'fork_session',
      'request_session_list',
      'delete_session',
      'request_session_list',
    ]);
    expect(sent[0]).toMatchObject({ type: 'rename_session', session_id: 's1', name: 'new title' });
    expect(sent[2]).toMatchObject({ type: 'archive_session', session_id: 's1' });
    expect(sent[4]).toMatchObject({ type: 'fork_session', session_id: 's1' });
    expect(sent[6]).toMatchObject({ type: 'delete_session', session_id: 's1' });
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

describe('session_updated vs session_opened', () => {
  it('session_updated merges metadata and does not replace completed tools', () => {
    const { apply, state } = harness();
    reducer(state, {
      type: 'tool_started',
      id: 't1',
      name: 'read_file',
      arguments: '{"path":"README.md"}',
      parallel: false,
    });
    reducer(state, { type: 'tool_completed', id: 't1', ok: true, preview: 'ok', durationMs: 8 });
    apply({
      type: 'session_updated',
      session: {
        id: 's1',
        repository: '/repo',
        goal: 'g',
        model: null,
        mode: 'full_access',
        branch: 'main',
        status: 'idle',
        messages: [],
        active_tools: [],
        work_profile: 'delivery',
        collaboration: 'goal',
      },
    });
    expect(state.current?.tools).toHaveLength(1);
    expect(state.current?.tools[0]?.status).toBe('done');
    expect(state.current?.permission).toBe('full_access');
    expect(state.current?.workProfile).toBe('delivery');
    expect(state.current?.collaboration).toBe('goal');
  });

  it('session_opened still replaces the session view from the snapshot', () => {
    const { apply, state } = harness();
    reducer(state, {
      type: 'tool_started',
      id: 't1',
      name: 'read_file',
      arguments: '{}',
      parallel: false,
    });
    apply({
      type: 'session_opened',
      session: {
        id: 's1',
        repository: '/repo',
        goal: 'g',
        model: null,
        mode: 'assisted',
        branch: null,
        status: 'idle',
        messages: [],
        active_tools: [],
      },
    });
    expect(state.current?.tools).toHaveLength(0);
  });
});

describe('query observability', () => {
  it('sends query_observability and stores ObservabilityLoaded off live tools', () => {
    const { bridge, sent, apply, state } = harness();
    bridge.queryObservability('s1');
    const sentQuery = sent[sent.length - 1];
    expect(sentQuery).toMatchObject({
      type: 'query_observability',
      session_id: 's1',
      before: 0,
      after: 80,
    });
    expect(sentQuery.type === 'query_observability' && sentQuery.query_id).toBeTruthy();
    const queryId = sentQuery.type === 'query_observability' ? sentQuery.query_id : '';
    apply({
      type: 'observability_loaded',
      query_id: queryId,
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
