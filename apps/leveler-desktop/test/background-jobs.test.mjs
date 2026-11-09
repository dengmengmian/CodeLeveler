// Background job rows against the runtime's durable reconciliation.
//
// A snapshot refresh used to keep every prior row that was absent from the
// live blocker list, so a task whose owning runtime died stayed "运行中"
// forever. The runtime's `background_tasks` records carry the real state; a
// vanished row must settle there, never stay live without proof.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { reconcileBackgroundJobs } from '../src/state.mjs';

const priorJob = (taskId, extra = {}) => ({
  task_id: taskId,
  program: 'sh',
  args: ['-c', 'sleep 600'],
  started_at: 1_000_000,
  elapsed_ms: 0,
  output: 'streamed so far\n',
  ...extra,
});

test('a lost owner settles the row with its frozen duration and retained output', () => {
  const { backgroundTasks, settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [],
    prior: [priorJob('t-lost')],
    records: [{
      task_id: 't-lost',
      program: 'sh',
      args: ['-c', 'sleep 600'],
      state: 'lost',
      duration_ms: 42_000,
      output: 'host recorded tail\n',
    }],
    settled: [],
  });
  assert.deepEqual(backgroundTasks, []);
  assert.equal(settledBackgroundTasks.length, 1);
  const [row] = settledBackgroundTasks;
  assert.equal(row.status, 'lost');
  assert.equal(row.duration_ms, 42_000);
  // Duration is the host's proven lifetime, not the moment this client asked.
  assert.equal(row.finished_at, 1_000_000 + 42_000);
  assert.equal(row.output, 'host recorded tail\n');
  assert.equal(row.stopped, false);
});

test('a row that vanished with no durable record is unknown, not running', () => {
  const { backgroundTasks, settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [],
    prior: [priorJob('t-gone')],
    records: [],
    settled: [],
  });
  assert.deepEqual(backgroundTasks, []);
  assert.equal(settledBackgroundTasks[0].status, 'unknown');
});

test('a record that still says running keeps the row live', () => {
  const { backgroundTasks, settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [],
    prior: [priorJob('t-live')],
    records: [{ task_id: 't-live', state: 'running', program: 'sh', args: [] }],
    settled: [],
  });
  assert.equal(backgroundTasks.length, 1);
  assert.equal(backgroundTasks[0].task_id, 't-live');
  assert.deepEqual(settledBackgroundTasks, []);
});

test('terminal records carry their own status', () => {
  const cases = [
    ['completed', { status: 'completed', stopped: false, exit_code: 0 }],
    ['failed', { status: 'failed', stopped: false, exit_code: 3 }],
    ['cancelled', { status: 'completed', stopped: true }],
    ['unknown', { status: 'unknown', stopped: false }],
    ['conflict', { status: 'conflict', stopped: false }],
  ];
  for (const [state, expected] of cases) {
    const { settledBackgroundTasks } = reconcileBackgroundJobs({
      active: [],
      prior: [priorJob(`t-${state}`)],
      records: [{ task_id: `t-${state}`, state, exit_code: state === 'failed' ? 3 : state === 'completed' ? 0 : undefined }],
      settled: [],
    });
    assert.equal(settledBackgroundTasks[0].status, expected.status, state);
    assert.equal(settledBackgroundTasks[0].stopped, expected.stopped, state);
    if ('exit_code' in expected) assert.equal(settledBackgroundTasks[0].exit_code, expected.exit_code, state);
  }
});

test('a row already settled by an event is not duplicated', () => {
  const settled = [{ task_id: 't-lost', status: 'unknown', finished_at: 1 }];
  const { settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [],
    prior: [priorJob('t-lost')],
    records: [{ task_id: 't-lost', state: 'lost' }],
    settled,
  });
  assert.equal(settledBackgroundTasks.length, 1);
  assert.equal(settledBackgroundTasks[0].status, 'unknown');
});
