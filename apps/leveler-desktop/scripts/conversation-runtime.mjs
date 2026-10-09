// Called by conversation.mjs: actual native Runtime + Chromium/Electron acceptance.
// The model script is Dogfood's existing Python provider, never DOM injection.
import { chromium, _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { connect } from 'node:net';
import { spawn, spawnSync, execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, writeFile, readFile, readdir, lstat } from 'node:fs/promises';
import { createHash, randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const source = path.resolve(process.env.LEVELER_ACCEPTANCE_SOURCE ?? fileURLToPath(new URL('../../..', import.meta.url)));
const client = process.env.LEVELER_ACCEPTANCE_CLIENT ?? 'desktop';
assert.ok(['web', 'desktop'].includes(client), 'explicit client required');
const binary = path.resolve(process.env.LEVELER_BINARY ?? path.join(source, 'target/debug/leveler'));
const out = path.resolve(process.env.LEVELER_ACCEPTANCE_OUTPUT ?? path.join(source, 'apps/leveler-desktop/acceptance-output/conversation-runtime'));
const dog = path.resolve(process.env.DOGFOOD_ROOT ?? path.join(source, '../dogfood'));
const providerPath = path.join(dog, 'eval/pty/mock_provider.py');
await mkdir(out, { recursive: true });
const temp = await mkdtemp('/tmp/cl-p35-'), home = path.join(temp, 'home'), workspace = path.join(temp, 'workspace');
await mkdir(home); await mkdir(path.join(workspace, 'src'), { recursive: true });
const beforeFile = 'const RECOMMENDED: &[&str] = &["gpt-5.6"];\n';
const afterFile = 'const RECOMMENDED: &[&str] = &["gpt-6"];\n';
await writeFile(path.join(workspace, 'src/models.rs'), beforeFile);
await writeFile(path.join(workspace, 'src/config.rs'), 'const MODELS: &str = "recommended";\n');
const markers = ['先看 provider catalog。', '再核对推荐映射。', '跑一下映射测试。', '测试通过，确认 catalog 默认值没有回退。'];
const caseIds = ['W1_completed_thought', 'W2_confirmed_applied_diff', 'W3_cross_round_exploration', 'W4_interactive_chat', 'W5_finalization', 'W6_cancellation', 'W7_compaction_resume'];
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const git = args => execFileSync('git', args, { cwd: source, encoding: 'utf8' }).trim();
async function identity() {
  const scripts = {};
  for (const file of [path.join(source, 'apps/leveler-desktop/scripts/conversation.mjs'), fileURLToPath(import.meta.url), providerPath]) scripts[file] = sha(await readFile(file));
  return { commit: git(['rev-parse', 'HEAD']), source_status: git(['status', '--porcelain']), binary_sha256: sha(await readFile(binary)), binary_version: execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim(), scripts };
}
const report = { binary, client, renderer: client === 'web' ? 'chromium' : 'electron', renderer_started: false, identity: await identity(), results: [], failed: 0, home, workspace, scope: 'actual Runtime/Wire/Projection/Renderer; raw IPC is explicitly identified where Desktop exposes no command' };
const author = process.env.LEVELER_ACCEPTANCE_ALLOW_WIP === '1';
if (!author) { assert.equal(report.identity.source_status, '', 'candidate must be clean'); assert.ok(report.identity.binary_version.includes(report.identity.commit.slice(0, 12)), 'source/binary revision mismatch'); }
report.qualification = !author;
const resumeTransportFault = process.env.LEVELER_ACCEPTANCE_TEST_RESUME_DISCONNECT === '1';
assert.ok(!resumeTransportFault || (author && client === 'web'), 'transport fault requires explicit Web author mode');
const providers = [], requests = [], wire = [], pageErrors = [];
let application, browser, page, webChild, ownRuntime, proxy, base, token, phase = 'full', held, releaseLate;
let failure, ownSelectedSession, cancelAckObserved = false;
const alive = pid => { try { process.kill(pid, 0); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } };
async function poll(label, read, accept, timeout = 45000) {
  const end = Date.now() + timeout;
  for (;;) { const value = await read(); if (accept(value)) return value; if (Date.now() >= end) throw new Error(`${label} timeout: ${JSON.stringify(value)}`); await new Promise(resolve => setTimeout(resolve, 40)); }
}
function record(caseId, evidence) { report.results.push({ case: caseId, status: 'pass', evidence }); }
async function provider(scenario, name, marker = 'PTY-MARKER') {
  const child = spawn('python3', [providerPath, '--scenario', scenario, '--port-file', path.join(temp, name + '.port'), '--log-file', path.join(out, name + '-provider.log'), '--flag-file', path.join(temp, name + '.mode'), '--marker', marker]);
  providers.push(child); let output = '';
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(name + ' provider readiness timeout')), 15000);
    child.stdout.on('data', data => { output += data; const match = output.match(/MOCK_READY (\d+)/); if (match) { clearTimeout(timer); resolve(Number(match[1])); } });
    child.once('exit', code => { clearTimeout(timer); reject(new Error(name + ' provider exited ' + code)); });
  });
  return `http://127.0.0.1:${port}`;
}
function facts(id) {
  const code = `import sqlite3,pathlib,json,sys\nfor p in pathlib.Path(sys.argv[1]).rglob('sessions.db'):\n c=sqlite3.connect('file:'+str(p)+'?mode=ro',uri=True);c.row_factory=sqlite3.Row\n if c.execute('select 1 from sessions where id=?',(sys.argv[2],)).fetchone():\n  print(json.dumps({'db':str(p),'session':dict(c.execute('select * from sessions where id=?',(sys.argv[2],)).fetchone()),'turns':[dict(x) for x in c.execute('select * from turns where session_id=? order by ordinal',(sys.argv[2],))],'events':[dict(x) for x in c.execute('select * from events where session_id=? order by sequence',(sys.argv[2],))],'messages':[dict(x) for x in c.execute('select * from session_messages where session_id=? order by ordinal',(sys.argv[2],))]},ensure_ascii=False));break\nelse:raise RuntimeError('missing isolated session database')`;
  const result = spawnSync('python3', ['-c', code, home, id], { encoding: 'utf8' }); if (result.status !== 0) throw new Error(result.stderr); return JSON.parse(result.stdout);
}
async function rawIPC(command) {
  assert.ok(ownRuntime?.pid && ownRuntime.runtime_id, 'own runtime identity must already be observed');
  const sockets = [];
  async function visit(dir) { for (const name of await readdir(dir)) { const file = path.join(dir, name), stat = await lstat(file); if (stat.isSocket()) sockets.push(file); else if (stat.isDirectory()) await visit(file); } }
  await visit(path.join(home, 'run'));
  const wrap = body => ({ protocol: { major: 1, minor: 14 }, capabilities: ['optional_workspace'], body });
  async function request(socket, body) {
    return new Promise((resolve, reject) => { const stream = connect(socket); let bytes = Buffer.alloc(0); const timer = setTimeout(() => { stream.destroy(); reject(new Error('raw IPC timeout')); }, 15000);
      stream.on('connect', () => { const payload = Buffer.from(JSON.stringify(wrap(body))), size = Buffer.alloc(4); size.writeUInt32BE(payload.length); stream.write(Buffer.concat([size, payload])); });
      stream.on('error', reject); stream.on('data', data => { bytes = Buffer.concat([bytes, data]); if (bytes.length >= 4 && bytes.length >= bytes.readUInt32BE(0) + 4) { clearTimeout(timer); stream.end(); resolve(JSON.parse(bytes.subarray(4, bytes.readUInt32BE(0) + 4)).body); } });
    });
  }
  for (const socket of sockets) {
    const info = await request(socket, { type: 'runtime_info' });
    if (info.type !== 'runtime_info' || info.body.pid !== ownRuntime.pid || info.body.runtime_id !== ownRuntime.runtime_id) continue;
    const response = await request(socket, { type: 'deliver', body: wrap({ command_id: randomUUID(), session_id: command.session_id, issued_at: new Date().toISOString(), expected_version: null, command }) });
    assert.equal(response.type, 'ack', JSON.stringify(response)); return { socket, runtime_id: info.body.runtime_id, pid: info.body.pid, command, response };
  }
  throw new Error('No isolated socket matches observed RuntimeInfo identity');
}
async function http(route, body) {
  const response = await fetch(base + route, { method: body ? 'POST' : 'GET', headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json' }, ...(body ? { body: JSON.stringify(body) } : {}) });
  const result = await response.json(); if (!response.ok) throw new Error(`HTTP ${response.status}: ${JSON.stringify(result)}`); return result;
}
async function snapshot(id) { return client === 'web' ? http(`/api/sessions/${id}/snapshot`) : page.evaluate(id => window.desktop.snapshot(id), id); }
async function deliver(id, command, endpoint = base) {
  const commandId = randomUUID();
  if (client === 'desktop') return page.evaluate(({ id, command, commandId }) => window.desktop.deliver({ command_id: commandId, session_id: id, issued_at: new Date().toISOString(), expected_version: null, command }), { id, command, commandId });
  return page.evaluate(({ base, token, id, command, commandId }) => new Promise((resolve, reject) => {
    const ws = new WebSocket(base.replace('http', 'ws') + '/ws?token=' + encodeURIComponent(token) + '&session=' + encodeURIComponent(id)); const timer = setTimeout(() => { ws.close(); reject(new Error('WS ACK timeout')); }, 15000);
    ws.onopen = () => ws.send(JSON.stringify({ type: 'deliver', session_id: id, command_id: commandId, expected_version: null, command }));
    ws.onerror = () => { clearTimeout(timer); ws.close(); reject(new Error('WS error')); };
    ws.onmessage = event => { const frame = JSON.parse(event.data); if (['ack', 'error'].includes(frame.type) && frame.command_id === commandId) { clearTimeout(timer); ws.close(); resolve(frame); } };
  }), { base: endpoint, token, id, command, commandId });
}
async function sendUI(content) { if (client === 'web') { await page.locator('.composer textarea').fill(content); await page.getByRole('button', { name: '发送', exact: true }).click(); } else { await page.locator('#message').fill(content); await page.locator('#send').click(); } }
async function selected() { if (client === 'desktop') return await page.locator('.task.selected').count() ? page.locator('.task.selected').getAttribute('data-session-id') : null; if (await page.locator('.sess.active').count() !== 1) return null; return page.evaluate(() => localStorage.getItem('leveler.web.lastSession')); }
async function thoughts() {
  if (client === 'web') {
    while (await page.locator('.exploration-head[aria-expanded="false"]').count()) await page.locator('.exploration-head[aria-expanded="false"]').first().click();
    while (await page.locator('.thought-head[aria-expanded="false"]').count()) await page.locator('.thought-head[aria-expanded="false"]').first().click();
  } else {
    for (const receipt of await page.locator('details.exploration').all()) if (await receipt.getAttribute('open') === null) await receipt.locator(':scope > summary').click();
    for (const thought of await page.locator('details.thought').all()) if (await thought.getAttribute('open') === null) await thought.locator(':scope > summary').click();
  }
  return page.locator('.thought-body').allTextContents();
}
async function assertPresentation(id, name) {
  await poll(name + ' four completed Thought bodies', thoughts, bodies => markers.every(marker => bodies.filter(body => body === marker).length === 1));
  const bodies = await thoughts(); assert.deepEqual(bodies.filter(body => markers.includes(body)), markers);
  const diffs = page.locator('.confirmed-diff'); assert.ok(await diffs.first().isVisible(), name + ' canonical diff hidden');
  const patch = await diffs.allTextContents(); for (const marker of ['--- a/src/models.rs', '+++ b/src/models.rs', 'gpt-5.6', 'gpt-6']) assert.ok(patch.join('').includes(marker), marker);
  const labels = await page.locator(client === 'web' ? '.exploration-label' : '.exploration > summary').allTextContents(); assert.equal(labels.length, 1); assert.ok(labels[0].includes('读取 2 个文件 · 搜索 2 次'));
  assert.equal((await snapshot(id)).collaboration, 'chat');
  await page.screenshot({ path: path.join(out, name + '.png'), fullPage: true });
  return { thought_bodies: bodies, receipt_labels: labels, confirmed_diff_text: patch, full_diff_visible: true };
}
async function reopen(id) {
  if (client === 'web') { await page.reload(); await page.locator('.composer textarea').waitFor(); await poll('automatic browser selection restore', selected, value => value === id); }
  else { await page.evaluate(async id => { const index = await window.desktop.listTasks(); const task = index.tasks.find(task => task.id === id); if (!task) throw new Error('own task missing from actual index'); return window.desktop.openTask(task); }, id); await page.locator(`.task.selected[data-session-id="${id}"]`).waitFor(); await deliver(id, { type: 'query_session_history', session_id: id, query_id: randomUUID() }); }
}
try {
  const full = await provider('full_execution', 'full'), cancel = await provider('reasoning_long', 'cancel'), continuation = await provider('chat', 'continuation', 'CONTINUATION_AFTER_COMPACT');
  const latePayloads = [];
  proxy = createServer(async (request, response) => {
    try { let raw = ''; for await (const chunk of request) raw += chunk; const doc = JSON.parse(raw); requests.push({ phase, body: doc });
      const upstream = await fetch((phase === 'cancel' ? cancel : phase === 'continue' ? continuation : full) + request.url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: raw });
      const body = await upstream.text(); response.writeHead(upstream.status, { 'content-type': upstream.headers.get('content-type') });
      if (phase === 'cancel' && doc.stream === true && doc.tools?.length) {
        const decoded = body.split('\n\n').filter(frame => frame.startsWith('data: ') && !frame.includes('[DONE]')).map(frame => JSON.parse(frame.slice(6)));
        assert.ok(decoded.some(frame => frame.choices?.[0]?.delta?.content === '长推理结束。'), 'same Python reasoning_long final required');
        const firstEnd = body.indexOf('\n\n') + 2; assert.ok(firstEnd > 1);
        response.write(body.slice(0, firstEnd)); held = { first_delta_sent: true, final_marker: '长推理结束。', remaining_bytes: body.length - firstEnd };
        releaseLate = () => { if (held.released) return; held.released = true; latePayloads.push({ ...held, released_after_cancel_ack: cancelAckObserved, peer_closed: response.destroyed }); response.end(body.slice(firstEnd)); };
      } else response.end(body);
    } catch (error) { response.destroy(error); failure ??= error; }
  });
  await new Promise(resolve => proxy.listen(0, '127.0.0.1', resolve));
  await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/main"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "http://127.0.0.1:${proxy.address().port}"\napi_key = "isolated-test-only"\n[models.main]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\n`, { mode: 0o600 });
  if (client === 'web') {
    const web = path.join(source, 'crates/leveler-web/web'); const build = spawnSync('npm', ['run', 'build'], { cwd: web, encoding: 'utf8' }); await writeFile(path.join(out, 'frontend-build.log'), build.stdout + build.stderr); assert.equal(build.status, 0, 'matching source frontend build failed');
    const dist = path.join(web, 'dist'); report.frontend_dist = dist; report.frontend_sha256 = {}; for (const file of ['index.html', ...(await readdir(path.join(dist, 'assets'))).map(name => 'assets/' + name)]) report.frontend_sha256[file] = sha(await readFile(path.join(dist, file)));
    let output = ''; webChild = spawn(binary, ['--repo', workspace, 'web', '--addr', '127.0.0.1:0', '--model', 'fixture/main', '--permission', 'assisted'], { cwd: workspace, env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_WEB_DIST: dist } }); webChild.stdout.on('data', data => output += data); webChild.stderr.on('data', data => output += data);
    const url = await poll('own Web readiness', () => output.match(/url: (http:\/\/127\.0\.0\.1:\d+\/\?token=[^\s]+)/)?.[1], Boolean); const parsed = new URL(url); base = parsed.origin; token = parsed.searchParams.get('token'); report.own_web_pid = webChild.pid;
    browser = await chromium.launch({ headless: true }); page = await browser.newPage({ viewport: { width: 1440, height: 1200 } }); report.renderer_started = true;
    page.on('websocket', socket => socket.on('framereceived', frame => { try { wire.push(JSON.parse(frame.payload)); } catch {} })); await page.goto(url); await page.locator('.composer textarea').waitFor();
  } else {
    application = await electron.launch({ args: [path.join(source, 'apps/leveler-desktop'), `--user-data-dir=${path.join(temp, 'electron-profile')}`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: binary, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' } }); page = await application.firstWindow(); report.renderer_started = true;
    await page.waitForFunction(() => window.desktop && document.querySelector('#refresh')?.disabled === false);
    await application.evaluate(({ dialog }, directory) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [directory] }); }, workspace); await page.locator('#space-add').click(); await page.waitForFunction(() => document.querySelector('#status').textContent === '未连接');
    await page.evaluate(() => { window.__presentationWire = []; window.desktop.onEvent(frame => window.__presentationWire.push(frame)); });
  }
  page.setDefaultTimeout(20000); page.on('pageerror', error => pageErrors.push(error.message));
  let id;
  if (client === 'web') { const creating = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes('/api/sessions/identified')); await sendUI('看下现在最新模型，同步更新配置'); id = (await (await creating).json()).session.id; }
  else { await sendUI('看下现在最新模型，同步更新配置'); id = await poll('selected own Desktop session', selected, Boolean); ownRuntime = await page.evaluate(() => window.desktop.runtimeInfo()); assert.ok(Number.isInteger(ownRuntime.pid) && ownRuntime.pid > 0); report.own_runtime = ownRuntime; }
  report.session_id = id; ownSelectedSession = id;
  await poll('native final committed', () => snapshot(id), value => value.messages.some(message => message.text.includes('已同步最新模型。')) && value.task_status !== 'running');
  const live = await assertPresentation(id, 'full-execution-live'); const durableBefore = facts(id); await writeFile(path.join(out, 'durable-before-compact.json'), JSON.stringify(durableBefore, null, 2));
  const events = client === 'web' ? wire.filter(frame => frame.event).map(frame => frame.event) : (await page.evaluate(() => window.__presentationWire)).filter(frame => frame.event === 'runtime').map(frame => frame.data);
  const completed = events.filter(event => event.type === 'reasoning_completed'); assert.equal(completed.length, 4); assert.deepEqual(completed.map(event => event.elapsed_ms), [0, 0, 0, 0]);
  record(caseIds[0], { wire_completed: completed, ...live });
  const patch = events.find(event => event.type === 'tool_call_completed' && event.id === 'c5'); assert.ok(patch?.applied_diff); assert.equal(await readFile(path.join(workspace, 'src/models.rs'), 'utf8'), afterFile);
  record(caseIds[1], { filesystem_before: beforeFile, filesystem_after: afterFile, matched_tool_id: 'c5', wire_applied_diff: patch.applied_diff, ...live });
  const exploration = events.filter(event => event.type === 'tool_call_started' && ['c1', 'c2', 'c3', 'c4'].includes(event.id)).map(event => ({ id: event.id, model_step: event.model_step }));
  assert.deepEqual(exploration, [{ id: 'c1', model_step: 1 }, { id: 'c2', model_step: 1 }, { id: 'c3', model_step: 2 }, { id: 'c4', model_step: 2 }]);
  record(caseIds[2], { tool_ids: exploration, ...live });
  const collaboration = (await snapshot(id)).collaboration; assert.equal(collaboration, 'chat');
  record(caseIds[3], { ui_created_collaboration: collaboration });
  assert.equal(await page.locator('.thought.live').count(), 0); assert.equal(await page.getByText('已同步最新模型。', { exact: true }).count(), 1);
  record(caseIds[4], { native_terminal: (await snapshot(id)).task_status, final_count: 1, live_activity_cleared: true });
  // Actually compact the model context, then replay the durable transcript.
  const beforeContext = await snapshot(id); let compactTransport;
  if (client === 'web') { await sendUI('/compact'); compactTransport = 'web_ui_ws'; }
  else { report.compact_raw_ipc = await rawIPC({ type: 'compact_context', session_id: id }); compactTransport = 'raw_ipc'; }
  const afterCompact = await poll('actual durable ContextCompacted', () => facts(id), value => value.events.some(event => /compact/i.test(event.type)));
  const compactFacts = afterCompact.events.filter(event => /compact/i.test(event.type)); assert.equal(compactFacts.length, 1);
  assert.deepEqual(JSON.parse(compactFacts[0].payload).payload, { from: durableBefore.messages.length, to: 1 });
  const compactSnapshot = await snapshot(id); assert.equal(compactSnapshot.messages.length, 1, 'model context was not compacted to one summary');
  assert.ok(compactSnapshot.messages.some(message => /摘要|压缩/.test(message.text)), 'actual compacted model context summary missing');
  await reopen(id); const replay = await assertPresentation(id, 'after-actual-compact-history-replay');
  assert.equal(facts(id).messages.length, 1, 'durable replay wrote full history back into model context');
  phase = 'continue'; await sendUI('继续'); await poll('ordinary continuation after compact', () => snapshot(id), value => value.messages.some(message => message.text.includes('CONTINUATION_AFTER_COMPACT')) && value.task_status !== 'running');
  const afterResume = facts(id); const resumePayload = { kind: afterResume.turns.at(-1)?.kind, payload: JSON.parse(afterResume.turns.at(-1)?.payload ?? '{}') };
  assert.ok(afterResume.turns.length > durableBefore.turns.length, 'continuation turn not persisted');
  assert.equal(resumePayload.payload.continuation_root_turn_id, durableBefore.turns[0].id);
  assert.equal(resumePayload.payload.objective.primary, '看下现在最新模型，同步更新配置');
  const continuationRequest = requests.find(request => request.phase === 'continue' && request.body.stream === true && request.body.tools?.length);
  assert.ok(continuationRequest, 'actual continuation model request missing');
  assert.ok(continuationRequest.body.messages.some(message => message.role === 'user' && message.content === compactSnapshot.messages[0].text), 'actual model request did not receive the compacted summary');
  assert.equal(continuationRequest.body.messages.filter(message => message.role === 'assistant').length, 0, 'replayed assistant history expanded the model context');
  assert.equal((await snapshot(id)).collaboration, 'chat'); await assertPresentation(id, 'after-compact-continuation');
  record(caseIds[6], { compact_command_transport: compactTransport, compact_facts: afterCompact.events.filter(event => /compact/i.test(event.type)), model_context_before: beforeContext.messages, model_context_after: compactSnapshot.messages, durable_history_before: durableBefore.events.length, durable_history_after: afterResume.events.length, resume_initiating_kind: resumePayload, replay });
  await writeFile(path.join(out, 'durable-after-compact-resume.json'), JSON.stringify(afterResume, null, 2));
  // Cancellation holds a real Python SSE stream after its first delta. Only
  // after native CancelTask ACK+Cancelled is the old response released.
  phase = 'cancel';
  let cancelId;
  if (client === 'web') { await page.getByRole('button', { name: 'New Task', exact: true }).click(); const creating = page.waitForResponse(response => response.request().method() === 'POST' && response.url().includes('/api/sessions/identified')); await sendUI('受控取消：在真实推理流中停止'); cancelId = (await (await creating).json()).session.id; }
  else { await page.locator('#new-task').click(); await sendUI('受控取消：在真实推理流中停止'); cancelId = await poll('cancel session selection', selected, value => value && value !== id); }
  ownSelectedSession = cancelId;
  await poll('real held SSE reasoning arrived', () => page.locator('.thought.live').count(), count => count === 1); assert.ok(held && releaseLate, 'provider gate handshake missing');
  const ack = await deliver(cancelId, { type: 'cancel_task', session_id: cancelId }); if (client === 'web') assert.equal(ack.type, 'ack'); else assert.equal(ack.ok, true, JSON.stringify(ack));
  cancelAckObserved = true;
  await poll('permanent native Task Cancelled', () => snapshot(cancelId), value => value.task_status === 'cancelled');
  releaseLate(); await deliver(cancelId, { type: 'query_session_history', session_id: cancelId, query_id: randomUUID() });
  await poll('native cancellation terminal rendered with no live thought', () => page.locator('.thought.live').count(), count => count === 0);
  const cancelled = facts(cancelId); assert.equal((await snapshot(cancelId)).task_status, 'cancelled');
  assert.ok(!JSON.stringify(cancelled).includes('长推理结束。'), 'late provider final persisted after cancellation'); assert.equal(await page.getByText('长推理结束。', { exact: true }).count(), 0); assert.equal(await page.locator('.thought.live').count(), 0);
  let resumeEndpoint = base;
  if (resumeTransportFault) {
    const closed = createServer(); await new Promise(resolve => closed.listen(0, '127.0.0.1', resolve));
    resumeEndpoint = `http://127.0.0.1:${closed.address().port}`; await new Promise(resolve => closed.close(resolve));
    report.resume_transport_fault = { endpoint: resumeEndpoint, own_loopback_listener_closed: true };
  }
  const refusal = await deliver(cancelId, { type: 'resume_task', session_id: cancelId, content: '继续' }, resumeEndpoint);
  if (client === 'desktop') {
    assert.equal(refusal.ok, false, 'Cancelled task reopened'); assert.equal(refusal.error?.kind, 'rejected');
    assert.ok(refusal.error.message.includes('该任务已被取消，无法继续'), 'missing Runtime cancellation refusal');
  } else {
    // deliver resolves only frames correlated to the issued command ID.
    assert.equal(refusal.type, 'error', 'Cancelled task reopened'); assert.equal(refusal.code, 'runtime_error');
    assert.ok(refusal.command_id, 'uncorrelated Runtime refusal');
    assert.ok(refusal.message.includes('该任务已被取消，无法继续'), 'missing Runtime cancellation refusal');
  }
  assert.equal((await snapshot(cancelId)).task_status, 'cancelled');
  record(caseIds[5], { cancel_ack: ack, cancelled_status: 'cancelled', late_provider_payload: latePayloads, durable_late_final_absent: true, dom_late_final_absent: true, resume_refusal: refusal });
  await writeFile(path.join(out, 'durable-cancelled.json'), JSON.stringify(cancelled, null, 2));
  await page.screenshot({ path: path.join(out, 'cancelled.png'), fullPage: true });
  assert.equal(pageErrors.length, 0, pageErrors.join('\n')); if (failure) throw failure;
} catch (error) { failure = error; report.error = error.stack; if (page) await page.screenshot({ path: path.join(out, 'failure.png'), fullPage: true }).catch(() => {}); }
finally {
  if (client === 'desktop' && page && !page.isClosed()) wire.push(...await page.evaluate(() => window.__presentationWire ?? []).catch(() => []));
  await writeFile(path.join(out, 'wire-frames.json'), JSON.stringify(wire, null, 2)); await writeFile(path.join(out, 'actual-model-requests.json'), JSON.stringify(requests, null, 2));
  // Cleanup is scoped to the session this consumer created and selected.
  if (failure && ownSelectedSession && page && !page.isClosed()) {
    const own = await snapshot(ownSelectedSession).catch(() => null);
    if (own?.task_status === 'running') report.failure_cleanup_stop = await deliver(ownSelectedSession, { type: 'cancel_current_turn', session_id: ownSelectedSession }).catch(error => ({ error: error.message }));
  }
  if (application) await application.close(); if (browser) await browser.close();
  const cleanup = { own_runtime: ownRuntime, web_pid: webChild?.pid, web_terminated_by_own_spawn_handle: false, desktop_normal_idle_exit_observed: false, provider_pids: providers.map(child => child.pid) };
  if (webChild && webChild.exitCode === null && webChild.signalCode === null) { webChild.kill('SIGTERM'); await poll('own Web child exits normally', () => webChild.exitCode !== null || webChild.signalCode !== null, Boolean); cleanup.web_terminated_by_own_spawn_handle = true; }
  if (ownRuntime) { await poll('own Desktop Runtime normal idle exit', () => !alive(ownRuntime.pid), Boolean); cleanup.desktop_normal_idle_exit_observed = true; }
  releaseLate?.(); if (proxy) await new Promise(resolve => { proxy.close(resolve); proxy.closeAllConnections(); });
  for (const child of providers) { if (child.exitCode === null && child.signalCode === null) { child.kill('SIGTERM'); await poll('own provider exits', () => child.exitCode !== null || child.signalCode !== null, Boolean); } }
  cleanup.all_own_runtime_pids_gone = [ownRuntime?.pid, webChild?.pid].filter(Boolean).every(pid => !alive(pid)); cleanup.all_provider_pids_gone = providers.every(child => !alive(child.pid)); report.cleanup = cleanup; report.page_errors = pageErrors;
  for (const id of caseIds) if (!report.results.some(result => result.case === id)) report.results.push({ case: id, status: 'fail', error: failure?.message ?? 'required case was not reached' });
  report.failed = report.results.filter(result => result.status !== 'pass').length; report.identity_after = await identity(); assert.deepEqual(report.identity_after, report.identity, 'source/binary/script identity changed during run'); report.passed = !failure && report.failed === 0 && cleanup.all_own_runtime_pids_gone && cleanup.all_provider_pids_gone;
  await writeFile(path.join(out, 'conversation-evidence.json'), JSON.stringify(report, null, 2) + '\n');
}
if (failure) throw failure;
assert.equal(report.failed, 0);
