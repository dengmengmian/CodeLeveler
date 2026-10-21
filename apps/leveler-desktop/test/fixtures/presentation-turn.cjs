// Synthetic JSONL transport peer for the turn-presentation parity regression.
// No provider, Runtime or execution evidence is claimed by this fixture.
//
// It behaves like a real runtime for one turn: the live stream is emitted once,
// the durable log keeps every event, and a later snapshot/history reload
// reconstructs the same turn. That reload is what the desktop performs after a
// terminal event, so a fixture that answered with a static snapshot would test
// a state the product never sees.
const readline = require('node:readline');
const date = '2026-10-21T08:00:00Z';
const file = 'leveler-chat-20261021-000850.md';
const patch = `--- a/${file}\n+++ b/${file}\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context\n`;
const user = { id: 'u1', role: 'user', text: '把会话记录写入文件', seq: 0 };
const entries = [{ turn_elapsed_ms: 0, turn_start: true, event: { type: 'user_message_added', message: user } }];
const messages = [user];
let status = 'running';
let emitted = false;
let sequence = 1;
function send(frame) {
  process.stdout.write(JSON.stringify(frame) + '\n');
}
function event(session, data) {
  send({ event: 'runtime', session_id: session, data });
}
function record(session, data) {
  sequence += 1;
  entries.push({ turn_elapsed_ms: entries.length, turn_start: data.type === 'user_message_added', event: data });
  if (data.type === 'assistant_message_started') messages.push({ id: data.message_id, role: 'assistant', text: '' });
  if (data.type === 'assistant_text_delta') {
    const message = messages.find(item => item.id === data.message_id);
    if (message) message.text += data.delta;
  }
  event(session, data);
}
function snapshot() {
  return {
    id: 'parity',
    goal: 'Presentation parity',
    repository: '/synthetic/workspace',
    model: 'synthetic/render-fixture',
    task_status: status,
    last_sequence: sequence,
    messages,
    pending_interactions: [],
    active_tools: [],
    diff: null,
  };
}
function emitTurn(session) {
  if (emitted) return;
  emitted = true;
  record(session, { type: 'tool_call_started', id: 't1', name: 'apply_patch', arguments: JSON.stringify({ patch }), parallel: false });
  record(session, { type: 'tool_call_completed', id: 't1', ok: true, preview: 'applied', duration_ms: 120, applied_diff: patch, exit_code: null, stop: null });
  record(session, { type: 'assistant_message_started', message_id: 'a1' });
  record(session, { type: 'assistant_text_delta', message_id: 'a1', delta: '会话记录已写入，我再看一下目录确认位置。' });
  record(session, { type: 'tool_call_started', id: 't2', name: 'read_file', arguments: JSON.stringify({ path: '.' }), parallel: false });
  record(session, { type: 'tool_call_completed', id: 't2', ok: true, preview: file, duration_ms: 30, applied_diff: null, exit_code: null, stop: null });
  record(session, { type: 'assistant_message_started', message_id: 'a2' });
  record(session, { type: 'assistant_text_delta', message_id: 'a2', delta: `已写入 ${file} 并确认目录。` });
  record(session, { type: 'turn_answered' });
  // A refreshed workspace diff: the transcript must not repeat these files as
  // a conversation card; the Changes view owns that surface.
  record(session, { type: 'diff_updated', diff: { files: [{ path: file, added: 1, removed: 1 }] } });
  // The runtime settles its status a moment after the terminal event: the
  // snapshot at terminal time still reads running, and the correction arrives
  // as session_updated. A client that only re-renders on the terminal event
  // keeps the running row forever.
  setTimeout(() => {
    status = 'answered';
    sequence += 1;
    event(session, { type: 'session_updated', session: snapshot() });
  }, 400);
}
readline.createInterface({ input: process.stdin }).on('line', line => {
  const request = JSON.parse(line);
  const params = request.params ?? {};
  let result = {};
  if (request.method === 'list_tasks') result = {
    tasks: [{ id: 'parity', title: 'Presentation parity', source_id: 'synthetic', primary_workspace: '/synthetic/workspace', status, created_at: date, updated_at: date, last_activity_at: date }],
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
      const first = !emitted;
      event(session, { type: 'session_history_loaded', query_id: command.query_id, session_id: session, entries: [...entries], omitted_turns: 0 });
      if (first) setImmediate(() => emitTurn(session));
    }
    return;
  } else if (request.method !== 'connect') {
    send({ id: request.id, ok: false, error: { message: 'Unsupported synthetic method', kind: 'invalid_request' } });
    return;
  }
  send({ id: request.id, ok: true, result });
});
