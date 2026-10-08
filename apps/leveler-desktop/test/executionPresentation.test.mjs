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

test('execution presentation contract v1 (desktop)', () => {
  const failures = [];
  const compared = new Set();
  for (const fixture of fixtures()) {
    for (const [name, steps] of Object.entries(fixture.paths)) {
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
});

test('a session reopened from durable history alone rebuilds its conversation', () => {
  // The C10/replay shape: no snapshot carries the transcript, only the runtime's
  // own projection of its durable log. Before this, a history-only load yielded
  // no messages at all — a reopened session painted an empty conversation.
  const state = runPath([
    {
      history: [
        {
          turn_elapsed_ms: 0,
          turn_start: true,
          event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: 'q' } },
        },
        {
          turn_elapsed_ms: 100,
          turn_start: false,
          event: { type: 'assistant_message_started', message_id: 'm1' },
        },
        {
          turn_elapsed_ms: 200,
          turn_start: false,
          event: { type: 'assistant_text_delta', message_id: 'm1', delta: 'a' },
        },
        {
          turn_elapsed_ms: 300,
          turn_start: false,
          event: { type: 'assistant_message_completed', message_id: 'm1' },
        },
        { turn_elapsed_ms: 400, turn_start: false, event: { type: 'turn_answered' } },
      ],
    },
  ]);
  assert.deepEqual(
    state.messages.map((message) => [message.role, message.text]),
    [
      ['user', 'q'],
      ['assistant', 'a'],
    ],
  );
  assert.equal(state.lastTerminal, 'answered');
});

test('a compacted session replays the conversation, not the summary row', () => {
  // `/compact` leaves the model context as one summary row. A reopen must paint
  // the durable conversation, and the internal summary is never a user line.
  const state = projectSnapshot(
    {
      id: 's1',
      messages: [{ id: 'summary', role: 'user', text: '对话摘要（已压缩历史）：前面聊过解析器。' }],
    },
    [
      {
        turn_elapsed_ms: 0,
        turn_start: true,
        event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: '看下解析器' } },
      },
      { turn_elapsed_ms: 10, turn_start: false, event: { type: 'reasoning_started' } },
      { turn_elapsed_ms: 20, turn_start: false, event: { type: 'reasoning_delta', delta: '先看入口。' } },
      { turn_elapsed_ms: 1600, turn_start: false, event: { type: 'reasoning_completed', elapsed_ms: 1600 } },
      { turn_elapsed_ms: 1700, turn_start: false, event: { type: 'assistant_message_started', message_id: 'm1' } },
      { turn_elapsed_ms: 1800, turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm1', delta: '解析器没问题。' } },
      { turn_elapsed_ms: 1900, turn_start: false, event: { type: 'assistant_message_completed', message_id: 'm1' } },
      { turn_elapsed_ms: 2000, turn_start: false, event: { type: 'turn_answered' } },
    ],
  );
  assert.deepEqual(
    state.messages.map((message) => message.text),
    ['看下解析器', '解析器没问题。'],
  );
  assert.equal(
    state.messages.some((message) => message.text.includes('对话摘要（已压缩历史）')),
    false,
    'the internal summary is a context artifact, not a conversation row',
  );
  assert.deepEqual(
    state.thoughts.map((thought) => [thought.text, thought.elapsedMs]),
    [['先看入口。', 1600]],
    'the completed Thought comes back with the runtime duration',
  );
  assert.equal(state.lastTerminal, 'answered');
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
