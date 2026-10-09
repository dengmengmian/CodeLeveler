import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { applyEvent, projectSnapshot } from '../src/state.mjs';
import { groupExecutionRounds, foldedThoughts, turnBlocks, confirmedDiffs, explorationEntries } from '../src/presentation.mjs';

const capture = JSON.parse(await readFile(new URL('../../../testdata/conversation_presentation/wire-v1/full-execution.json', import.meta.url), 'utf8'));
function actualState() {
  let state = projectSnapshot(capture.frames.find(frame => frame.type === 'snapshot').session);
  for (const frame of capture.frames) if (frame.type === 'event') state = applyEvent(state, frame.event);
  return state;
}

test('actual wire exploration merges adjacent model rounds without losing tool IDs or completed thoughts', () => {
  const state = actualState();
  const blocks = turnBlocks(foldedThoughts(state.thoughts), groupExecutionRounds(state.tools));
  const receipts = blocks.filter(block => block.kind === 'receipt');
  assert.equal(receipts.length, 1);
  assert.deepEqual(receipts[0].receipt.members.map(member => member.id), ['c1', 'c2', 'c3', 'c4']);
  assert.equal(receipts[0].receipt.reads, 2);
  assert.equal(receipts[0].receipt.searches, 2);
  const thoughts = blocks.flatMap(block => block.kind === 'thought' ? [block.thought] : block.kind === 'receipt' ? block.thoughts : []);
  assert.equal(thoughts.length, 4);
  assert.deepEqual(thoughts.map(thought => thought.elapsedMs), [0, 0, 0, 0]);
  assert.equal(new Set(thoughts.map(thought => thought.id)).size, 4);
  assert.equal(confirmedDiffs(state.tools)[0].toolId, 'c5');
});

test('exploration stops at task ownership, errors, unknown outcomes and execution barriers', () => {
  const state = actualState();
  const first = state.tools.filter(tool => ['c1', 'c2', 'c3', 'c4'].includes(tool.id));
  const receipts = tools => turnBlocks([], groupExecutionRounds(tools)).filter(block => block.kind === 'receipt');
  for (const status of ['failed', 'unknown', 'cancelled', 'running']) {
    const tools = first.map(tool => ({ ...tool, status: tool.id === 'c3' ? status : 'ok' }));
    assert.equal(receipts(tools).length, 2, status);
  }
  assert.equal(receipts(first.map(tool => ({ ...tool, anchor: tool.modelStep === 1 ? 'task-a' : 'task-b' }))).length, 2);
  assert.equal(receipts(first.map(tool => ({ ...tool, task_id: tool.modelStep === 1 ? 'task-a' : 'task-b' }))).length, 2);
  const tools = [first[0], first[1], { ...state.tools.find(tool => tool.id === 'c5'), seq: first[1].seq + .5 }, first[2], first[3]];
  assert.equal(receipts(tools).length, 2);
});

test('completed thought IDs are deduplicated while receipt chronology remains reversible', () => {
  const state = actualState();
  const thoughts = foldedThoughts(state.thoughts);
  const blocks = turnBlocks([...thoughts, ...thoughts], groupExecutionRounds(state.tools));
  const retained = blocks.flatMap(block => block.kind === 'thought' ? [block.thought] : block.kind === 'receipt' ? block.thoughts : []);
  assert.equal(retained.length, 4);
  assert.deepEqual(retained.map(thought => thought.id), thoughts.map(thought => thought.id));
});

test('actual thought message anchors survive the shared projection', () => {
  const state = actualState();
  const projected = foldedThoughts(state.thoughts);
  assert.ok(state.thoughts.every(thought => typeof thought.anchor === 'string'));
  assert.deepEqual(projected.map(thought => thought.anchor), state.thoughts.map(thought => thought.anchor));
});

test('opening the actual cross-round receipt restores the original member and Thought chronology', () => {
  const state = actualState();
  const receipt = turnBlocks(foldedThoughts(state.thoughts), groupExecutionRounds(state.tools)).find(block => block.kind === 'receipt');
  assert.ok(receipt);
  const restored = explorationEntries(receipt.receipt, receipt.thoughts);
  assert.deepEqual(restored.map(entry => entry.kind === 'thought' ? entry.thought.body : entry.member.id), ['c1', 'c2', '再核对推荐映射。', 'c3', 'c4']);
  assert.equal(receipt.receipt.anchor, state.tools[0].anchor);
  assert.deepEqual(restored.map(entry => entry.seq), restored.map(entry => entry.seq).toSorted((a, b) => a - b));
});

test('a Thought from a different task anchor never disappears inside an exploration receipt', () => {
  const state = actualState();
  const thoughts = foldedThoughts(state.thoughts).map(thought => ({ ...thought, anchor: 'other-task' }));
  const blocks = turnBlocks(thoughts, groupExecutionRounds(state.tools));
  assert.equal(blocks.filter(block => block.kind === 'thought').length, 4);
  assert.equal(blocks.filter(block => block.kind === 'receipt').flatMap(block => block.thoughts).length, 0);
});
