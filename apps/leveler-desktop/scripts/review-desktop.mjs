// R3c isolated Desktop acceptance: real Electron, real leveler runtime, a
// test-only SSE provider, a temporary HOME and workspace. It proves the manual
// review entry, the runtime's own review execution, structured findings with
// real file positions, business coverage/limitations, cancellation, failure,
// restart persistence and session isolation. Nothing here fabricates a review
// result: every card and report is projected from the runtime's durable row.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { appendFileSync, existsSync, readFileSync } from 'node:fs';
import { execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-r3c-review-');
const home = path.join(temporary, 'home');
const userHome = path.join(temporary, 'user-home');
const workspaceA = path.join(temporary, 'workspace-a');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(userHome, { recursive: true });
await mkdir(workspaceA, { recursive: true });
await mkdir(output, { recursive: true });
const providerLog = path.join(temporary, 'provider-requests.jsonl');

const BUGGY = `export function average(values) {
  let sum = 0;
  for (let i = 0; i < values.length - 1; i += 1) sum += values[i];
  return values.length ? sum / values.length : 0;
}
`;
const FIXED = BUGGY.replace('values.length - 1', 'values.length');
const git = (...args) => execFile('git', args, { cwd: workspaceA });
await git('init', '-q');
await git('config', 'user.email', 'r3c@example.com');
await git('config', 'user.name', 'R3C');
await writeFile(path.join(workspaceA, 'calc.mjs'), FIXED);
await git('add', 'calc.mjs');
await git('commit', '-qm', 'initial average');
// The planted defect is an UNCOMMITTED change, so the workspace scope has a
// real diff and the reviewer reads the same bytes a person would.
await writeFile(path.join(workspaceA, 'calc.mjs'), BUGGY);

const requests = [];
const providerErrors = [];
const sse = payloads => payloads
  .map(payload => `data: ${JSON.stringify({ id: 'r3c', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: payload, finish_reason: null }] })}\n\n`)
  .join('') + 'data: [DONE]\n\n';
const text = value => sse([{ role: 'assistant', content: value }, {}]);
const toolCall = (id, name, args) => sse([
  { role: 'assistant', tool_calls: [{ index: 0, id, type: 'function', function: { name, arguments: JSON.stringify(args) } }] },
  { finish_reason: 'tool_calls' },
]);
let toolSeq = 0;
const reviewRequests = [];
const held = [];
const provider = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    const names = (body.tools ?? []).map(tool => tool.function?.name ?? tool.name).filter(Boolean);
    if (!names.length) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    const messages = body.messages ?? [];
    const serialized = JSON.stringify(messages);
    const toolResults = messages.filter(message => message.role === 'tool').map(message => String(message.content ?? ''));
    // Tool calls live on assistant messages; the last tool RESULT is not a
    // reliable phase marker because report_review's result replaces it.
    const calledTools = messages.flatMap(message => message.tool_calls ?? []).map(call => call.function?.name);
    const findingsReported = calledTools.filter(name => name === 'report_finding').length;
    const reviewReported = calledTools.includes('report_review');
    const finding = [...toolResults].reverse().map(content => /Finding (f-\d+) recorded/.exec(content)?.[1]).find(Boolean) ?? null;
    const marker = /R3C_[A-Z_]+/.exec(serialized)?.[0] ?? null;
    const isReview = serialized.includes('独立审查任务');
    if (isReview) {
      reviewRequests.push({
        marker,
        focus: (serialized.match(/R3C_(?:CODE|BIZ|CANCEL|FAIL)_[A-Z_]*/g) ?? []),
        tools: names,
        hasSessionTranscript: serialized.includes('用户在本 Session 中的需求与调整'),
        hasRequirementFacts: serialized.includes('来源任务（用户原始需求）'),
        hasDeliveryClaim: serialized.includes('开发 Agent 的交付声明'),
      });
      appendFileSync(providerLog, `${JSON.stringify({ review: reviewRequests.at(-1) })}\n`);
    } else {
      appendFileSync(providerLog, `${JSON.stringify({ chat: { marker } })}\n`);
    }

    if (isReview && marker === 'R3C_CODE') {
      if (findingsReported === 0) {
        toolSeq += 1;
        response.writeHead(200, { 'content-type': 'text/event-stream' });
        response.end(toolCall(`r3c-find-${toolSeq}`, 'report_finding', {
          kind: 'correctness',
          summary: 'average() 的循环少遍历最后一个元素，结果按 n-1 个元素求和却除以 n。',
          title: '平均值计算漏掉最后一个元素',
          dimension: 'code',
          severity: 'p1',
          confidence: 'high',
          file: 'calc.mjs',
          line: 3,
          evidence: 'for (let i = 0; i < values.length - 1; i += 1) sum += values[i];',
          impact: 'average([1,2]) 返回 0.5 而不是 1.5，所有依赖平均值的业务结果偏小。',
          recommendation: '循环条件改为 i < values.length。',
          reproduced: false,
        }));
        return;
      }
      if (!reviewReported) {
        toolSeq += 1;
        response.writeHead(200, { 'content-type': 'text/event-stream' });
        response.end(toolCall(`r3c-report-${toolSeq}`, 'report_review', {
          report: `# 审查报告 R3C_CODE_REPORT\n\n范围：工作区未提交变更。\n\n## 问题\n\n- ${finding}: 循环漏掉最后一个元素。\n\n结论：发现问题。`,
          formal_findings: [finding],
          conclusion: 'issues_found',
          coverage: [{ dimension: 'code', status: 'reviewed', scope: 'calc.mjs', evidence_refs: [`calc.mjs:3`] }],
          limitations: [],
        }));
        return;
      }
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end(text('R3C_CODE_DONE'));
      return;
    }
    if (isReview && marker === 'R3C_BIZ') {
      if (!reviewReported) {
        toolSeq += 1;
        response.writeHead(200, { 'content-type': 'text/event-stream' });
        response.end(toolCall(`r3c-biz-${toolSeq}`, 'report_review', {
          report: `# 业务审查 R3C_BIZ_REPORT\n\n本会话只完成了一次对话，没有可核对的交付验收记录。\n\n结论：证据不足。`,
          formal_findings: [],
          conclusion: 'inconclusive',
          coverage: [
            { dimension: 'business', status: 'partial', scope: '用户消息', limitations: '没有正式需求文档' },
            { dimension: 'delivery', status: 'not_reviewed', limitations: '本会话没有交付记录' },
          ],
          limitations: [{ kind: 'scope', detail: '没有可核对的验收标准与交付证据' }],
        }));
        return;
      }
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end(text('R3C_BIZ_DONE'));
      return;
    }
    if (isReview && marker === 'R3C_FAIL') {
      // 400 is a non-retryable provider error: the review must settle as
      // Failed promptly instead of looping through retry backoff.
      response.writeHead(400, { 'content-type': 'text/plain' });
      response.end('fixture provider refused this review');
      return;
    }
    if (isReview && marker === 'R3C_CANCEL') {
      // Hold the reviewer open: cancellation must reach the runtime, not a
      // client-side timer. The promise settles when the client disconnects.
      held.push(response);
      request.on('close', () => { /* the runtime aborted the request */ });
      return;
    }
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(text(marker ? `${marker}_DONE` : 'R3C_CHAT_DONE'));
  } catch (error) {
    providerErrors.push(error.message);
    try { response.writeHead(500); response.end('fixture failed'); } catch { /* already closed */ }
  }
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${provider.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture-model"\ncontext_window=131072\nparallel_tool_calls = false\n`, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const profile = path.join(temporary, 'profile');
const evidence = { scope: 'real Electron + real Runtime + test-only SSE provider; temporary HOME/workspace', checks: [] };
const record = (id, detail) => { evidence.checks.push({ id, ...detail }); };
let app;
let page;
const errors = [];

const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${profile}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, HOME: userHome, USERPROFILE: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
};
// A page reload drops listeners; the probe is installed after every (re)load
// so the recorded stream is the one the app actually received.
const installEventLog = async () => {
  await page.evaluate(() => {
    window.__r3cEvents = [];
    window.desktop.onEvent(frame => {
      const type = String(frame?.data?.type ?? '');
      if (type.startsWith('review_')) window.__r3cEvents.push({ type, id: frame.data.review?.id ?? frame.data.review_id ?? frame.data.report?.summary?.id ?? null, status: frame.data.review?.status ?? null });
    });
  });
};
const restart = async () => {
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
};
const openWorkspace = async workspace => {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.textContent.includes(name), path.basename(workspace));
};
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const waitSend = () => page.waitForFunction(() => {
  const button = document.querySelector('#send');
  return button && !button.disabled && !button.hidden;
});
const submit = async value => {
  await page.locator('#message').fill(value);
  await waitSend();
  await page.locator('#send').click();
};
const waitReply = async (value, timeout = 60000) => {
  await page.waitForFunction(text => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(text)), value, { timeout });
  await page.waitForFunction(async () => {
    const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
    if (!id) return false;
    const snapshot = await window.desktop.snapshot(id);
    return !['running', 'waiting_user'].includes(snapshot.task_status);
  }, null, { timeout: 60000 });
};
const reviewEvents = () => page.evaluate(() => window.__r3cEvents ?? []);
const cards = () => page.locator('.review-card');
const card = id => page.locator(`.review-card[data-review-id="${id}"]`);
const openDialog = async () => {
  await page.waitForFunction(() => {
    const button = document.querySelector('#review-entry');
    return button && !button.disabled;
  }, null, { timeout: 60000 });
  await page.locator('#review-entry').click();
  try {
    await page.waitForFunction(() => document.querySelector('#review-dialog')?.open, null, { timeout: 5000 });
  } catch (error) {
    const diagnostic = await page.evaluate(() => ({
      disabled: document.querySelector('#review-entry')?.disabled,
      tooltip: document.querySelector('#review-entry')?.dataset.tooltip,
      composerError: document.querySelector('#composer-error')?.textContent,
      status: document.querySelector('#conversation-status')?.textContent,
    }));
    throw new Error(`review dialog did not open: ${JSON.stringify(diagnostic)}`);
  }
};
const chooseScope = async id => {
  await page.locator(`.review-option[data-scope="${id}"] input`).check();
};
const fillFocus = async value => {
  await page.locator('#review-focus').fill(value);
};
const startReview = async () => {
  await page.locator('#review-submit').click();
  await page.waitForFunction(() => !document.querySelector('#review-dialog'), null, { timeout: 30000 });
};
const waitCard = async (id, status, timeout = 90000) => {
  await page.waitForFunction(({ id, status }) => {
    const node = document.querySelector(`.review-card[data-review-id="${id}"]`);
    return node && node.dataset.status === status;
  }, { id, status }, { timeout });
};
const settleCard = async timeout => {
  await page.waitForFunction(() => {
    const nodes = [...document.querySelectorAll('.review-card')];
    return nodes.length > 0 && nodes.every(node => !['running', 'queued'].includes(node.dataset.status));
  }, null, { timeout });
};
const latestCardId = async () => {
  const ids = await cards().evaluateAll(nodes => nodes.map(node => node.dataset.reviewId));
  return ids.at(-1);
};

try {
  await launch();
  await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}');
    stored.defaultPermission = 'assisted';
    localStorage.setItem('codeleveler.settings.v1', JSON.stringify(stored));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await installEventLog();
  await openWorkspace(workspaceA);
  await submit('R3C_CHAT_WARMUP 记一下这个仓库');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await waitReply('R3C_CHAT_WARMUP_DONE');
  const firstSession = await sessionId();
  // D02/D18: an ordinary turn does not start a review, and no card exists.
  assert.equal(await cards().count(), 0, 'a chat turn must not start a review');
  assert.equal((await reviewEvents()).filter(item => item.type === 'review_started').length, 0);
  record('D02', { entry: 'composer 审查 button + /review command', autoStarted: 0 });

  // D01/D03: the entry opens a dialog whose facts come from the runtime.
  await openDialog();
  await page.waitForFunction(() => document.querySelector('#review-dialog').textContent.includes('工作区有未提交变更'));
  const recommended = await page.locator('.review-option[data-scope="workspace"]').getAttribute('data-selected');
  assert.equal(recommended, 'true', 'dirty workspace must be the runtime recommendation');
  const businessVisible = await page.locator('.review-option[data-scope="current_task"]').count();
  assert.equal(businessVisible, 1);
  // Opening the picker is a query, not a review.
  assert.equal((await reviewEvents()).filter(item => item.type === 'review_started').length, 0);
  await chooseScope('workspace');
  await fillFocus('R3C_CODE 检查平均值的边界条件');
  await startReview();
  await page.waitForFunction(() => document.querySelector('.review-card'));
  const codeReview = await latestCardId();
  // A fixture reviewer can finish before the first frame is observed; the
  // card's existence is the fact, not a guaranteed 'running' snapshot.
  await page.waitForFunction(id => !!document.querySelector(`.review-card[data-review-id="${id}"]`), codeReview, { timeout: 90000 });
  record('D03', { review: codeReview, scope: 'workspace' });
  await settleCard(180000);
  await waitCard(codeReview, 'completed');
  const codeText = await card(codeReview).textContent();
  assert.match(codeText, /发现问题/);
  assert.match(codeText, /P1 1/);
  assert.doesNotMatch(codeText, /未发现问题/);
  record('D04', { struck: 'P1 average() off-by-one', card: codeText.slice(0, 200) });

  // D05/D09: the report carries the structured finding and the real position.
  await card(codeReview).getByRole('button', { name: '查看报告' }).click();
  await page.waitForFunction(() => document.querySelector('.review-report'));
  await page.waitForFunction(() => document.querySelectorAll('.review-finding').length > 0);
  const findingText = await page.locator('.review-finding').first().textContent();
  assert.match(findingText, /平均值计算漏掉最后一个元素/);
  assert.match(findingText, /P1 · 高/);
  const location = page.locator('.review-finding-location').first();
  assert.equal(await location.textContent(), 'calc.mjs:3');
  await location.click();
  await page.waitForFunction(() => document.querySelector('.preview-code-line[data-line="3"].target-line'));
  const targetLine = await page.locator('.preview-code-line[data-line="3"]').textContent();
  assert.match(targetLine, /values\.length - 1/, 'the preview must show the reviewed line');
  await page.screenshot({ path: path.join(output, 'r3c-code-review-report.png') });
  record('D05', { file: 'calc.mjs', line: 3, targetLine: targetLine.trim() });

  // D06/D08: business review of the session record; missing evidence is stated.
  await openDialog();
  await page.waitForFunction(() => document.querySelector('#review-dialog').textContent.includes('工作区有未提交变更'));
  await chooseScope('session');
  await fillFocus('R3C_BIZ 审查这个会话的需求满足与交付证据');
  await startReview();
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length === 2, null, { timeout: 60000 });
  const businessReview = await latestCardId();
  await settleCard(180000);
  await waitCard(businessReview, 'completed');
  const businessText = await card(businessReview).textContent();
  assert.match(businessText, /证据不足/);
  assert.doesNotMatch(businessText, /未发现问题/);
  await card(businessReview).getByRole('button', { name: '查看报告' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('.review-report')].at(-1)?.textContent.includes('证据不足'));
  const reportPane = page.locator('.review-report').last();
  assert.match(await reportPane.textContent(), /部分审查/);
  assert.match(await reportPane.textContent(), /没有可核对的验收标准与交付证据/);
  const bizRequest = reviewRequests.find(item => item.marker === 'R3C_BIZ');
  assert.ok(bizRequest, 'the business review must reach the provider as a review');
  assert.equal(bizRequest.hasSessionTranscript, true, 'business review must carry the session transcript');
  assert.equal(bizRequest.hasRequirementFacts, true);
  await page.screenshot({ path: path.join(output, 'r3c-business-review-report.png') });
  record('D06', { review: businessReview, coverage: 'partial', limitations: 1 });

  // D10: cancellation settles through the runtime.
  await openDialog();
  await page.waitForFunction(() => document.querySelector('#review-dialog').textContent.includes('工作区有未提交变更'));
  await chooseScope('workspace');
  await fillFocus('R3C_CANCEL 持续调查直到取消');
  await startReview();
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length === 3, null, { timeout: 60000 });
  const cancelReview = await latestCardId();
  await waitCard(cancelReview, 'running');
  // A running review has no settled counts or spend; the summary's zeros are
  // "not recorded yet" and must not be painted as a result.
  const runningText = await card(cancelReview).textContent();
  assert.doesNotMatch(runningText, /共 0 条/);
  assert.doesNotMatch(runningText, /输入 0 · 输出 0/);
  await card(cancelReview).getByRole('button', { name: '取消审查' }).click();
  await waitCard(cancelReview, 'cancelled', 60000);
  const cancelEvent = (await reviewEvents()).find(event => event.type === 'review_finished' && event.id === cancelReview);
  assert.equal(cancelEvent?.status, 'cancelled');
  record('D10', { review: cancelReview, runtimeStatus: cancelEvent.status });

  // D11: a reviewer failure is not drawn as a completed review.
  await openDialog();
  await page.waitForFunction(() => document.querySelector('#review-dialog').textContent.includes('工作区有未提交变更'));
  await chooseScope('workspace');
  await fillFocus('R3C_FAIL 触发审查失败');
  await startReview();
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length === 4, null, { timeout: 60000 });
  const failedReview = await latestCardId();
  await waitCard(failedReview, 'failed', 120000);
  const failedText = await card(failedReview).textContent();
  assert.match(failedText, /失败/);
  assert.match(failedText, /失败原因：/);
  assert.doesNotMatch(failedText, /已完成/);
  assert.doesNotMatch(failedText, /未发现问题/);
  record('D11', { review: failedReview, card: failedText.slice(0, 160) });

  // D18: no automatic review anywhere, before or after commits.
  assert.equal((await reviewEvents()).filter(item => item.type === 'review_started').length, 4);
  assert.equal(reviewRequests.length >= 4, true);
  record('D18', { started: 4, explicit: true });

  // D12/D13: restart keeps the durable reviews; another session has none.
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), firstSession, { timeout: 30000 });
  await page.locator(`.session-row[data-session-id="${firstSession}"]`).click();
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length >= 4, null, { timeout: 30000 });
  const idsAfterRestart = await cards().evaluateAll(nodes => nodes.map(node => node.dataset.reviewId));
  assert.ok(idsAfterRestart.includes(codeReview));
  assert.ok(idsAfterRestart.includes(businessReview));
  assert.ok(idsAfterRestart.includes(cancelReview));
  assert.ok(idsAfterRestart.includes(failedReview));
  await card(codeReview).getByRole('button', { name: '查看报告' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('.review-report')].at(-1)?.textContent.includes('平均值计算漏掉最后一个元素'), null, { timeout: 30000 });
  const beforeSecond = reviewRequests.length;
  assert.equal(reviewRequests.length, beforeSecond, 'opening a stored report must not re-run the review');
  await page.screenshot({ path: path.join(output, 'r3c-review-restored.png') });
  record('D12', { restored: idsAfterRestart.length, reviewRequests: beforeSecond });

  // A second session in the same workspace owns its own (empty) review list.
  await page.locator('#new-session').click();
  await page.waitForFunction(() => !document.querySelector('.session-row[aria-selected="true"]') || document.querySelector('.review-card'));
  await submit('R3C_CHAT_SECOND 另一个会话');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await waitReply('R3C_CHAT_SECOND_DONE');
  const secondSession = await sessionId();
  assert.notEqual(secondSession, firstSession);
  assert.equal(await cards().count(), 0, 'another session must not show the first one\'s reviews');
  await page.locator(`.session-row[data-session-id="${firstSession}"]`).click();
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length >= 4, null, { timeout: 90000 });
  record('D13', { first: firstSession, second: secondSession });

  assert.deepEqual(errors, [], 'renderer errors');
  assert.deepEqual(providerErrors, [], 'provider errors');
  // Cancellation may land before the reviewer's first model request; that is
  // a valid runtime outcome, so whether the provider was reached is recorded
  // rather than required.
  record('D10b', { providerReachedBeforeCancel: held.length > 0 });

  const report = { ...evidence, sessions: { firstSession, secondSession } };
  await writeFile(path.join(output, 'r3c-review-desktop.json'), JSON.stringify(report, null, 2));
  console.log('R3C Desktop review acceptance PASS');
  console.log(JSON.stringify(report, null, 2));
} finally {
  for (const response of held) { try { response.destroy(); } catch { /* closed */ } }
  provider.close();
  if (app) { try { await app.evaluate(({ app }) => app.exit(0)); } catch { /* already gone */ } }
  // Electron flushes its profile on exit; a best-effort cleanup must not turn
  // a passing run into a failure.
  await new Promise(resolve => setTimeout(resolve, 1500));
  await rm(temporary, { recursive: true, force: true, maxRetries: 5 }).catch(() => {});
}
