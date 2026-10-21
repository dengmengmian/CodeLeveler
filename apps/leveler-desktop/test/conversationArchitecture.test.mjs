// The Desktop conversation's information architecture, as one projection.
//
// `conversationItems` is the projection `renderer.mjs` paints. These tests
// drive the real state reducer (the same one the window uses) and assert the
// order, classification and fold facts the reader sees: narration stays where
// it arrived, execution groups are siblings collapsed by default, a completed
// Thought paints no row, and the turn receipt names the runtime's own terminal.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { applyEvent, mergeRestoredConversation, projectSnapshot } from '../src/state.mjs';
import {
  confirmedChangeFiles, conversationItems, executionStatusLabel, executionView, receiptMarker,
} from '../src/presentation.mjs';

function state() {
  return projectSnapshot({ id: 's1', messages: [], task_status: 'idle' }, []);
}

function send(target, type, extra = {}) {
  return applyEvent(target, { type, ...extra });
}

function tool(target, id, name, { step = 1, args = '{}', effect = 'work', ok = true, preview = 'ok', patch = null, exit } = {}) {
  let next = send(target, 'tool_call_started', { id, name, arguments: args, model_step: step, answer_effect: effect });
  return send(next, 'tool_call_completed', {
    id, ok, preview, applied_diff: patch,
    ...(exit === undefined ? {} : { exit_code: exit }),
  });
}

function assistant(target, id, text) {
  let next = send(target, 'assistant_message_started', { message_id: id });
  next = send(next, 'assistant_text_delta', { message_id: id, delta: text });
  return send(next, 'assistant_message_completed', { message_id: id });
}

function kinds(items) { return items.map(item => item.kind); }

function executionItems(items) {
  return items.filter(item => item.kind === 'execution').map(item => item.execution);
}

test('pure chat is user, the answer, and its receipt — no execution card', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '你好' } });
  s = assistant(s, 'm1', '你好，我在。');
  s = send(s, 'turn_answered');
  const items = conversationItems(s);
  assert.deepEqual(kinds(items), ['user', 'final', 'turn_receipt']);
  assert.equal(items[1].text, '你好，我在。');
  assert.equal(items[1].streaming, false);
  assert.equal(items[2].terminal.status, 'answered');
  assert.equal(items[2].terminal.glyph, '✓');
  assert.match(items[2].terminal.stats.join(' · '), /1 轮/);
});

test('narration, execution and the final answer keep true arrival order', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '检查分支' } });
  s = assistant(s, 'm1', '我先检查远端引用和本地分支状态。');
  s = tool(s, 't1', 'run_command', { args: '{"cmd":"git fetch --all --prune"}', step: 1, preview: 'ok' });
  s = tool(s, 't2', 'run_command', { args: '{"cmd":"git status -sb"}', step: 1, preview: 'ok' });
  s = assistant(s, 'm2', '远端引用已经更新，我继续确认提交差异。');
  s = tool(s, 't3', 'run_command', { args: '{"cmd":"git log --oneline -5"}', step: 2, ok: false, preview: 'fatal: not a repository', exit: 128 });
  s = assistant(s, 'm3', '已定位问题，现在修改代码。');
  s = tool(s, 't4', 'apply_patch', {
    step: 3,
    patch: '--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n-a\n+b\n',
  });
  s = assistant(s, 'm4', '改好了。');
  s = send(s, 'turn_answered');
  const items = conversationItems(s);
  assert.deepEqual(kinds(items), [
    'user', 'narration', 'execution',
    'narration', 'execution', 'narration', 'execution',
    'final', 'turn_receipt',
  ]);
  assert.equal(items[1].text, '我先检查远端引用和本地分支状态。');
  assert.equal(items[3].text, '远端引用已经更新，我继续确认提交差异。');
  assert.equal(items[5].text, '已定位问题，现在修改代码。');
  assert.equal(items[7].text, '改好了。');
  assert.equal(items[2].execution.variant, 'tools');
  assert.equal(items[2].execution.label, '执行 2 条命令');
  assert.equal(items[4].execution.failure, 'fatal: not a repository');
  assert.equal(items[6].execution.variant, 'edit');
  assert.deepEqual(items[6].execution.files.map(file => file.path), ['src/a.rs', 'src/b.rs']);
  assert.equal(items[6].execution.added, 2);
  assert.equal(items[6].execution.removed, 2);
  assert.equal(items[6].execution.label, '修改 2 个文件 · +2 / -2');
});

test('consecutive exploration collapses into one reversible receipt, collapsed by default', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '看一下' } });
  for (const [index, name] of [['read_file'], ['grep'], ['glob']].entries()) {
    s = tool(s, `t${index}`, name[0], { step: 1, effect: 'bookkeeping', args: JSON.stringify({ path: `src/${index}.rs`, pattern: 'x' }) });
  }
  s = send(s, 'turn_completed');
  const [execution] = executionItems(conversationItems(s));
  assert.equal(execution.variant, 'exploration');
  assert.equal(execution.label, '读取 2 次 · 搜索 1 次');
  assert.equal(execution.collapsed, true);
  assert.deepEqual(execution.toolIds, ['t0', 't1', 't2']);
});

test('a failed explorer is not aggregated into a success receipt; its failure stays visible', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '看一下' } });
  s = tool(s, 't1', 'read_file', { step: 1, effect: 'bookkeeping', args: '{"path":"a"}' });
  s = tool(s, 't2', 'grep', { step: 1, effect: 'bookkeeping', args: '{"pattern":"x"}', ok: false, preview: 'error: no such directory' });
  s = send(s, 'turn_completed');
  const [execution] = executionItems(conversationItems(s));
  assert.equal(execution.variant, 'tools');
  assert.equal(execution.failed, 1);
  assert.equal(execution.failure, 'error: no such directory');
  assert.equal(executionStatusLabel(execution), '1 项失败');
});

test('different model steps stay different rounds even when both are exploration', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '找' } });
  s = tool(s, 't1', 'read_file', { step: 1, effect: 'bookkeeping', args: '{"path":"a"}' });
  s = tool(s, 't2', 'read_file', { step: 2, effect: 'bookkeeping', args: '{"path":"b"}' });
  s = send(s, 'turn_completed');
  const [execution] = executionItems(conversationItems(s));
  assert.deepEqual(execution.rounds, [1, 2], 'both real model steps remain on the receipt');
  assert.deepEqual(execution.toolIds, ['t1', 't2'], 'the members keep arrival order');
});

test('a confirmed edit shares its round with another call but still owns the summary', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '改并确认' } });
  s = send(s, 'tool_call_started', { id: 'e1', name: 'apply_patch', arguments: '{}', model_step: 1, answer_effect: 'work' });
  s = send(s, 'tool_call_completed', { id: 'e1', ok: true, preview: 'applied', applied_diff: '--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n' });
  s = tool(s, 'r1', 'read_file', { step: 1, effect: 'bookkeeping', args: '{"path":"src/a.ts"}' });
  s = send(s, 'turn_completed');
  const [execution] = executionItems(conversationItems(s));
  assert.equal(execution.variant, 'edit');
  assert.equal(execution.label, '修改 1 个文件 · +1 / -1 · 另有 1 项');
  assert.deepEqual(execution.editTools, ['e1']);
  assert.deepEqual(execution.toolIds, ['e1', 'r1'], 'the other call is still on the same real round');
});

test('a single command is its own row, not an "executed 1 item" parent', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '查' } });
  s = tool(s, 't1', 'run_command', { args: '{"cmd":"git status"}' });
  s = send(s, 'turn_completed');
  const [execution] = executionItems(conversationItems(s));
  assert.equal(execution.variant, 'tool');
  assert.equal(execution.toolIds.length, 1);
  assert.equal(execution.label, '运行命令');
  assert.equal(executionStatusLabel(execution), '全部成功');
});

test('a group keeps its stable identity while its members update', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '跑' } });
  s = tool(s, 't1', 'run_command', { args: '{"cmd":"a"}', step: 1 });
  const two = tool(s, 't2', 'run_command', { args: '{"cmd":"b"}', step: 1 });
  const [before] = executionItems(conversationItems(s));
  const [after] = executionItems(conversationItems(two));
  assert.equal(before.id, after.id, 'the first member owns the group identity');
  assert.equal(after.toolIds.length, 2);
});

test('streaming Thought is live; a completed Thought paints no conversation row', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '想' } });
  s = send(s, 'reasoning_started');
  s = send(s, 'reasoning_delta', { delta: '先看入口。' });
  let items = conversationItems(s);
  assert.equal(items.at(-1).kind, 'live_thought');
  assert.equal(items.at(-1).text, '先看入口。');
  s = send(s, 'reasoning_completed', { elapsed_ms: 1600 });
  items = conversationItems(s);
  assert.equal(items.some(item => item.kind === 'live_thought'), false);
  const thought = items.find(item => item.kind === 'thought');
  assert.ok(thought, 'the semantic Thought is retained');
  assert.equal(thought.visible, false, 'the settled Thought is not a conversation row');
  assert.equal(thought.thought.body, '先看入口。');
});

test('an interrupted Thought is kept and never presented as completed analysis', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '想' } });
  s = send(s, 'reasoning_started');
  s = send(s, 'reasoning_delta', { delta: '半句' });
  s = send(s, 'tool_call_started', { id: 't1', name: 'read_file', arguments: '{}', model_step: 1, answer_effect: 'bookkeeping' });
  const thought = conversationItems(s).find(item => item.kind === 'thought');
  assert.ok(thought, 'the interrupted segment survives the action');
  assert.equal(thought.thought.interrupted, true);
  assert.equal(thought.visible, false);
});

test('a terminal turn without an answer reads no_final_answer, never a green completion', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '跑' } });
  s = tool(s, 't1', 'run_command', { args: '{"cmd":"true"}' });
  s = send(s, 'turn_completed');
  const items = conversationItems(s);
  const receipt = items.find(item => item.kind === 'turn_receipt');
  assert.equal(receipt.terminal.status, 'no_final_answer');
  assert.equal(receipt.terminal.tone, 'warning');
  assert.equal(items.some(item => item.kind === 'final'), false);
  assert.deepEqual(receiptMarker('answered'), { glyph: '✓', label: '回答结束', tone: 'success' });
});

test('a cancelled or failed turn keeps its own status word', () => {
  let cancelled = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '停' } });
  cancelled = send(cancelled, 'turn_cancelled');
  assert.equal(conversationItems(cancelled).at(-1).terminal.status, 'cancelled');

  let failed = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '坏' } });
  failed = send(failed, 'turn_failed', { error: 'provider down' });
  const receipt = conversationItems(failed).find(item => item.kind === 'turn_receipt');
  assert.equal(receipt.terminal.status, 'failed');
  assert.equal(receipt.terminal.detail, 'provider down');
});

test('an approval request is attached to the group that is waiting, once', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '写' } });
  s = send(s, 'tool_call_started', { id: 't1', name: 'apply_patch', arguments: '{}', model_step: 1, answer_effect: 'work' });
  s = send(s, 'approval_requested', { request: { id: 'a1', call_id: 't1', tool: 'apply_patch', summary: 'write' } });
  let [execution] = executionItems(conversationItems(s));
  assert.equal(execution.waiting, true);
  assert.equal(executionStatusLabel(execution), '等待审批');
  assert.equal(JSON.stringify(conversationItems(s)).includes('a1'), false, 'the approval is not duplicated into the transcript');
  s = send(s, 'approval_resolved', { id: 'a1' });
  [execution] = executionItems(conversationItems(s));
  assert.equal(execution.waiting, false);
  // A waiting call in a multi-call group keeps the state on the collapsed
  // summary, where the single decision card cannot be missed.
  const rows = [
    { id: 'x1', name: 'run_command', status: 'running', modelStep: 1, preview: '', seq: 1 },
    { id: 'x2', name: 'run_command', status: 'ok', modelStep: 1, preview: 'ok', seq: 2 },
  ];
  const view = executionView({ kind: 'round', round: { modelStep: 1, tools: rows, status: 'running', allOk: false, batches: [] } }, rows, new Set(['x1']));
  assert.equal(view.waiting, true);
  assert.equal(executionStatusLabel(view), '等待审批');
});

test('runtime notices stay runtime-authored and compaction summaries stay summaries', () => {
  let s = send(state(), 'user_message_added', { message: { id: 'u1', role: 'user', text: '真的问题' } });
  s = send(s, 'user_message_added', { message: { id: 'n1', role: 'user', kind: 'runtime_notice', text: '子任务已结束' } });
  s = send(s, 'assistant_message_started', { message_id: 'm1' });
  s = send(s, 'assistant_text_delta', { message_id: 'm1', delta: '答' });
  s = send(s, 'turn_answered');
  let items = conversationItems(s);
  assert.deepEqual(kinds(items), ['user', 'notice', 'final', 'turn_receipt']);
  assert.equal(items[1].message.id, 'n1');

  let compacted = send(state(), 'user_message_added', { message: { id: 'u2', role: 'user', text: '之前聊过' }, seq: 0 });
  compacted.messages.push({ id: 'c1', role: 'user', kind: 'compaction_summary', text: '摘要正文', seq: 1 });
  compacted.messages.push({ id: 'm2', role: 'assistant', text: '答', seq: 2 });
  compacted = send(compacted, 'turn_answered', { seq: 3 });
  items = conversationItems(compacted);
  assert.deepEqual(kinds(items), ['user', 'summary', 'final', 'turn_receipt']);
  assert.equal(items[1].text, '摘要正文');
});

test('the runtime compaction prefix marks a replacement message, not a user turn', () => {
  const summary = { id: 'c1', role: 'user', text: '对话摘要（已压缩历史）：前面聊过解析器。', seq: 0 };
  const items = conversationItems({ messages: [summary], tools: [], turnTerminals: [] });
  assert.deepEqual(kinds(items), ['summary']);
  assert.equal(items[0].text, '前面聊过解析器。');
});

test('multiple historical turns never bleed work, files or receipts into each other', () => {
  const history = [
    { turn_elapsed_ms: 0, turn_start: true, event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: '第一轮' } } },
    { turn_elapsed_ms: 100, turn_start: false, event: { type: 'assistant_message_started', message_id: 'm1' } },
    { turn_elapsed_ms: 110, turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm1', delta: '第一答' } },
    { turn_elapsed_ms: 120, turn_start: false, event: { type: 'assistant_message_completed', message_id: 'm1' } },
    { turn_elapsed_ms: 3300, turn_start: false, event: { type: 'turn_answered' } },
    { turn_elapsed_ms: 0, turn_start: true, event: { type: 'user_message_added', message: { id: 'u2', role: 'user', text: '第二轮' } } },
    { turn_elapsed_ms: 40, turn_start: false, event: { type: 'tool_call_started', id: 'e1', name: 'apply_patch', arguments: '{}', model_step: 1, answer_effect: 'work' } },
    { turn_elapsed_ms: 50, turn_start: false, event: { type: 'tool_call_completed', id: 'e1', ok: true, preview: 'applied', applied_diff: '--- a/two.md\n+++ b/two.md\n@@ -1 +1 @@\n-a\n+b\n' } },
    { turn_elapsed_ms: 90, turn_start: false, event: { type: 'assistant_message_started', message_id: 'm2' } },
    { turn_elapsed_ms: 100, turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm2', delta: '第二答' } },
    { turn_elapsed_ms: 2000, turn_start: false, event: { type: 'turn_answered' } },
  ];
  const s = projectSnapshot({ id: 's', messages: [], task_status: 'answered' }, history);
  const items = conversationItems(s);
  assert.deepEqual(kinds(items), [
    'user', 'final', 'turn_receipt',
    'user', 'execution', 'final', 'turn_receipt',
  ]);
  assert.equal(items[2].terminal.elapsedMs, 3300, 'the durable turn elapsed, not a replay wall clock');
  assert.equal(items[2].terminal.endedHm, null, 'a replay has no clock');
  assert.deepEqual(items[4].execution.files.map(file => file.path), ['two.md']);
  assert.equal(items[6].terminal.elapsedMs, 2000);
  assert.equal(items.filter(item => item.kind === 'final').length, 2);
  assert.equal(items.filter(item => item.kind === 'turn_receipt').length, 2);
});

test('confirmed change files and the workspace snapshot are different facts', () => {
  const tools = [
    { id: 't1', status: 'success', appliedDiff: '--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n' },
    { id: 't2', status: 'success', appliedDiff: '--- a/src/b.ts\n+++ b/src/b.ts\n@@ -1 +1 @@\n-a\n+b\n' },
  ];
  assert.deepEqual(confirmedChangeFiles(['t1'], tools).map(file => file.path), ['src/a.ts']);
  // A workspace diff is a different object: present only when it was read, and
  // never substituted for the call's own confirmed history.
  assert.deepEqual(confirmedChangeFiles(['t1'], tools.map(tool => ({ ...tool, appliedDiff: null }))), []);
});

test('a history replay keeps live events that arrived while the query was in flight', () => {
  // The durable log ends at the first turn. The live state already contains
  // the next prompt and its wait for approval; replaying history must neither
  // drop those facts nor duplicate the first turn.
  const replayed = projectSnapshot({ id: 's', messages: [], task_status: 'answered' }, [
    { turn_elapsed_ms: 0, turn_start: true, event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: '第一轮' } } },
    { turn_elapsed_ms: 100, turn_start: false, event: { type: 'tool_call_started', id: 't1', name: 'run_command', arguments: '{}', model_step: 1, answer_effect: 'work' } },
    { turn_elapsed_ms: 200, turn_start: false, event: { type: 'tool_call_completed', id: 't1', ok: true, preview: 'ok' } },
    { turn_elapsed_ms: 300, turn_start: false, event: { type: 'assistant_message_started', message_id: 'm1' } },
    { turn_elapsed_ms: 310, turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm1', delta: '第一答' } },
    { turn_elapsed_ms: 400, turn_start: false, event: { type: 'turn_answered' } },
  ]);
  let live = send(replayed, 'user_message_added', { message: { id: 'u2', role: 'user', text: '第二轮' } });
  live = send(live, 'tool_call_started', { id: 'w1', name: 'write_file', arguments: '{}', model_step: 1, answer_effect: 'work' });
  live = send(live, 'approval_requested', { request: { id: 'a1', call_id: 'w1', tool: 'write_file', summary: '写' } });
  const lastDurable = Math.max(...replayed.messages.map(message => message.seq), ...replayed.tools.map(tool => tool.seq));
  const merged = mergeRestoredConversation(replayed, live);
  assert.deepEqual(merged.messages.map(message => message.id), ['u1', 'm1', 'u2']);
  assert.deepEqual(merged.tools.map(tool => tool.id), ['t1', 'w1']);
  assert.equal(merged.messages.filter(message => message.id === 'u1').length, 1, 'the replay duplicated the user line');
  assert.ok(merged.messages.find(message => message.id === 'u2').seq > lastDurable, 'the live turn starts after the durable log');
  assert.ok(merged.nextSeq > 3);
  const items = conversationItems(merged);
  assert.ok(items.some(item => item.kind === 'user' && item.message.id === 'u2'), 'the live prompt disappeared behind the replay');
  assert.ok(items.some(item => item.kind === 'execution' && item.execution.waiting), 'the waiting approval disappeared behind the replay');
});

test('a replay that regenerated user message ids does not duplicate the user line', () => {
  // The durable history projection creates a fresh MessageId for every user
  // fact, so id equality cannot identify the same line across a replay.
  const live = {
    ...projectSnapshot({ id: 's', messages: [], task_status: 'answered' }, []),
    messages: [
      { id: 'live-u1', role: 'user', text: '同一个问题', seq: 0, live: true },
      { id: 'live-u2', role: 'user', text: '同一个问题', seq: 1, live: true },
    ],
    nextSeq: 2,
  };
  const replayed = {
    ...projectSnapshot({ id: 's', messages: [], task_status: 'answered' }, []),
    messages: [
      { id: 'replay-u1', role: 'user', text: '同一个问题', seq: 0 },
      { id: 'replay-u2', role: 'user', text: '同一个问题', seq: 1 },
    ],
    nextSeq: 2,
  };
  const merged = mergeRestoredConversation(replayed, live);
  assert.deepEqual(merged.messages.map(message => message.id), ['replay-u1', 'replay-u2'], 'the same prompt was appended again');
  assert.equal(merged.messages.length, 2);
});

test('a snapshot active context is not promoted into the transcript once history exists', () => {
  const restored = projectSnapshot({ id: 's', messages: [], task_status: 'answered' }, [
    { turn_elapsed_ms: 0, turn_start: true, event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: '真正的问题' } } },
    { turn_elapsed_ms: 100, turn_start: false, event: { type: 'assistant_message_started', message_id: 'm1' } },
    { turn_elapsed_ms: 110, turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm1', delta: '真正回答' } },
  ]);
  const placeholder = projectSnapshot({
    id: 's',
    task_status: 'answered',
    messages: [
      { id: 'snapshot-u', role: 'user', text: '模型上下文里的旧问题' },
      { id: 'snapshot-m', role: 'assistant', text: '模型上下文里的旧回答' },
    ],
  });
  const merged = mergeRestoredConversation(restored, { ...placeholder, messages: [...placeholder.messages] });
  assert.deepEqual(merged.messages.map(message => message.id), ['u1', 'm1'], 'snapshot context leaked into the transcript');
  // A brand-new session with no history keeps its snapshot rows.
  const empty = mergeRestoredConversation(projectSnapshot({ id: 's', messages: [] }, []), placeholder);
  assert.deepEqual(empty.messages.map(message => message.id), ['snapshot-u', 'snapshot-m']);
});
