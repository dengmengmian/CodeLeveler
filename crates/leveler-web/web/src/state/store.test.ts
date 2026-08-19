// 状态层契约测试：Runtime 事件必须真实进入 web state ——
// memory / sub-agent / background task / 产品轴 / 终态保真。

import { describe, expect, it } from 'vitest';
import type { ProjectInfo, UiObservabilityLoaded, UiSessionSnapshot } from '../types/protocol';
import { sessionsForProject } from '../lib/projectScope';
import { headerWaitingCue, inspectorMode } from '../lib/inspectorModel';
import { initialState, reducer, type AppState } from './store';

function observation(over: Partial<UiObservabilityLoaded> = {}): UiObservabilityLoaded {
  return {
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
      last_sequence: 12,
    },
    ...over,
  };
}

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

function project(path: string, over: Partial<ProjectInfo> = {}): ProjectInfo {
  return { path, name: path.split('/').pop() ?? path, status: 'online', sessions: 0, ...over };
}

describe('project → sessions', () => {
  it('selecting a project filters the session list to that repository', () => {
    const state = structuredClone(initialState);
    reducer(state, {
      type: 'projects',
      projects: [project('/A', { name: 'Alpha' }), project('/B', { name: 'Beta' })],
    });
    reducer(state, {
      type: 'session_list',
      sessions: [
        { id: 'a1', goal: 'fix A', status: 'idle', model: '', updated_at: 't', repository: '/A' },
        { id: 'b1', goal: 'fix B', status: 'idle', model: '', updated_at: 't', repository: '/B' },
      ],
    });
    reducer(state, { type: 'select_project', path: '/A' });
    expect(state.selectedProject).toBe('/A');
    expect(sessionsForProject(state.sessions, state.selectedProject).map((s) => s.id)).toEqual(['a1']);
  });

  it('new_draft targets the selected project', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'select_project', path: '/A' });
    reducer(state, { type: 'new_draft', project: '/A' });
    expect(state.draft).toBe(true);
    expect(state.draftProject).toBe('/A');
    expect(state.selectedProject).toBe('/A');
    expect(state.current).toBeNull();
  });

  it('switching project clears the other project session and observability ownership', () => {
    const state = stateWithSession({ id: 'a1', repository: '/A' });
    reducer(state, { type: 'observation_loading', queryId: 'q-a' });
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q-a',
      observation: observation({ session: { ...observation().session, session_id: 'a1' } }),
    });
    expect(state.observation).not.toBeNull();
    reducer(state, { type: 'select_project', path: '/B' });
    expect(state.selectedProject).toBe('/B');
    expect(state.current).toBeNull();
    expect(state.draft).toBe(true);
    expect(state.draftProject).toBe('/B');
    expect(state.observation).toBeNull();
    expect(state.pendingObservationQuery).toBeNull();
    expect(state.observationStatus).toBe('idle');
  });

  it('a late observability payload for the previous session is dropped', () => {
    const state = stateWithSession({ id: 'a1', repository: '/A' });
    reducer(state, { type: 'observation_loading', queryId: 'q-a' });
    reducer(state, { type: 'select_project', path: '/B' });
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q-a',
      observation: observation({ session: { ...observation().session, session_id: 'a1' } }),
    });
    expect(state.observation).toBeNull();
    expect(state.pendingObservationQuery).toBeNull();
  });

  it('opening a session snapshot aligns the selected project to its repository', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'select_project', path: '/A' });
    reducer(state, { type: 'snapshot', session: snapshot({ id: 'b1', repository: '/B' }) });
    expect(state.selectedProject).toBe('/B');
    expect(state.current?.id).toBe('b1');
  });

  it('records offline project status without inventing online', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'projects', projects: [project('/A', { status: 'offline' })] });
    expect(state.projects[0]?.status).toBe('offline');
    reducer(state, { type: 'project_status', path: '/A', status: 'starting' });
    expect(state.projects[0]?.status).toBe('starting');
  });

  it('clears pending attachments when leaving a project session', () => {
    const state = stateWithSession({ id: 'a1', repository: '/A' });
    reducer(state, {
      type: 'attachment_added',
      attachment: {
        id: 'att1',
        name: 'note.txt',
        mime_type: 'text/plain',
        kind: 'text_file',
        sha256: 'x',
        size_bytes: 1,
      },
    });
    expect(state.pendingAttachments).toHaveLength(1);
    reducer(state, { type: 'select_project', path: '/B' });
    expect(state.pendingAttachments).toEqual([]);
  });

  it('keeps the open session when re-selecting its project', () => {
    const state = stateWithSession({ id: 'a1', repository: '/A' });
    reducer(state, { type: 'select_project', path: '/A' });
    expect(state.current?.id).toBe('a1');
    expect(state.draft).toBe(false);
  });

  it('falls back to another listed project when the selected one disappears', () => {
    const state = stateWithSession({ id: 'b1', repository: '/B' });
    reducer(state, {
      type: 'projects',
      projects: [project('/A'), project('/B')],
    });
    expect(state.selectedProject).toBe('/B');
    reducer(state, { type: 'projects', projects: [project('/A')] });
    expect(state.selectedProject).toBe('/A');
    expect(state.current).toBeNull();
    expect(state.draft).toBe(true);
    expect(state.observation).toBeNull();
  });

  it('new_draft without a path uses the selected project', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'select_project', path: '/A' });
    reducer(state, { type: 'new_draft' });
    expect(state.draftProject).toBe('/A');
    expect(state.selectedProject).toBe('/A');
    expect(state.draft).toBe(true);
  });
});

describe('chrome / diff focus', () => {
  it('focus_diff opens the changes workspace on that file', () => {
    const state = stateWithSession();
    reducer(state, { type: 'focus_diff', path: 'src/auth.rs' });
    expect(state.stageView).toBe('diff');
    expect(state.railNav).toBe('changes');
    expect(state.diffFocus).toBe('src/auth.rs');
  });

  it('rail activity only switches the execution workspace slot', () => {
    const state = stateWithSession();
    reducer(state, { type: 'set_rail_nav', nav: 'activity' });
    expect(state.railNav).toBe('activity');
    expect(state.stageView).toBe('execution');
  });

  it('sessions rail does not steal the workspace surface', () => {
    const state = stateWithSession();
    reducer(state, { type: 'stage_view', view: 'diff' });
    reducer(state, { type: 'set_rail_nav', nav: 'sessions' });
    expect(state.railNav).toBe('sessions');
    expect(state.stageView).toBe('diff');
  });

  it('execution workspace tab is a placeholder stage, not a protocol query', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'stage_view', view: 'execution' });
    expect(state.stageView).toBe('execution');
    expect(state.railNav).toBe('activity');
  });

  function acceptObservation(state: AppState, obs: UiObservabilityLoaded, queryId: string): void {
    reducer(state, { type: 'observation_loading', queryId });
    reducer(state, { type: 'observation_loaded', observation: obs, queryId });
  }

  it('stores QueryObservability payload off SessionView.tools', () => {
    const state = stateWithSession();
    acceptObservation(state, observation(), 'q1');
    expect(state.observation?.session.tool_started).toBe(21);
    expect(state.observation?.tools[0]?.calls).toBe(40);
    expect(state.current?.tools).toEqual([]);
  });

  it('drops an older query after a newer one has been accepted', () => {
    const state = stateWithSession();
    acceptObservation(
      state,
      observation({ session: { ...observation().session, last_sequence: 70 } }),
      'q-b',
    );
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q-a',
      observation: observation({
        tools: [{ name: 'read_file', class: 'read', calls: 1, succeeded: 1, failed: 0, unfinished: 0 }],
        session: { ...observation().session, last_sequence: 50 },
      }),
    });
    expect(state.observation?.session.last_sequence).toBe(70);
    expect(state.observationStatus).toBe('ready');
  });

  it('does not let a same-sequence historical query overwrite the owned tail', () => {
    const state = stateWithSession();
    acceptObservation(
      state,
      observation({
        window_from: 21,
        window_to: 100,
        session: { ...observation().session, last_sequence: 100 },
      }),
      'web-tail',
    );
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'tui-inspect-40',
      observation: observation({
        window_from: 20,
        window_to: 60,
        session: { ...observation().session, last_sequence: 100 },
      }),
    });
    expect(state.observation?.window_from).toBe(21);
    expect(state.observation?.window_to).toBe(100);
  });

  it('accepts a refresh of the currently owned query at the same last_sequence', () => {
    const state = stateWithSession();
    acceptObservation(
      state,
      observation({
        tools: [{ name: 'read_file', class: 'read', calls: 40, succeeded: 40, failed: 0, unfinished: 0 }],
        session: { ...observation().session, last_sequence: 100 },
      }),
      'q-a',
    );
    acceptObservation(
      state,
      observation({
        tools: [{ name: 'grep', class: 'search', calls: 2, succeeded: 2, failed: 0, unfinished: 0 }],
        session: { ...observation().session, last_sequence: 100 },
      }),
      'q-b',
    );
    expect(state.observation?.tools[0]?.name).toBe('grep');
  });

  it('drops a late response from a superseded Web query at the same last_sequence', () => {
    const state = stateWithSession();
    reducer(state, { type: 'observation_loading', queryId: 'q-a' });
    reducer(state, { type: 'observation_loading', queryId: 'q-b' });
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q-b',
      observation: observation({
        window_from: 21,
        window_to: 100,
        session: { ...observation().session, last_sequence: 100 },
      }),
    });
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q-a',
      observation: observation({
        window_from: 1,
        window_to: 80,
        session: { ...observation().session, last_sequence: 100 },
      }),
    });
    expect(state.observation?.window_from).toBe(21);
    expect(state.observation?.window_to).toBe(100);
  });

  it('ignores a legacy observability payload with no query_id', () => {
    const state = stateWithSession();
    reducer(state, { type: 'observation_loading', queryId: 'q1' });
    reducer(state, {
      type: 'observation_loaded',
      queryId: null,
      observation: observation({
        window_from: 20,
        window_to: 60,
        session: { ...observation().session, last_sequence: 100 },
      }),
    });
    expect(state.observation).toBeNull();
  });

  it('still ignores observability for a different session', () => {
    const state = stateWithSession();
    reducer(state, { type: 'observation_loading', queryId: 'q1' });
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q1',
      observation: observation({
        session: { ...observation().session, session_id: 'other', last_sequence: 99 },
      }),
    });
    expect(state.observation).toBeNull();
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

  it('live SessionView.agents is not the session-wide agent history', () => {
    const state = stateWithSession();
    reducer(state, { type: 'observation_loading', queryId: 'q-hist' });
    reducer(state, {
      type: 'observation_loaded',
      queryId: 'q-hist',
      observation: observation({
        agents: [
          { id: 'agent-1', nickname: 'Explorer', role: 'explorer', status: 'ok', summary: 'done' },
          { id: 'agent-2', nickname: 'Worker', role: 'worker', status: 'ok', summary: 'done' },
        ],
        session: { ...observation().session, subagent_started: 2, last_sequence: 40 },
      }),
    });
    reducer(state, {
      type: 'sub_agent_updated',
      id: 'agent-3',
      nickname: 'Worker',
      role: 'worker',
      done: false,
      ok: false,
      detail: 'Add tests',
    });
    expect(state.current?.agents).toHaveLength(1);
    expect(state.observation?.agents).toHaveLength(2);
    reducer(state, { type: 'user_message', id: 'm9', text: 'next', time: '10:00:00' });
    expect(state.current?.agents).toHaveLength(0);
    expect(state.observation?.agents).toHaveLength(2);
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
