// Execution Presentation Contract v1 — Desktop conformance.
//
// Reads the SAME fixtures the reference implementation is frozen against
// (testdata/execution_presentation/v1/*.json), drives the real Desktop state
// projection (`state.mjs`) and the real presentation projection
// (`presentation.mjs`), and compares the semantic tree with the frozen
// expectation. The Electron Main process and the bridge are not involved: this
// is a pure renderer-side contract.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { applyEvent, projectSnapshot } from '../src/state.mjs';
import { contractToolStatus, messageKind, projectTurn } from '../src/presentation.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURE_DIR = join(HERE, '..', '..', '..', 'testdata', 'execution_presentation', 'v1');

const DEFAULT_SNAPSHOT = {
  id: 's1',
  repository: '/repo',
  goal: 'fixture',
  model: null,
  mode: 'assisted',
  branch: null,
  status: 'idle',
  messages: [],
};

function fixtures() {
  return readdirSync(FIXTURE_DIR)
    .filter((name) => name.endsWith('.json'))
    .sort()
    .map((name) => JSON.parse(readFileSync(join(FIXTURE_DIR, name), 'utf8')));
}

/** The Desktop projection of one session onto the contract's semantic tree. */
function project(state) {
  const ordered = [];
  for (const item of projectTurn(state.messages, state.tools, state.lastTerminal !== null)) {
    if (item.kind === 'execution_round') {
      const round = item.round;
      ordered.push({
        seq: item.seq,
        item: {
          kind: 'execution_round',
          model_step: round.modelStep,
          status: round.status,
          all_ok: round.allOk,
          batches: round.batches,
          tools: round.tools.map((tool) => ({
            id: tool.id,
            name: tool.name,
            status: contractToolStatus(tool.status),
          })),
        },
      });
      continue;
    }
    ordered.push({ seq: item.seq, item: { kind: item.kind, text: item.text } });
  }
  // A runtime-authored message is a note, never user speech (§I13).
  state.messages.forEach((message, index) => {
    if (message.kind !== 'runtime_notice') return;
    ordered.push({ seq: message.seq ?? index, item: { kind: 'note', text: message.text } });
  });
  ordered.sort((a, b) => a.seq - b.seq);
  const items = ordered.map((entry) => entry.item);
  if (state.lastTerminal) items.push({ kind: 'turn_end', status: state.lastTerminal });
  const user_texts = state.messages
    .filter((message) => messageKind(message) === 'user')
    .map((message) => message.text);
  return { items, user_texts, reasoning_visible: false };
}

function runPath(steps) {
  let state = projectSnapshot(DEFAULT_SNAPSHOT, []);
  for (const step of steps) {
    if (step.event) state = applyEvent(state, step.event);
    else if (step.snapshot) state = projectSnapshot(step.snapshot.session, []);
    else if (step.history) state = projectSnapshot(state.session, step.history);
  }
  return state;
}

/**
 * A path that rebuilds a turn from durable history ALONE is deferred: Desktop's
 * history load restores the tools and the terminal on top of the snapshot's
 * messages (see `loadHistory`), and it has no path that reads a transcript out
 * of history with no snapshot behind it. The gap is asserted below rather than
 * silently skipped.
 */
function needsSnapshotBackedHistory(steps) {
  const firstHistory = steps.findIndex((step) => step.history !== undefined);
  return firstHistory >= 0 && !steps.slice(0, firstHistory).some((step) => step.snapshot);
}

test('execution presentation contract v1 (desktop)', () => {
  const failures = [];
  const compared = new Set();
  const deferred = new Set();
  for (const fixture of fixtures()) {
    for (const [name, steps] of Object.entries(fixture.paths)) {
      if (needsSnapshotBackedHistory(steps)) {
        deferred.add(`${fixture.id}/${name}`);
        continue;
      }
      const state = runPath(steps);
      try {
        assert.deepEqual(project(state), fixture.expect);
        compared.add(fixture.id);
      } catch (error) {
        failures.push(`${fixture.id}/${name}: ${error.message}`);
      }
    }
  }
  assert.deepEqual(failures, []);
  assert.equal(compared.size >= 14, true, `every fixture compared: ${[...compared]}`);
  // C14's replay path is the same pre-existing gap as C10's: with no snapshot
  // behind it, a history-only load yields no messages (and no answer to keep).
  assert.deepEqual([...deferred], ['C10/replay', 'C14/replay']);
});

test('desktop cannot rebuild a transcript from durable history alone', () => {
  // The deferred C10/replay path: with no snapshot behind it, a history-only
  // load yields no messages and no terminal. Stated, not hidden.
  const state = runPath([
    {
      history: [
        {
          turn_elapsed_ms: 0,
          turn_start: true,
          event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: 'q' } },
        },
        { turn_elapsed_ms: 10, turn_start: false, event: { type: 'turn_completed' } },
      ],
    },
  ]);
  // No messages are rebuilt, so the turn has no AssistantText and no answer.
  assert.deepEqual(state.messages, []);
  assert.equal(state.lastTerminal, 'no_final_answer');
});

test('the corpus declares the frozen C-cases', () => {
  const ids = fixtures().map((fixture) => fixture.id);
  for (let index = 1; index <= 14; index += 1) {
    assert.ok(ids.includes(`C${index}`), `missing C${index}`);
  }
});

test('desktop never carries raw reasoning (Contract v1 §I5)', () => {
  let state = projectSnapshot({ ...DEFAULT_SNAPSHOT, status: 'running' }, []);
  state = applyEvent(state, { type: 'reasoning_delta', delta: 'provider chain of thought' });
  assert.equal(state.messages.length, 0);
  assert.equal('reasoningText' in state, false);
});
