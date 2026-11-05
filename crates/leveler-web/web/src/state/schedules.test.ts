// 定时任务页的状态契约：运行时投影是唯一事实源，UI 只做归属、合并与呈现。
// 这些测试锁定的是产品语义——创建成功会长出真实详情、拒绝不是成功、
// 未知不是失败、筛选不会把旧详情留在屏幕上。

import { describe, expect, it } from 'vitest';
import type {
  ScheduleResponse,
  ScheduleRunView,
  ScheduleStatusView,
  ScheduleView,
} from '../types/protocol';
import { initialState, reducer, type AppState } from './store';

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

function run(id: string, over: Partial<ScheduleRunView> = {}): ScheduleRunView {
  return {
    id,
    scheduled_for: '2026-11-05T01:00:00Z',
    started_at: '2026-11-05T01:00:01Z',
    state: 'succeeded',
    ...over,
  };
}

function status(registered: boolean): ScheduleStatusView {
  return {
    definitions: 2,
    enabled: 1,
    paused: 1,
    running: 0,
    overdue_pending: 0,
    next_occurrence: '2026-11-06T01:00:00Z',
    wakeup: {
      backend: 'launchd',
      label: 'com.leveler.schedule',
      supported: true,
      registered,
      loaded: registered,
      interval_secs: 60,
    },
  };
}

function apply(state: AppState, response: ScheduleResponse, intent: string | null = null): void {
  reducer(state, {
    type: 'schedule_applied',
    intent: intent as never,
    response,
  });
}

describe('schedules state', () => {
  it('opening marks the read in flight and a list answer replaces the rows', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'schedules_opening' });
    expect(state.schedules.loading).toBe(true);
    expect(state.schedules.loaded).toBe(false);

    apply(state, { kind: 'list', schedules: [view('a'), view('b')] });
    expect(state.schedules.loading).toBe(false);
    expect(state.schedules.loaded).toBe(true);
    expect(state.schedules.items.map((s) => s.id)).toEqual(['a', 'b']);
  });

  it('a created schedule opens its own detail instead of staying on a stale list', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a')] });
    const created = view('created', { name: 'created job', revision: 'rev-new' });
    apply(state, { kind: 'changed', message: 'schedule created', schedule: created }, 'create');

    expect(state.schedules.selected).toBe('created');
    expect(state.schedules.detail?.name).toBe('created job');
    expect(state.schedules.items.map((s) => s.id)).toEqual(['a', 'created']);
    expect(state.schedules.lastChange).toMatchObject({ intent: 'create', message: 'schedule created' });
    expect(state.schedules.busy).toBe(false);
  });

  it('an update refreshes the open detail from the runtime answer', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a')] });
    reducer(state, { type: 'schedule_select', id: 'a' });
    apply(state, { kind: 'show', schedule: view('a') });

    const updated = view('a', { name: 'renamed', revision: 'rev-2' });
    apply(state, { kind: 'changed', message: 'schedule updated', schedule: updated }, 'update');
    expect(state.schedules.detail?.name).toBe('renamed');
    expect(state.schedules.items[0]?.name).toBe('renamed');
  });

  it('a refusal is an error, never a silent success', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a')] });
    apply(
      state,
      { kind: 'error', message: 'schedule changed since it was read; re-read before editing' },
      'update',
    );
    expect(state.schedules.error).toBe(
      'schedule changed since it was read; re-read before editing',
    );
    expect(state.schedules.lastChange).toBeNull();
    expect(state.schedules.items[0]?.name).toBe('nightly a');
  });

  it('a removed definition leaves neither a selected row nor an open detail', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a'), view('b')] });
    reducer(state, { type: 'schedule_select', id: 'a' });
    apply(state, { kind: 'show', schedule: view('a') });

    apply(state, { kind: 'removed', id: 'a' }, 'remove');
    expect(state.schedules.items.map((s) => s.id)).toEqual(['b']);
    expect(state.schedules.selected).toBeNull();
    expect(state.schedules.detail).toBeNull();
    expect(state.schedules.runs).toEqual([]);
  });

  it('selecting another row never keeps the previous detail while it loads', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a'), view('b')] });
    reducer(state, { type: 'schedule_select', id: 'a' });
    apply(state, { kind: 'show', schedule: view('a') });
    apply(state, { kind: 'runs', runs: [run('r1')] });

    reducer(state, { type: 'schedule_select', id: 'b' });
    expect(state.schedules.selected).toBe('b');
    expect(state.schedules.detail).toBeNull();
    expect(state.schedules.runs).toEqual([]);
    expect(state.schedules.runsLoaded).toBe(false);
  });

  it('changing the filter drops the detail that belonged to the old query', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a')] });
    reducer(state, { type: 'schedule_select', id: 'a' });
    apply(state, { kind: 'show', schedule: view('a') });

    reducer(state, { type: 'schedules_filter', filter: 'paused' });
    expect(state.schedules.filter).toBe('paused');
    expect(state.schedules.selected).toBeNull();
    expect(state.schedules.detail).toBeNull();
    expect(state.schedules.runs).toEqual([]);
  });

  it('status and doctor answers are stored as the runtime reported them', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'status', status: status(false) });
    expect(state.schedules.status?.wakeup.registered).toBe(false);
    apply(state, {
      kind: 'doctor',
      healthy: false,
      checks: [{ name: 'launchd', severity: 'error', detail: 'not registered' }],
    });
    expect(state.schedules.doctor?.healthy).toBe(false);
    expect(state.schedules.doctor?.checks[0]?.detail).toBe('not registered');
  });

  it('closing forgets the view so reopening reads the runtime again', () => {
    const state = structuredClone(initialState);
    apply(state, { kind: 'list', schedules: [view('a')] });
    reducer(state, { type: 'schedule_select', id: 'a' });
    apply(state, { kind: 'show', schedule: view('a') });
    reducer(state, { type: 'schedules_closed' });

    expect(state.schedules.loaded).toBe(false);
    expect(state.schedules.items).toEqual([]);
    expect(state.schedules.selected).toBeNull();
    expect(state.schedules.detail).toBeNull();
  });

  it('a busy mutation is reflected so the surface can disable conflicting actions', () => {
    const state = structuredClone(initialState);
    reducer(state, { type: 'schedules_busy', busy: true });
    expect(state.schedules.busy).toBe(true);
    apply(state, { kind: 'changed', message: 'ok', schedule: null }, 'pause');
    expect(state.schedules.busy).toBe(false);
  });
});
