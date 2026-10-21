// Synthetic JSONL transport peer for the conversation information-architecture
// acceptance. It behaves like the runtime's event stream (live, snapshot and
// durable history) but claims no provider or tool execution: the tool results,
// diffs and terminals below are fixture facts.
const readline = require('node:readline');
const date = '2026-10-21T08:00:00Z';

const sessionId = 'architecture';
const messages = [];
const tools = [];
const entries = [];
let status = 'idle';
let sequence = 0;
let pendingApproval = null;
let emittedRunning = false;

function send(frame) { process.stdout.write(JSON.stringify(frame) + '\n'); }
function event(session, data) { send({ event: 'runtime', session_id: session, data }); }
function record(session, data) {
  sequence += 1;
  entries.push({ turn_elapsed_ms: entries.length * 40, turn_start: data.type === 'user_message_added', event: data });
  if (data.type === 'assistant_message_started') messages.push({ id: data.message_id, role: 'assistant', text: '' });
  if (data.type === 'assistant_text_delta') {
    const message = messages.find(item => item.id === data.message_id);
    if (message) message.text += data.delta;
  }
  event(session, data);
}
function snapshot() {
  return {
    id: sessionId,
    goal: 'Conversation architecture',
    repository: '/synthetic/architecture',
    model: 'synthetic/render-fixture',
    task_status: status,
    last_sequence: sequence,
    messages,
    pending_interactions: pendingApproval ? [{ type: 'approval', request: pendingApproval }] : [],
    active_tools: [],
    plan: null,
    // A workspace-level diff is a different fact from any call's applied_diff.
    diff: {
      files: [{
        path: 'src/workspace-only.ts', added: 1, removed: 0,
        patch: '--- a/src/workspace-only.ts\n+++ b/src/workspace-only.ts\n@@ -0,0 +1 @@\n+workspace\n',
      }],
    },
  };
}
function toolStarted(id, name, args, step, effect) {
  tools.push({ id, name, arguments: JSON.stringify(args), status: 'running', model_step: step, answer_effect: effect });
  record(sessionId, { type: 'tool_call_started', id, name, arguments: JSON.stringify(args), parallel: false, model_step: step, answer_effect: effect });
}
function toolFinished(id, { ok = true, preview = 'ok', patch = null, exit = null } = {}) {
  const tool = tools.find(item => item.id === id);
  if (tool) Object.assign(tool, { status: ok ? 'success' : 'failed', preview, applied_diff: patch, exit_code: exit });
  record(sessionId, { type: 'tool_call_completed', id, ok, preview, applied_diff: patch, exit_code: exit, stop: null });
}
function assistant(id, text) {
  record(sessionId, { type: 'assistant_message_started', message_id: id });
  record(sessionId, { type: 'assistant_text_delta', message_id: id, delta: text });
  record(sessionId, { type: 'assistant_message_completed', message_id: id });
}
function finish(type = 'turn_answered', extra = {}) {
  status = 'answered';
  record(sessionId, { type, ...extra });
  sequence += 1;
  event(sessionId, { type: 'session_updated', session: snapshot() });
}

const PATCH = [
  '--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-old-a\n+new-a\n',
  '--- a/src/b.ts\n+++ b/src/b.ts\n@@ -1,2 +1,2 @@\n-old-b1\n-old-b2\n+new-b1\n+new-b2\n',
  '--- a/src/c.ts\n+++ b/src/c.ts\n@@ -1 +1 @@\n-old-c\n+new-c\n',
].join('');

function pureChat(session) {
  assistant('chat-1', '这是纯聊天回答。');
  finish();
}
function investigation(session) {
  assistant('inv-1', '我先检查配置和测试。');
  toolStarted('r1', 'read_file', { path: 'src/config.ts' }, 1, 'bookkeeping');
  toolFinished('r1', { preview: 'export const config = {}' });
  toolStarted('r2', 'read_file', { path: 'src/models.ts' }, 1, 'bookkeeping');
  toolFinished('r2', { preview: 'export const models = []' });
  toolStarted('s1', 'grep', { pattern: 'recommended' }, 1, 'bookkeeping');
  toolFinished('s1', { preview: 'src/config.ts:1:recommended' });
  toolStarted('c1', 'run_command', { cmd: 'git status -sb' }, 2, 'work');
  toolFinished('c1', { preview: '## main', exit: 0 });
  toolStarted('c2', 'run_command', { cmd: 'cargo test -p demo' }, 2, 'work');
  toolFinished('c2', { ok: false, preview: 'test mapping::recommended ... FAILED\nexit: 1', exit: 1 });
  assistant('inv-2', '运行结果里有一条失败，我先修复。');
  toolStarted('e1', 'apply_patch', { patch: PATCH }, 3, 'work');
  toolFinished('e1', { preview: 'applied 3 files', patch: PATCH });
  assistant('inv-3', '已定位问题，代码修改完成。');
  finish();
}
function approvalTurn(session) {
  toolStarted('w1', 'write_file', { path: 'src/new-file.ts' }, 1, 'work');
  pendingApproval = { id: 'approval-1', call_id: 'w1', tool: 'write_file', summary: '写入 src/new-file.ts', command: null, risks: ['新建文件'], grant: null, requires_human_consent: false, always_persists: true };
  status = 'waiting_user';
  record(sessionId, { type: 'approval_requested', request: pendingApproval });
}
function finishApproval(session, decision) {
  const approved = decision === 'approve_once' || decision === 'approve_session' || decision === 'approve_always';
  pendingApproval = null;
  record(sessionId, { type: 'approval_resolved', id: 'approval-1' });
  if (approved) {
    toolFinished('w1', { preview: 'written', patch: '--- a/src/new-file.ts\n+++ b/src/new-file.ts\n@@ -0,0 +1 @@\n+new\n' });
    assistant('approval-1', '写入已获批准并完成。');
  } else {
    toolFinished('w1', { ok: false, preview: 'denied by user' });
    assistant('approval-1', '已取消写入。');
  }
  finish();
}
function runningTurn(session) {
  toolStarted('run-1', 'run_command', { cmd: 'sleep 2' }, 1, 'work');
  record(sessionId, { type: 'command_progress', label: 'sleep 2', elapsed_ms: 1200 });
  setTimeout(() => {
    if (emittedRunning) return;
    emittedRunning = true;
    toolFinished('run-1', { preview: 'done', exit: 0 });
    assistant('run-2', '命令执行完成。');
    finish();
  }, 2500);
}

readline.createInterface({ input: process.stdin }).on('line', line => {
  const request = JSON.parse(line);
  const params = request.params ?? {};
  let result = {};
  if (request.method === 'list_tasks') result = {
    tasks: [{ id: sessionId, title: 'Conversation architecture', source_id: 'synthetic', primary_workspace: '/synthetic/architecture', status, created_at: date, updated_at: date, last_activity_at: date }],
    source_errors: [],
  };
  else if (request.method === 'open') result = { session: snapshot() };
  else if (request.method === 'snapshot') result = snapshot();
  else if (request.method === 'runtime_info') result = { synthetic: true };
  else if (request.method === 'deliver') {
    const command = params.envelope.command;
    const session = params.envelope.session_id;
    send({ id: request.id, ok: true, result: { ok: true } });
    if (command.type === 'query_session_history') {
      event(session, { type: 'session_history_loaded', query_id: command.query_id, session_id: session, entries: [...entries], omitted_turns: 0 });
      return;
    }
    if (command.type === 'request_diff') {
      event(session, { type: 'diff_updated', query_id: command.query_id, diff: snapshot().diff });
      return;
    }
    if (command.type === 'submit_message') {
      const text = command.content ?? '';
      status = 'running';
      record(session, { type: 'user_message_added', message: { id: `u-${entries.length}`, role: 'user', text } });
      if (text.includes('纯聊天')) pureChat(session);
      else if (text.includes('调查')) investigation(session);
      else if (text.includes('审批')) approvalTurn(session);
      else if (text.includes('运行中')) runningTurn(session);
      return;
    }
    if (command.type === 'approval_decision') {
      finishApproval(session, command.decision);
      return;
    }
    return;
  } else if (request.method !== 'connect') {
    send({ id: request.id, ok: false, error: { message: 'Unsupported synthetic method', kind: 'invalid_request' } });
    return;
  }
  send({ id: request.id, ok: true, result });
});
