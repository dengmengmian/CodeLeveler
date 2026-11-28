import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createMemoryActionTracker, MEMORY_ACTION_SETTLE_MS} from '../src/memory-action.mjs';

// Deterministic time simulation: the tracker takes an injected clock, so this
// pins the 35-minute boundary without waiting 35 real minutes. It does not
// pretend to measure the wall clock.
const clock = start => { let value = start; return { now: () => value, at: next => { value = next; } }; };

test('a pending action is unknown only at the 35-minute bound', () => {
  const time = clock(1_000_000);
  const tracker = createMemoryActionTracker({ now: time.now, settleMs: MEMORY_ACTION_SETTLE_MS });
  assert.deepEqual(tracker.begin('q1', '记忆整理'), { queryId: 'q1', label: '记忆整理', startedAt: 1_000_000 });
  time.at(1_000_000 + MEMORY_ACTION_SETTLE_MS - 1);
  assert.equal(tracker.expired(), false, 'one millisecond early is still waiting');
  time.at(1_000_000 + MEMORY_ACTION_SETTLE_MS);
  assert.equal(tracker.expired(), true, 'the bound itself is unknown');
});

test('crossing the bound neither cancels the work nor claims success', () => {
  const time = clock(0);
  const tracker = createMemoryActionTracker({ now: time.now, settleMs: 100 });
  tracker.begin('q1', '记忆整理');
  time.at(100);
  assert.equal(tracker.expired(), true);
  // The pending action still exists; only the client's report is UNKNOWN.
  assert.equal(tracker.current()?.queryId, 'q1');
  // The real answer arriving late is still the real answer, marked late.
  const settled = tracker.settle('q1', 500);
  assert.equal(settled.expired, true);
  assert.equal(settled.queryId, 'q1');
  assert.equal(tracker.current(), null);
});

test('a duplicate answer is not a second outcome', () => {
  const tracker = createMemoryActionTracker({ now: () => 0, settleMs: 100 });
  tracker.begin('q1', '记忆整理');
  assert.equal(tracker.settle('q1').duplicate, undefined);
  assert.deepEqual(tracker.settle('q1'), { duplicate: true });
});

test('draining the pending state leaves a late answer unmatched, not duplicated', () => {
  const tracker = createMemoryActionTracker({ now: () => 0, settleMs: 100 });
  tracker.begin('q1', '记忆整理');
  assert.equal(tracker.drop(), '记忆整理');
  assert.equal(tracker.current(), null);
  // The page switched sessions or lost the connection; a result can still
  // arrive, and it is displayed once as an unmatched real fact.
  assert.equal(tracker.settle('q1'), null);
  assert.equal(tracker.drop(), null);
});

test('only one action may be pending at a time', () => {
  const tracker = createMemoryActionTracker({ now: () => 0, settleMs: 100 });
  assert.ok(tracker.begin('q1', 'A'));
  assert.equal(tracker.begin('q2', 'B'), null);
  tracker.settle('q2');
  assert.equal(tracker.current()?.queryId, 'q1', 'an unmatched id never clears the waiter');
});

test('the settled-answer window stays bounded', () => {
  const tracker = createMemoryActionTracker({ now: () => 0, settleMs: 100 });
  for (let index = 0; index < 40; index += 1) {
    tracker.begin(`q${index}`, 'A');
    tracker.settle(`q${index}`);
  }
  // The oldest ids fell out of the bounded window; the newest is still known.
  assert.equal(tracker.settle('q39').duplicate, true);
  assert.equal(tracker.settle('q0'), null);
});
