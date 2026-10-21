// Synthetic JSONL transport peer for the analysis-row presentation regression.
// No provider, Runtime or execution evidence is claimed by this fixture.
const readline = require('node:readline');
const date = '2026-10-21T08:00:00Z';
const messages = [{ id: 'u1', role: 'user', text: '可以', seq: 1 }];
const snapshot = {
  id: 'thoughts',
  goal: 'Analysis row order',
  repository: null,
  model: 'synthetic/render-fixture',
  task_status: 'running',
  messages,
  pending_interactions: [],
  active_tools: [],
};
function send(frame) {
  process.stdout.write(JSON.stringify(frame) + '\n');
}
function event(session, data) {
  send({ event: 'runtime', session_id: session, data });
}
readline.createInterface({ input: process.stdin }).on('line', line => {
  const request = JSON.parse(line);
  const params = request.params ?? {};
  let result = {};
  if (request.method === 'list_tasks') result = {
    tasks: [{ id: snapshot.id, title: snapshot.goal, source_id: 'synthetic', primary_workspace: null, status: snapshot.task_status, created_at: date, updated_at: date, last_activity_at: date }],
    source_errors: [],
  };
  else if (request.method === 'open') result = { session: snapshot };
  else if (request.method === 'snapshot') result = snapshot;
  else if (request.method === 'runtime_info') result = { synthetic: true };
  else if (request.method === 'deliver') {
    const command = params.envelope.command;
    const session = params.envelope.session_id;
    send({ id: request.id, ok: true, result: { ok: true } });
    if (command.type === 'query_session_history') setImmediate(() => {
      event(session, { type: 'session_history_loaded', query_id: command.query_id, session_id: session, entries: [], omitted_turns: 0 });
      event(session, { type: 'reasoning_started' });
      event(session, { type: 'reasoning_delta', delta: 'First line of reasoning.\n关于 tools/list 的检查：tools/list 走同样的 Host' });
      event(session, { type: 'reasoning_completed', elapsed_ms: 2300 });
      event(session, { type: 'assistant_message_started', message_id: 'a1' });
      event(session, { type: 'assistant_text_delta', message_id: 'a1', delta: '好，我把运营端 MCP 验证固化成回归用例。先看下 lib.sh 的辅助函数和现有用例的完整写法：' });
      event(session, { type: 'reasoning_started' });
      event(session, { type: 'reasoning_delta', delta: '正在核对第二个理由。\nThe user repeated the same question: "你好这是什么东西？" — "Hello, what is this?" Still no attachment reached this side, so I cannot guess what "this" refers to.' });
    });
    return;
  } else if (request.method !== 'connect') {
    send({ id: request.id, ok: false, error: { message: 'Unsupported synthetic method', kind: 'invalid_request' } });
    return;
  }
  send({ id: request.id, ok: true, result });
});
