import { describe, expect, it } from 'vitest';
import type { UiObservabilityLoaded, UiObservationRow } from '../types/protocol';
import {
  groupByTurn,
  projectAgentDelegation,
  projectObservability,
  projectRow,
  shouldRefreshObservability,
} from './observabilityView';

function row(over: Partial<UiObservationRow> & Pick<UiObservationRow, 'sequence' | 'class' | 'title'>): UiObservationRow {
  return {
    created_at: '2026-08-19T10:20:10.000Z',
    status: 'ok',
    event_type: 'tool_call_finished',
    target: '',
    ...over,
  };
}

function loaded(over: Partial<UiObservabilityLoaded> = {}): UiObservabilityLoaded {
  return {
    agents: [],
    recovery: {
      interrupted_turns: 0,
      repair_attempts: 0,
      workspace_snapshots: 0,
      review_stages: [],
    },
    requests: [],
    tools: [{ name: 'read_file', class: 'read', calls: 40, succeeded: 40, failed: 0, unfinished: 0 }],
    window: [],
    window_from: 1,
    window_to: 12,
    session: {
      session_id: 's1',
      goal: 'fix auth',
      repository: '/repo',
      created_at: '2026-08-19T10:20:00.000Z',
      updated_at: '2026-08-19T10:33:00.000Z',
      status: 'completed',
      model: 'deepseek/v4',
      work_profile: 'balanced',
      collaboration: 'chat',
      last_sequence: 12,
      request_count: 3,
      input_tokens: 1000,
      output_tokens: 40,
      request_failures: 0,
      request_retries: 0,
      tool_started: 21,
      tool_finished: 21,
      verification_runs: 1,
      compact_count: 0,
      subagent_started: 0,
      repair_started: 0,
    },
    ...over,
  };
}

describe('observability projection', () => {
  it('maps a tool row to a view step without inventing duration', () => {
    const step = projectRow(
      row({
        sequence: 4,
        class: 'read',
        title: 'read_file',
        target: 'src/auth.rs',
        status: 'ok',
        duration_ms: 41,
        turn_id: 't1',
      }),
    );
    expect(step.kind).toBe('tool');
    expect(step.title).toBe('read_file');
    expect(step.detail).toBe('src/auth.rs');
    expect(step.durationMs).toBe(41);
    expect(step.turnId).toBe('t1');
  });

  it('groups steps by durable turn_id, not wall-clock', () => {
    const groups = groupByTurn([
      projectRow(row({ sequence: 1, class: 'terminal', title: 'turn started', turn_id: 't1' })),
      projectRow(row({ sequence: 3, class: 'read', title: 'read_file', turn_id: 't1' })),
      projectRow(row({ sequence: 8, class: 'terminal', title: 'turn started', turn_id: 't2' })),
    ]);
    expect(groups).toHaveLength(2);
    expect(groups[0].turnId).toBe('t1');
    expect(groups[0].steps).toHaveLength(2);
    expect(groups[1].turnId).toBe('t2');
  });

  it('session tool totals come from observatory session fields, not the window', () => {
    const view = projectObservability(
      loaded({
        window: [
          row({ sequence: 10, class: 'read', title: 'read_file', turn_id: 't1' }),
          row({ sequence: 11, class: 'search', title: 'grep', turn_id: 't1' }),
        ],
      }),
    );
    expect(view.summary.toolStarted).toBe(21);
    expect(view.summary.verificationRuns).toBe(1);
    expect(view.summary.requestCount).toBe(3);
    expect(view.tools[0]?.calls).toBe(40);
    expect(view.groups[0]?.steps).toHaveLength(2);
    expect(view.summary.durationMs).toBe(13 * 60 * 1000);
  });

  it('refreshes on the same live events as TUI /trace, not on observability_loaded', () => {
    expect(shouldRefreshObservability({ type: 'tool_call_completed', id: 'c', ok: true, preview: '', duration_ms: 1 })).toBe(
      true,
    );
    expect(shouldRefreshObservability({ type: 'observability_loaded', observation: loaded() })).toBe(false);
  });

  it('projects running / completed / failed agents from protocol status only', () => {
    const views = projectAgentDelegation([
      { id: 'agent-1', nickname: 'Explorer', role: 'explorer', status: 'running', summary: 'Inspect authentication flow' },
      { id: 'agent-2', nickname: 'Worker', role: 'worker', status: 'ok', summary: 'tests added' },
      { id: 'agent-3', nickname: 'Worker', role: 'worker', status: 'fail', summary: 'compile error' },
    ]);
    expect(views).toEqual([
      {
        id: 'agent-1',
        nickname: 'Explorer',
        role: 'explorer',
        status: 'running',
        task: 'Inspect authentication flow',
        summary: null,
      },
      {
        id: 'agent-2',
        nickname: 'Worker',
        role: 'worker',
        status: 'completed',
        task: null,
        summary: 'tests added',
      },
      {
        id: 'agent-3',
        nickname: 'Worker',
        role: 'worker',
        status: 'failed',
        task: null,
        summary: 'compile error',
      },
    ]);
  });

  it('does not invent a finished agent task that the protocol already overwrote', () => {
    const [view] = projectAgentDelegation([
      { id: 'agent-1', nickname: 'Explorer', role: 'explorer', status: 'ok', summary: 'auth flow mapped' },
    ]);
    expect(view?.task).toBeNull();
    expect(view?.summary).toBe('auth flow mapped');
  });

  it('session-wide agent count comes from QueryObservability.agents, not a live list', () => {
    const view = projectObservability(
      loaded({
        agents: [
          { id: 'agent-1', nickname: 'Explorer', role: 'explorer', status: 'ok', summary: 'done' },
          { id: 'agent-2', nickname: 'Worker', role: 'worker', status: 'running', summary: 'Add tests' },
        ],
        session: { ...loaded().session, subagent_started: 2 },
      }),
    );
    expect(view.agents).toHaveLength(2);
    expect(view.summary.delegatedAgents).toBe(2);
  });

  it('tool attribution uses Agent field; Main is unlabeled', () => {
    const view = projectObservability(
      loaded({
        agents: [{ id: 'agent-1', nickname: 'Explorer', role: 'explorer', status: 'ok', summary: 'done' }],
        window: [
          row({
            sequence: 4,
            class: 'read',
            title: 'read_file',
            target: 'src/auth.rs',
            turn_id: 't1',
            fields: [
              { key: 'Call', value: 'c1' },
              { key: 'Agent', value: 'agent-1' },
            ],
          }),
          row({
            sequence: 5,
            class: 'shell',
            title: 'run_command',
            turn_id: 't1',
            fields: [{ key: 'Agent', value: 'main' }],
          }),
          row({
            sequence: 6,
            class: 'search',
            title: 'grep',
            turn_id: 't1',
            fields: [{ key: 'Agent', value: 'agent-9' }],
          }),
        ],
      }),
    );
    expect(view.groups[0]?.steps[0]?.agentLabel).toBe('Explorer');
    expect(view.groups[0]?.steps[1]?.agentLabel).toBeNull();
    expect(view.groups[0]?.steps[2]?.agentLabel).toBe('agent-9');
  });
});
