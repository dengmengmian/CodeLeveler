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

import { applyEvent } from '../src/state.mjs';
import { confirmedDiff, diffCounts, diffLines, failureReason } from '../src/presentation.mjs';

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
  const patch = confirmedDiff(tool);
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
  assert.equal(confirmedDiff(tool), null, 'a failed run is not a confirmed change');
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
