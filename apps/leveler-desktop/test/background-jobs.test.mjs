// Background job rows against the runtime's durable reconciliation.
//
// A snapshot refresh used to keep every prior row that was absent from the
// live blocker list, so a task whose owning runtime died stayed "运行中"
// forever. The runtime's `background_tasks` records carry the real state; a
// vanished row must settle there, never stay live without proof.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { reconcileBackgroundJobs, settledJobState } from '../src/state.mjs';

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

test('a cold start restores historical lost and unknown records', () => {
  const { backgroundTasks, settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [],
    prior: [],
    records: [
      {
        task_id: 't-lost',
        program: 'sh',
        args: ['-c', 'sleep 600'],
        state: 'lost',
        duration_ms: 42_000,
        output: 'host recorded tail\n',
      },
      { task_id: 't-unknown', program: 'curl', args: ['example.test'], state: 'unknown' },
      { task_id: 't-done', program: 'echo', args: ['ok'], state: 'completed', exit_code: 0 },
    ],
    settled: [],
  });
  assert.deepEqual(backgroundTasks, []);
  assert.deepEqual(
    settledBackgroundTasks.map(job => job.task_id),
    ['t-lost', 't-unknown'],
    'only outcomes that cannot be proven are restored; completed history stays out',
  );
  const lost = settledBackgroundTasks[0];
  assert.equal(lost.status, 'lost');
  assert.equal(lost.duration_ms, 42_000);
  assert.equal(lost.output, 'host recorded tail\n');
  assert.equal(lost.finished_at, Number(lost.finished_at));
  assert.equal(settledBackgroundTasks[1].status, 'unknown');
});

test('a live row whose durable record already settled is not kept running', () => {
  const { backgroundTasks, settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [priorJob('t-lost')],
    prior: [],
    records: [{
      task_id: 't-lost',
      program: 'sh',
      args: ['-c', 'sleep 600'],
      state: 'lost',
      duration_ms: 7_000,
      output: 'held output\n',
    }],
    settled: [],
  });
  assert.deepEqual(backgroundTasks, [], 'a lost owner has no live work to show');
  assert.equal(settledBackgroundTasks[0].status, 'lost');
  assert.equal(settledBackgroundTasks[0].duration_ms, 7_000);
});

test('a cold-start record already settled is not duplicated', () => {
  const settled = [{ task_id: 't-lost', status: 'unknown', finished_at: 1 }];
  const { settledBackgroundTasks } = reconcileBackgroundJobs({
    active: [],
    prior: [],
    records: [{ task_id: 't-lost', state: 'lost' }],
    settled,
  });
  assert.equal(settledBackgroundTasks.length, 1);
  assert.equal(settledBackgroundTasks[0].status, 'unknown');
});

test('a lost or unknown outcome is never labelled as a failure', () => {
  assert.deepEqual(settledJobState({ status: 'failed' }), { label: '已失败', dot: 'failed' });
  for (const status of ['lost', 'unknown']) {
    const state = settledJobState({ status });
    assert.notEqual(state.label, '已失败', status);
    assert.equal(state.dot, 'warning', `${status} is uncertain, not failed`);
  }
  assert.deepEqual(settledJobState({ status: 'lost' }), { label: '状态未知', dot: 'warning' });
  assert.deepEqual(settledJobState({ status: 'unknown' }), { label: '结果未知', dot: 'warning' });
  assert.deepEqual(settledJobState({ status: 'failed', exit_code: 7 }), { label: '已失败', dot: 'failed' });
});
