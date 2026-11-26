// Called by conversation.mjs: actual native Runtime + Electron acceptance.
// The model script is Dogfood's existing Python provider, never DOM injection.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { connect } from 'node:net';
import { spawn, spawnSync, execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, writeFile, readFile, readdir, lstat } from 'node:fs/promises';
import { readFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const source = path.resolve(process.env.LEVELER_ACCEPTANCE_SOURCE ?? fileURLToPath(new URL('../../..', import.meta.url)));
const client = 'desktop';
const binary = path.resolve(process.env.LEVELER_BINARY ?? path.join(source, 'target/debug/leveler'));
const out = path.resolve(process.env.LEVELER_ACCEPTANCE_OUTPUT ?? path.join(source, 'test-output/desktop/conversation-runtime'));
const dog = path.resolve(process.env.DOGFOOD_ROOT ?? path.join(source, '../dogfood'));
const providerPath = path.join(dog, 'eval/pty/mock_provider.py');
await mkdir(out, { recursive: true });
const temp = await mkdtemp('/tmp/cl-p35-'), home = path.join(temp, 'home'), workspace = path.join(temp, 'workspace');
await mkdir(home); await mkdir(path.join(workspace, 'src'), { recursive: true });
const beforeFile = 'const RECOMMENDED: &[&str] = &["gpt-5.6"];\n';
const afterFile = 'const RECOMMENDED: &[&str] = &["gpt-6"];\n';
await writeFile(path.join(workspace, 'src/models.rs'), beforeFile);
await writeFile(path.join(workspace, 'src/config.rs'), 'const MODELS: &str = "recommended";\n');
const caseIds = ['W1_completed_thought', 'W2_confirmed_applied_diff', 'W3_cross_round_exploration', 'W4_interactive_chat', 'W5_finalization', 'W6_cancellation', 'W7_compaction_resume'];
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const git = args => execFileSync('git', args, { cwd: source, encoding: 'utf8' }).trim();
async function identity() {
  const scripts = {};
  for (const file of [path.join(source, 'apps/leveler-desktop/scripts/conversation.mjs'), fileURLToPath(import.meta.url), providerPath]) scripts[file] = sha(await readFile(file));
  return { commit: git(['rev-parse', 'HEAD']), source_status: git(['status', '--porcelain']), binary_sha256: sha(await readFile(binary)), binary_version: execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim(), scripts };
}
const report = { binary, client, renderer: 'electron', renderer_started: false, identity: await identity(), results: [], failed: 0, home, workspace, scope: 'actual Runtime/Wire/Projection/Renderer; raw IPC is explicitly identified where Desktop exposes no command' };
const author = process.env.LEVELER_ACCEPTANCE_ALLOW_WIP === '1';
if (!author) { assert.equal(report.identity.source_status, '', 'candidate must be clean'); assert.ok(report.identity.binary_version.includes(report.identity.commit.slice(0, 12)), 'source/binary revision mismatch'); }
report.qualification = !author;
const providers = [], requests = [], wire = [], pageErrors = [];
let application, page, ownRuntime, proxy, phase = 'full', held, releaseLate;
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
  // The wire major/minor are read from the product's single source of truth:
  // a script that hardcoded them silently stopped speaking to the runtime the
  // day the protocol moved, and reported the silence as a product timeout.
  const protocol = (() => {
    const text = readFileSync(path.join(source, 'crates/leveler-client-protocol/src/version.rs'), 'utf8');
    const block = text.match(/pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion \{([\s\S]*?)\};/);
    if (!block) throw new Error('cannot read PROTOCOL_VERSION from the candidate source');
    const pick = name => Number(block[1].match(new RegExp(name + ":\\s*(\\d+)"))[1]);
    return { major: pick('major'), minor: pick('minor') };
  })();
  const wrap = body => ({ protocol, capabilities: ['optional_workspace'], body });
  async function request(socket, body) {
    return new Promise((resolve, reject) => { const stream = connect(socket); let bytes = Buffer.alloc(0); const timer = setTimeout(() => { stream.destroy(); reject(new Error('raw IPC timeout')); }, 30000);
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
async function snapshot(id) { return page.evaluate(id => window.desktop.snapshot(id), id); }
async function deliver(id, command) {
  const commandId = randomUUID();
  return page.evaluate(({ id, command, commandId }) => window.desktop.deliver({ command_id: commandId, session_id: id, issued_at: new Date().toISOString(), expected_version: null, command }), { id, command, commandId });
}
async function sendUI(content) { await page.locator('#message').fill(content); await page.locator('#send').click(); }
const liveThoughts = () => page.locator('.thought-row.thought-live').count();
async function selected() { return await page.locator('.session-row[aria-selected="true"]').count() ? page.locator('.session-row[aria-selected="true"]').first().getAttribute('data-session-id') : null; }
async function thoughts() {
  // Desktop keeps reasoning in the runtime's record but paints no settled
  // Thought row (the TUI conversation rule). The semantic assertions below
  // read the wire events instead of a DOM body that no longer exists.
  assert.equal(await page.locator('.thought-row:not(.thought-live)').count(), 0, 'a settled Thought was painted in the conversation');
  return [];
}
async function assertPresentation(id, name) {
  const bodies = await thoughts();
  let patch;
  let confirmedFile = '';
  await page.locator('.execution-group.execution-edit > .execution-summary').last().click();
  await page.waitForFunction(() => document.querySelector('.review-scope')?.textContent.includes('已确认'));
  confirmedFile = await page.locator('.review-path').innerText();
  patch = await page.locator('.review-body').allTextContents();
  await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-open'));
  assert.ok(confirmedFile.includes('src/models.rs'), 'the confirmed viewer is not on the edited file');
  for (const marker of ['gpt-5.6', 'gpt-6']) assert.ok(patch.join('').includes(marker), marker);
  const labels = await page.locator('.execution-group.execution-exploration .execution-label').allTextContents(); assert.equal(labels.length, 1); assert.ok(labels[0].includes('读取 2 次 · 搜索 2 次'));
  assert.equal((await snapshot(id)).collaboration, 'chat');
  await page.screenshot({ path: path.join(out, name + '.png'), fullPage: true });
  return { thought_bodies: bodies, receipt_labels: labels, confirmed_diff_text: patch, full_diff_visible: true };
}
async function reopen(id) {
  await page.evaluate(async id => { const index = await window.desktop.listTasks(); const task = index.tasks.find(task => task.id === id); if (!task) throw new Error('own task missing from actual index'); return window.desktop.openTask(task); }, id); await page.locator(`.session-row[aria-selected="true"][data-session-id="${id}"]`).waitFor(); await deliver(id, { type: 'query_session_history', session_id: id, query_id: randomUUID() });
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
  application = await electron.launch({ args: [path.join(source, 'apps/leveler-desktop'), `--user-data-dir=${path.join(temp, 'electron-profile')}`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: binary, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' } }); page = await application.firstWindow(); report.renderer_started = true;
  await page.waitForFunction(() => window.desktop && !!document.querySelector('#new-session'));
  await application.evaluate(({ dialog }, directory) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [directory] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
  await page.evaluate(() => { window.__presentationWire = []; window.desktop.onEvent(frame => window.__presentationWire.push(frame)); });
  page.setDefaultTimeout(20000); page.on('pageerror', error => pageErrors.push(error.message));
  let id;
  await sendUI('看下现在最新模型，同步更新配置'); id = await poll('selected own Desktop session', selected, Boolean); ownRuntime = await page.evaluate(() => window.desktop.runtimeInfo()); assert.ok(Number.isInteger(ownRuntime.pid) && ownRuntime.pid > 0); report.own_runtime = ownRuntime;
  report.session_id = id; ownSelectedSession = id;
  await poll('native final committed', () => snapshot(id), value => value.messages.some(message => message.text.includes('已同步最新模型。')) && value.task_status !== 'running');
  const live = await assertPresentation(id, 'full-execution-live'); const durableBefore = facts(id); await writeFile(path.join(out, 'durable-before-compact.json'), JSON.stringify(durableBefore, null, 2));
  const events = (await page.evaluate(() => window.__presentationWire)).filter(frame => frame.event === 'runtime').map(frame => frame.data);
  const completed = events.filter(event => event.type === 'reasoning_completed'); assert.equal(completed.length, 4); assert.deepEqual(completed.map(event => event.elapsed_ms), [0, 0, 0, 0]);
  record(caseIds[0], { wire_completed: completed, ...live });
  const patch = events.find(event => event.type === 'tool_call_completed' && event.id === 'c5'); assert.ok(patch?.applied_diff); assert.equal(await readFile(path.join(workspace, 'src/models.rs'), 'utf8'), afterFile);
  record(caseIds[1], { filesystem_before: beforeFile, filesystem_after: afterFile, matched_tool_id: 'c5', wire_applied_diff: patch.applied_diff, ...live });
  const exploration = events.filter(event => event.type === 'tool_call_started' && ['c1', 'c2', 'c3', 'c4'].includes(event.id)).map(event => ({ id: event.id, model_step: event.model_step }));
  assert.deepEqual(exploration, [{ id: 'c1', model_step: 1 }, { id: 'c2', model_step: 1 }, { id: 'c3', model_step: 2 }, { id: 'c4', model_step: 2 }]);
  record(caseIds[2], { tool_ids: exploration, ...live });
  const collaboration = (await snapshot(id)).collaboration; assert.equal(collaboration, 'chat');
  record(caseIds[3], { ui_created_collaboration: collaboration });
  assert.equal(await liveThoughts(), 0); assert.equal(await page.getByText('已同步最新模型。', { exact: true }).count(), 1);
  record(caseIds[4], { native_terminal: (await snapshot(id)).task_status, final_count: 1, live_activity_cleared: true });
  // Actually compact the model context, then replay the durable transcript.
  const beforeContext = await snapshot(id); let compactTransport;
  report.compact_raw_ipc = await rawIPC({ type: 'compact_context', session_id: id }); compactTransport = 'raw_ipc';
  const afterCompact = await poll('actual durable ContextCompacted', () => facts(id), value => value.events.some(event => /compact/i.test(event.type)));
  const compactFacts = afterCompact.events.filter(event => /compact/i.test(event.type)); assert.equal(compactFacts.length, 1);
  assert.deepEqual(JSON.parse(compactFacts[0].payload).payload, { from: durableBefore.messages.length, to: 1 });
  const compactSnapshot = await snapshot(id); assert.equal(compactSnapshot.messages.length, 1, 'model context was not compacted to one summary');
  assert.ok(compactSnapshot.messages.some(message => /摘要|压缩/.test(message.text)), 'actual compacted model context summary missing');
  await reopen(id); const replay = await assertPresentation(id, 'after-actual-compact-history-replay');
  assert.equal(facts(id).messages.length, 1, 'durable replay wrote full history back into model context');
  phase = 'continue'; await sendUI('继续'); await poll('ordinary continuation after compact', () => snapshot(id), value => value.messages.some(message => message.text.includes('CONTINUATION_AFTER_COMPACT')) && value.task_status !== 'running');
  const afterResume = facts(id); const resumePayload = { kind: afterResume.turns.at(-1)?.kind, payload: JSON.parse(afterResume.turns.at(-1)?.payload ?? '{}') };
  assert.equal(afterResume.turns.length, durableBefore.turns.length + 1, 'continuation turn not persisted');
  // The runtime never reads the user's wording as a control signal. An
  // ordinary message after compaction starts an ordinary chat turn: it names
  // its own objective and carries no continuation anchor. Continuing a goal
  // requires the model's structured declaration inside the turn.
  assert.equal(resumePayload.kind, 'chat');
  assert.equal(resumePayload.payload.continuation_root_turn_id, undefined);
  assert.equal(resumePayload.payload.objective.primary, '继续');
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
  await page.locator('#new-session').click(); cancelId = await poll('cancel session selection', selected, value => value && value !== id); await sendUI('受控取消：在真实推理流中停止');
  ownSelectedSession = cancelId;
  await poll('real held SSE reasoning arrived', liveThoughts, count => count === 1); assert.ok(held && releaseLate, 'provider gate handshake missing');
  const ack = await deliver(cancelId, { type: 'cancel_task', session_id: cancelId }); assert.equal(ack.ok, true, JSON.stringify(ack));
  cancelAckObserved = true;
  await poll('permanent native Task Cancelled', () => snapshot(cancelId), value => value.task_status === 'cancelled');
  releaseLate(); await deliver(cancelId, { type: 'query_session_history', session_id: cancelId, query_id: randomUUID() });
  await poll('native cancellation terminal rendered with no live thought', liveThoughts, count => count === 0);
  const cancelled = facts(cancelId); assert.equal((await snapshot(cancelId)).task_status, 'cancelled');
  assert.ok(!JSON.stringify(cancelled).includes('长推理结束。'), 'late provider final persisted after cancellation'); assert.equal(await page.getByText('长推理结束。', { exact: true }).count(), 0); assert.equal(await liveThoughts(), 0);
  const refusal = await deliver(cancelId, { type: 'resume_task', session_id: cancelId, content: '继续' });
  assert.equal(refusal.ok, false, 'Cancelled task reopened'); assert.equal(refusal.error?.kind, 'rejected');
  assert.ok(refusal.error.message.includes('该任务已被取消，无法继续'), 'missing Runtime cancellation refusal');
  assert.equal((await snapshot(cancelId)).task_status, 'cancelled');
  record(caseIds[5], { cancel_ack: ack, cancelled_status: 'cancelled', late_provider_payload: latePayloads, durable_late_final_absent: true, dom_late_final_absent: true, resume_refusal: refusal });
  await writeFile(path.join(out, 'durable-cancelled.json'), JSON.stringify(cancelled, null, 2));
  await page.screenshot({ path: path.join(out, 'cancelled.png'), fullPage: true });
  assert.equal(pageErrors.length, 0, pageErrors.join('\n')); if (failure) throw failure;
} catch (error) { failure = error; report.error = error.stack; if (page) await page.screenshot({ path: path.join(out, 'failure.png'), fullPage: true }).catch(() => {}); }
finally {
  if (page && !page.isClosed()) wire.push(...await page.evaluate(() => window.__presentationWire ?? []).catch(() => []));
  await writeFile(path.join(out, 'wire-frames.json'), JSON.stringify(wire, null, 2)); await writeFile(path.join(out, 'actual-model-requests.json'), JSON.stringify(requests, null, 2));
  // Cleanup is scoped to the session this consumer created and selected.
  if (failure && ownSelectedSession && page && !page.isClosed()) {
    const own = await snapshot(ownSelectedSession).catch(() => null);
    if (own?.task_status === 'running') report.failure_cleanup_stop = await deliver(ownSelectedSession, { type: 'cancel_current_turn', session_id: ownSelectedSession }).catch(error => ({ error: error.message }));
  }
  if (application) await application.close();
  const cleanup = { own_runtime: ownRuntime, desktop_normal_idle_exit_observed: false, provider_pids: providers.map(child => child.pid) };
  if (ownRuntime) { await poll('own Desktop Runtime normal idle exit', () => !alive(ownRuntime.pid), Boolean); cleanup.desktop_normal_idle_exit_observed = true; }
  releaseLate?.(); if (proxy) await new Promise(resolve => { proxy.close(resolve); proxy.closeAllConnections(); });
  for (const child of providers) { if (child.exitCode === null && child.signalCode === null) { child.kill('SIGTERM'); await poll('own provider exits', () => child.exitCode !== null || child.signalCode !== null, Boolean); } }
  cleanup.all_own_runtime_pids_gone = [ownRuntime?.pid].filter(Boolean).every(pid => !alive(pid)); cleanup.all_provider_pids_gone = providers.every(child => !alive(child.pid)); report.cleanup = cleanup; report.page_errors = pageErrors;
  for (const id of caseIds) if (!report.results.some(result => result.case === id)) report.results.push({ case: id, status: 'fail', error: failure?.message ?? 'required case was not reached' });
  report.failed = report.results.filter(result => result.status !== 'pass').length; report.identity_after = await identity(); assert.deepEqual(report.identity_after, report.identity, 'source/binary/script identity changed during run'); report.passed = !failure && report.failed === 0 && cleanup.all_own_runtime_pids_gone && cleanup.all_provider_pids_gone;
  await writeFile(path.join(out, 'conversation-evidence.json'), JSON.stringify(report, null, 2) + '\n');
}
if (failure) throw failure;
assert.equal(report.failed, 0);
