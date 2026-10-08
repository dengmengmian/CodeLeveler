// Conversation Presentation Contract — Desktop conformance.
//
// Reads the SAME frozen corpus the reference implementation is anchored to
// (`testdata/conversation_presentation/v1/*.json`), drives the real Desktop
// state reducer with the fixture's live events, and asserts the two items the
// Desktop can carry today:
//
//   C1 a confirmed edit's diff is the runtime's `applied_diff`, displayed in
//      full — never rebuilt from the tool's arguments, never truncated;
//   C4 a failed run's row states its failure without expanding it.
//
// A path that needs a capability this client does not have yet is DEFERRED
// here, not silently skipped: the Desktop has no completed-Thought history
// (C3) and no exploration receipt (C2) yet, so those two assertions live in
// `test/executionPresentation.test.mjs`'s sibling work instead of being
// claimed by a passing test that never checked them.

import { strict as assert } from 'node:assert';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import test from 'node:test';

import { applyEvent, projectSnapshot } from '../src/state.mjs';
import {
  confirmedDiffOf,
  diffCounts,
  diffLines,
  failureReason,
  foldedThoughts,
  groupExploration,
  turnBlocks,
} from '../src/presentation.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const CORPUS = path.resolve(HERE, '../../../testdata/conversation_presentation/v1');

async function fixture(id) {
  return JSON.parse(await readFile(path.join(CORPUS, `${id}.json`), 'utf8'));
}

/** Drive a fixture's live path through the real Desktop reducer. */
function drive(fixtureDoc) {
  const message = { id: 's1', role: 'assistant', text: '', seq: 0 };
  let state = {
    session: { id: 's1', status: 'idle', collaboration: 'chat' },
    messages: [],
    nextSeq: 0,
    status: 'idle',
    tools: [],
    approvals: [],
    clarifications: [],
    activity: '',
    plan: null,
    diff: null,
    diffError: null,
    streamingMessageId: null,
    lastTerminal: null,
  };
  void message;
  for (const entry of fixtureDoc.paths.live) state = applyEvent(state, entry.event);
  return state;
}

test('C1: the confirmed diff comes from applied_diff, never from the arguments', async () => {
  const doc = await fixture('C1');
  const state = drive(doc);
  const tool = state.tools.find((candidate) => candidate.id === 't1');
  assert.ok(tool, 'the edit call is on the ledger');
  const patch = confirmedDiffOf(tool);
  assert.ok(patch, 'the runtime confirmed the edit');
  // The applied result — NOT the requested patch the arguments carry.
  assert.match(patch, /-const RECOMMENDED: &\[&str\] = \["gpt-5\.6"\];/);
  assert.match(patch, /\+const RECOMMENDED: &\[&str\] = \["gpt-6"\];/);
  assert.deepEqual(diffCounts(patch), [1, 1]);
  // Full display: every line of the patch is available to paint.
  assert.equal(diffLines(patch).length, patch.split('\n').length);
  assert.ok(diffLines(patch).some((line) => line.includes('@@')));
  // The corpus expectation is what the Desktop projection agrees with.
  const expected = doc.expect.items.find((item) => item.kind === 'edit_diff');
  assert.equal(expected.rendered_in_full, true);
  assert.equal(expected.needs_click, false);
});

test('C1: a call that did not confirm an edit shows no diff', async () => {
  const doc = await fixture('C4');
  const state = drive(doc);
  const tool = state.tools.find((candidate) => candidate.id === 't1');
  assert.equal(confirmedDiffOf(tool), null, 'a failed run is not a confirmed change');
});

test('C4: a failed run states its failure without expanding', async () => {
  const doc = await fixture('C4');
  const state = drive(doc);
  const tool = state.tools.find((candidate) => candidate.id === 't1');
  assert.equal(tool.status, 'failed');
  // The command's own failure line, with the runtime's execution rows removed.
  // Same line the reference picks: the first one that reports the failure.
  assert.equal(failureReason(tool.preview), 'test mapping::recommended ... FAILED');
  const expected = doc.expect.items.find((item) => item.kind === 'run_receipt');
  assert.equal(expected.failure_visible, true);
  assert.equal(expected.folded, true);
});


// The corpus' C2 and C3 were deferred by the Desktop until it had the
// capability. It has both now, so they are checked here: consecutive read-only
// exploration is one collapsed receipt whose members (and hidden Thoughts) are
// not painted until it opens, and a completed Thought is folded with the
// runtime's own duration.

test('C2: consecutive exploration is ONE collapsed, reversible receipt', () => {
  const tools = [
    { id: 't1', name: 'read_file', arguments: '{"path":"src/models.rs"}', status: 'ok', seq: 1 },
    { id: 't2', name: 'read_file', arguments: '{"path":"src/config.rs"}', status: 'ok', seq: 2 },
    { id: 't3', name: 'grep', arguments: '{"pattern":"recommended"}', status: 'ok', seq: 3 },
  ];
  const receipts = groupExploration(tools);
  assert.equal(receipts.length, 1, 'one receipt, not three rows');
  assert.equal(receipts[0].label, '读取 2 个文件 · 搜索 1 次');
  assert.equal(receipts[0].folded, true, 'collapsed by default');
  assert.deepEqual(
    receipts[0].members.map((member) => member.target),
    ['src/models.rs', 'src/config.rs', 'recommended'],
    'the fold keeps its real members, in arrival order',
  );
  const blocks = turnBlocks([], [
    { modelStep: 1, tools, status: 'ok', allOk: true, batches: [] },
  ]);
  assert.equal(blocks.length, 1);
  assert.equal(blocks[0].kind, 'receipt', 'a read-only round is the receipt');
});

test('C2: a completed Thought inside the fold is hidden by it, and kept', () => {
  const thought = { kind: 'thought', id: 'th1', elapsedMs: 900, folded: true, body: '先看映射。', seq: 2 };
  const blocks = turnBlocks([thought], [
    {
      modelStep: 1,
      tools: [
        { id: 't1', name: 'read_file', arguments: '{"path":"a"}', status: 'ok', seq: 1 },
        { id: 't2', name: 'grep', arguments: '{"pattern":"p"}', status: 'ok', seq: 3 },
      ],
      status: 'ok',
      allOk: true,
      batches: [],
    },
  ]);
  assert.equal(blocks.length, 1, 'the Thought belongs to the fold');
  assert.equal(blocks[0].kind, 'receipt');
  assert.deepEqual(blocks[0].thoughts.map((entry) => entry.body), ['先看映射。']);
});

test('C3: a completed Thought is folded, with the runtime duration', () => {
  const state = projectSnapshot(
    { id: 's1', messages: [] },
    [
      { turn_start: true, event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: 'q' } } },
      { turn_start: false, event: { type: 'reasoning_started' } },
      { turn_start: false, event: { type: 'reasoning_delta', delta: '先看入口。' } },
      { turn_start: false, event: { type: 'reasoning_delta', delta: '再看调用方。' } },
      { turn_start: false, event: { type: 'reasoning_completed', elapsed_ms: 1600 } },
      { turn_start: false, event: { type: 'assistant_message_started', message_id: 'm1' } },
      { turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm1', delta: '没问题。' } },
      { turn_start: false, event: { type: 'assistant_message_completed', message_id: 'm1' } },
      { turn_start: false, event: { type: 'turn_answered' } },
    ],
  );
  assert.equal(state.reasoning, '', 'a finished segment is not left live');
  const folded = foldedThoughts(state.thoughts);
  assert.equal(folded.length, 1);
  assert.equal(folded[0].elapsedMs, 1600, "the runtime's own measurement");
  assert.equal(folded[0].folded, true, 'collapsed by default');
  assert.equal(folded[0].body, '先看入口。再看调用方。', 'the body is the durable segment');
  // Reasoning never becomes assistant prose.
  assert.equal(state.messages.some((message) => message.text.includes('先看入口。')), false);
});
