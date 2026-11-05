// 自动化控制面契约：所有应答必须属于本客户端发起的 query，写入串行，
// 刷新在写后重新读运行时——UI 不自己推断任何一条定时任务事实。

import { beforeEach, describe, expect, it } from 'vitest';
import type { Action, AppState } from '../state/store';
import { initialState, reducer } from '../state/store';
import type {
  ClientCommand,
  RuntimeEvent,
  ScheduleResponse,
  ScheduleRunView,
  ScheduleStatusView,
  ScheduleView,
  UpFrame,
} from '../types/protocol';
import { RuntimeBridge } from './controller';

const localStore = new Map<string, string>();

beforeEach(() => {
  localStore.clear();
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
  g.localStorage = {
    getItem: (key: string) => localStore.get(key) ?? null,
    setItem: (key: string, value: string) => {
      localStore.set(key, value);
    },
    removeItem: (key: string) => {
      localStore.delete(key);
    },
  };
});

function view(id: string, over: Partial<ScheduleView> = {}): ScheduleView {
  return {
    id,
    name: `nightly ${id}`,
    description: 'runs the nightly job',
    action_kind: 'shell',
    action: { kind: 'shell', command: 'echo hi' },
    workspace: '/tmp/work',
    trigger: { kind: 'daily', time: '09:00', timezone: 'Asia/Shanghai' },
    trigger_text: 'Every day at 09:00',
    timezone: 'Asia/Shanghai',
    enabled: true,
    running: false,
    run_state: null,
    next_run_at: '2026-11-06T01:00:00Z',
    last_run: null,
    needs_attention: false,
    attention_reason: null,
    session_policy: 'continue',
    overlap: 'skip-if-running',
    mode: 'assisted',
    model: null,
    timeout_secs: 600,
    misfire: 'skip',
    misfire_grace_secs: 0,
    session_id: null,
    grant: {
      granted: true,
      external_side_effects: false,
      granted_at: null,
      requires_regrant: false,
    },
    created_at: '2026-11-01T00:00:00Z',
    revision: 'rev-1',
    ...over,
  };
}

function run(id: string): ScheduleRunView {
  return {
    id,
    scheduled_for: '2026-11-05T01:00:00Z',
    started_at: '2026-11-05T01:00:01Z',
    state: 'succeeded',
  };
}

function status(): ScheduleStatusView {
  return {
    definitions: 1,
    enabled: 1,
    paused: 0,
    running: 0,
    overdue_pending: 0,
    next_occurrence: '2026-11-06T01:00:00Z',
    wakeup: {
      backend: 'launchd',
      label: 'com.leveler.schedule',
      supported: true,
      registered: false,
      loaded: false,
      interval_secs: 60,
    },
  };
}

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
  (bridge as unknown as {
    ws: { send: (f: UpFrame) => boolean; setSession: (id: string | null) => void };
  }).ws = {
    send: (frame: UpFrame) => {
      if (frame.type === 'deliver' || frame.type === 'deliver_versioned_setting') {
        sent.push(frame.command);
      }
      return true;
    },
    setSession: () => {},
  };
  const snapshot = {
    id: 's1',
    repository: '/repo',
    goal: 'g',
    model: null,
    mode: 'assisted' as const,
    branch: null,
    status: 'idle',
    messages: [],
    last_sequence: 1,
  };
  reducer(state, { type: 'snapshot', session: snapshot });
  const apply = (ev: RuntimeEvent) =>
    (bridge as unknown as { applyEvent: (ev: RuntimeEvent) => void }).applyEvent(ev);
  apply({ type: 'session_updated', session: snapshot });
  return { bridge, state, sent, apply };
}

function manageCommands(sent: ClientCommand[]) {
  return sent
    .filter((c): c is Extract<ClientCommand, { type: 'manage_schedule' }> => c.type === 'manage_schedule')
    .map((c) => c);
}

/** Answer the given query as the runtime would. */
function answer(
  h: Harness,
  command: Extract<ClientCommand, { type: 'manage_schedule' }>,
  response: ScheduleResponse,
) {
  h.apply({
    type: 'schedule_managed',
    requester_session_id: 's1',
    query_id: command.query_id,
    response,
  });
}

describe('schedules controller', () => {
  it('opening reads list and status from the requester session', () => {
    const h = harness();
    h.bridge.openSchedules();

    const commands = manageCommands(h.sent);
    expect(commands).toHaveLength(2);
    expect(commands.map((c) => c.request.action).sort()).toEqual(['list', 'status']);
    for (const command of commands) {
      expect(command.requester_session_id).toBe('s1');
      expect(command.query_id).toBeTruthy();
    }
    expect(h.state.schedules.loading).toBe(true);
  });

  it('a foreign query id is never accepted as this view’s state', () => {
    const h = harness();
    h.bridge.openSchedules();
    h.apply({
      type: 'schedule_managed',
      requester_session_id: 's1',
      query_id: 'somebody-elses-query',
      response: { kind: 'list', schedules: [view('foreign')] },
    });
    expect(h.state.schedules.items).toEqual([]);
    expect(h.state.schedules.loading).toBe(true);
  });

  it('its own answers replace the rows and drop the loading state', () => {
    const h = harness();
    h.bridge.openSchedules();
    const [list, statusCommand] = manageCommands(h.sent);
    answer(h, list!, { kind: 'list', schedules: [view('a')] });
    expect(h.state.schedules.items.map((s) => s.id)).toEqual(['a']);
    expect(h.state.schedules.loading).toBe(false);
    answer(h, statusCommand!, { kind: 'status', status: status() });
    expect(h.state.schedules.status?.wakeup.registered).toBe(false);
  });

  it('selecting a row asks for that definition and its runs', () => {
    const h = harness();
    h.bridge.openSchedules();
    h.bridge.selectSchedule('a');
    const commands = manageCommands(h.sent).slice(2);
    expect(commands.map((c) => c.request.action).sort()).toEqual(['runs', 'show']);
    const show = commands.find((c) => c.request.action === 'show')!;
    expect(show.request).toEqual({ action: 'show', id: 'a' });
    answer(h, show, { kind: 'show', schedule: view('a') });
    expect(h.state.schedules.detail?.id).toBe('a');

    const runs = commands.find((c) => c.request.action === 'runs')!;
    answer(h, runs, { kind: 'runs', runs: [run('r1')] });
    expect(h.state.schedules.runs.map((r) => r.id)).toEqual(['r1']);
    expect(h.state.schedules.runsLoaded).toBe(true);
  });

  it('an answer for a definition the person has already left is ignored', () => {
    const h = harness();
    h.bridge.openSchedules();
    h.bridge.selectSchedule('a');
    const show = manageCommands(h.sent).find((c) => c.request.action === 'show')!;
    h.bridge.selectSchedule('b');
    answer(h, show, { kind: 'show', schedule: view('a') });
    expect(h.state.schedules.selected).toBe('b');
    expect(h.state.schedules.detail).toBeNull();
  });

  it('a mutation is single-flight and its refusal stays a visible error', () => {
    const h = harness();
    h.bridge.openSchedules();
    h.bridge.manageSchedule({ action: 'pause', id: 'a' }, 'pause');
    const first = manageCommands(h.sent).at(-1)!;
    expect(first.request).toEqual({ action: 'pause', id: 'a' });
    expect(h.state.schedules.busy).toBe(true);

    const before = manageCommands(h.sent).length;
    h.bridge.manageSchedule({ action: 'resume', id: 'a' }, 'resume');
    expect(manageCommands(h.sent)).toHaveLength(before);

    answer(h, first, {
      kind: 'error',
      message: 'schedule changed since it was read; re-read before editing',
    });
    expect(h.state.schedules.busy).toBe(false);
    expect(h.state.schedules.error).toContain('changed since it was read');
    expect(h.state.schedules.lastChange).toBeNull();
  });

  it('an update carries the revision that was read, and a change re-reads the runtime', () => {
    const h = harness();
    h.bridge.openSchedules();
    const [list, statusCommand] = manageCommands(h.sent);
    answer(h, list!, { kind: 'list', schedules: [view('a')] });
    answer(h, statusCommand!, { kind: 'status', status: status() });
    h.bridge.selectSchedule('a');
    const show = manageCommands(h.sent).find((c) => c.request.action === 'show')!;
    answer(h, show, { kind: 'show', schedule: view('a', { revision: 'rev-read' }) });

    h.bridge.manageSchedule(
      {
        action: 'update',
        id: 'a',
        expected_revision: 'rev-read',
        patch: { name: 'renamed' },
      },
      'update',
    );
    const update = manageCommands(h.sent).at(-1)!;
    expect(update.request).toMatchObject({ action: 'update', expected_revision: 'rev-read' });
    const beforeRefresh = manageCommands(h.sent).length;

    answer(h, update, {
      kind: 'changed',
      message: 'schedule updated',
      schedule: view('a', { name: 'renamed', revision: 'rev-2' }),
    });
    expect(h.state.schedules.detail?.name).toBe('renamed');

    // The runtime is the authority after a change: list, status, and the open
    // detail are read again instead of being assumed from the answer.
    const refreshed = manageCommands(h.sent).slice(beforeRefresh);
    expect(refreshed.map((c) => c.request.action).sort()).toEqual([
      'list',
      'runs',
      'show',
      'status',
    ]);
  });

  it('a create sends the structured draft without parsing any display text', () => {
    const h = harness();
    h.bridge.openSchedules();
    const draft = {
      name: 'morning report',
      workspace: '/repo',
      action: { kind: 'agent' as const, goal: 'summarize yesterday’s commits' },
      trigger: { kind: 'daily' as const, time: '09:00', timezone: 'Asia/Shanghai' },
      mode: 'assisted',
      grant_unattended: false,
      allow_external_side_effects: false,
    };
    h.bridge.manageSchedule({ action: 'create', draft }, 'create');
    const create = manageCommands(h.sent).at(-1)!;
    expect(create.request).toEqual({ action: 'create', draft });
  });

  it('changing the filter reads the same authority with the new filter', () => {
    const h = harness();
    h.bridge.openSchedules();
    const [list, statusCommand] = manageCommands(h.sent);
    answer(h, list!, { kind: 'list', schedules: [view('a')] });
    answer(h, statusCommand!, { kind: 'status', status: status() });

    h.bridge.setScheduleFilter('paused');
    expect(h.state.schedules.filter).toBe('paused');
    const refresh = manageCommands(h.sent).slice(2);
    expect(refresh.map((c) => c.request.action).sort()).toEqual(['list', 'status']);
    const listRequest = refresh.find((c) => c.request.action === 'list')!;
    expect(listRequest.request).toEqual({ action: 'list', filter: 'paused' });
  });

  it('closing forgets the projection so reopening re-reads it', () => {
    const h = harness();
    h.bridge.openSchedules();
    const [list] = manageCommands(h.sent);
    answer(h, list!, { kind: 'list', schedules: [view('a')] });
    h.bridge.closeSchedules();
    expect(h.state.schedules.items).toEqual([]);
    expect(h.state.schedules.loaded).toBe(false);
  });
});
