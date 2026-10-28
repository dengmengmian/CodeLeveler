// Real Desktop acceptance for the container-responsive composer.
// The same 1200px window is measured with the right pane closed and open:
// only the composer's own width changes, so a window-width media query cannot
// produce this result. A real Runtime turn supplies the Context Accounting.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-composer-narrow-');
const workspace = path.join(temporary, 'workspace');
const home = path.join(temporary, 'home');
const output = path.join(root, 'acceptance-output/desktop');
await mkdir(workspace); await mkdir(home); await mkdir(output, { recursive: true });
const requests = [];
const server = createServer(async (req, res) => {
  let raw = '';
  for await (const part of req) raw += part;
  const body = JSON.parse(raw);
  requests.push(body);
  if (!(body.tools ?? []).length) {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: 'NARROW_ACCEPTED' }, finish_reason: 'stop' }], usage: { prompt_tokens: 1200, completion_tokens: 80, total_tokens: 1280 } }));
    return;
  }
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  res.write(`data: ${JSON.stringify({ id: 'narrow', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: { content: 'NARROW_ACCEPTED' }, finish_reason: null }] })}\n\n`);
  res.write(`data: ${JSON.stringify({ id: 'narrow', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1200, completion_tokens: 80, total_tokens: 1280 } })}\n\n`);
  res.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const longModel = 'first-model-with-a-deliberately-long-selection-name';
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/${longModel}"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "http://127.0.0.1:${server.address().port}"\napi_key = "test-only"\n[models."${longModel}"]\nprovider = "fixture"\nmodel_id = "first-model"\ncontext_window = 131072\n`, { mode: 0o600 });
let app, page;
const evidence = { scope: 'real Runtime turn with a local fixture provider; same window width, right pane open vs closed', requests: 0, passed: false };
const measure = () => page.evaluate(() => {
  const box = selector => {
    const node = document.querySelector(selector);
    if (!node) return null;
    const rect = node.getBoundingClientRect();
    const style = getComputedStyle(node);
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height, right: rect.right, bottom: rect.bottom, clientWidth: node.clientWidth, scrollWidth: node.scrollWidth, display: style.display, text: node.innerText };
  };
  return {
    innerWidth,
    pageOverflow: document.documentElement.scrollWidth > innerWidth,
    main: box('.main'), card: box('#composer-card'), row: box('.composer-row'),
    tools: box('.composer-tools'), trailing: box('.composer-trailing'),
    permission: box('#permission-picker'), permissionLabel: box('#permission-picker .trigger-label'),
    model: box('#model-picker'), modelLabel: box('#model-picker .trigger-label'),
    send: box('#send'), stats: box('#context-stats'), usage: box('.usage-pill'),
    full: box('.context-full'), compact: box('.context-compact'),
    accountings: globalThis.__narrowContext?.map(entry => entry.accounting) ?? [],
    usages: globalThis.__narrowUsage ?? [],
    heights: globalThis.__narrowHeights ?? [],
  };
});
function assertNoOverflow(state, label) {
  assert.equal(state.pageOverflow, false, `page overflows horizontally (${label})`);
  assert.ok(state.card.scrollWidth <= state.card.clientWidth + 1, `composer card content overflows (${label}): ${JSON.stringify(state.card)}`);
  assert.ok(state.row.scrollWidth <= state.row.clientWidth + 1, `composer row content overflows (${label}): ${JSON.stringify(state.row)}`);
  assert.ok(state.send.right <= state.card.right + 1, `send button escapes the card (${label})`);
  assert.ok(state.permission.height <= 29, `permission trigger wrapped (${label}): ${state.permission.height}`);
  assert.ok(state.model.height <= 29, `model trigger wrapped (${label}): ${state.model.height}`);
  const sameLine = state.tools.y < state.trailing.bottom - 1 && state.trailing.y < state.tools.bottom - 1;
  if (sameLine) assert.ok(state.tools.right <= state.trailing.x + 1, `control groups overlap (${label})`);
  else assert.ok(state.trailing.y >= state.tools.bottom - 1, `wrapped groups overlap (${label})`);
}
function expectedPercent(accounting) {
  const used = accounting?.used_tokens;
  const total = [accounting.input_capacity_tokens, accounting.context_window_tokens].find(value => Number.isFinite(value) && value > 0);
  return Number.isFinite(used) && total !== undefined ? Math.min(100, Math.floor(used * 100 / total)) : null;
}
try {
  app = await electron.launch({ args: [root, `--user-data-dir=${temporary}/electron`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' } });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  const pageErrors = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const id = await page.evaluate(async () => { const folder = await window.desktop.chooseFolder(); return (await window.desktop.createTask(folder.id, crypto.randomUUID())).session.id; });
  await page.reload();
  await page.locator(`.session-row[data-session-id="${id}"]`).click();
  await page.evaluate(() => {
    globalThis.__narrowContext = []; globalThis.__narrowUsage = []; globalThis.__narrowHeights = [];
    window.desktop.onEvent(frame => {
      const type = frame.data?.type;
      if (type === 'context_usage' || type === 'context_loaded') {
        if (frame.data.accounting) globalThis.__narrowContext.push({ accounting: frame.data.accounting, type });
        globalThis.__narrowHeights.push(document.querySelector('#composer-card')?.getBoundingClientRect().height ?? null);
      }
      if (type === 'token_usage') {
        globalThis.__narrowUsage.push(frame.data);
        globalThis.__narrowHeights.push(document.querySelector('#composer-card')?.getBoundingClientRect().height ?? null);
      }
    });
  });
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await page.locator('#message').fill('narrow composer acceptance');
  await page.waitForFunction(() => !document.querySelector('#send')?.disabled, null, { timeout: 15000 }).catch(async error => {
    throw new Error(`send stayed disabled: ${await page.evaluate(() => ({ status: document.querySelector('#conversation-status')?.textContent, error: document.querySelector('#composer-error')?.textContent, value: document.querySelector('#message')?.value }))}`, { cause: error });
  });
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('NARROW_ACCEPTED') && ['已回答', '已完成'].includes(document.querySelector('#conversation-status')?.textContent), null, { timeout: 60000 });
  await page.waitForFunction(() => (globalThis.__narrowContext?.length ?? 0) > 0 && !document.querySelector('#context-stats')?.hidden, null, { timeout: 20000 });
  if (await page.evaluate(() => document.body.classList.contains('sidebar-collapsed'))) await page.locator('#sidebar-toggle').click();
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1200, 860));
  await page.waitForFunction(() => innerWidth === 1200);
  await page.waitForTimeout(250);
  // Same window: right pane closed.
  const wide = await measure();
  assert.ok(wide.card.width > 576, `the wide composer must exceed the 576px container breakpoint: ${wide.card.width}`);
  assert.notEqual(wide.permissionLabel.display, 'none', 'wide composer keeps the permission mode name');
  assert.ok(wide.permissionLabel.height <= 21, `wide permission label must stay on one line: ${wide.permissionLabel.height}`);
  assert.ok(wide.usage.width > 0, 'wide composer keeps the readable usage summary');
  assert.ok(wide.full.width > 0 && wide.compact.width === 0, 'wide composer shows the full context reading');
  assert.ok(wide.modelLabel.scrollWidth <= wide.modelLabel.clientWidth + 1, 'wide composer shows the full model name');
  assertNoOverflow(wide, 'wide');
  // Same window, right pane open: only the composer container narrows.
  await page.locator('#header-right-toggle:visible').click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-open'));
  await page.waitForFunction(() => document.querySelector('.main')?.clientWidth <= 400, null, { timeout: 10000 });
  await page.waitForTimeout(250);
  const narrow = await measure();
  assert.equal(narrow.innerWidth, wide.innerWidth, 'the window width must not change between the two layouts');
  assert.ok(narrow.main.width <= 400, `the right pane must compress the conversation: ${narrow.main.width}`);
  assert.ok(narrow.card.width < 576, `the composer must be in its narrow container mode: ${narrow.card.width}`);
  assert.equal(narrow.permissionLabel.display, 'none', 'narrow composer collapses the permission mode to its icon');
  assert.ok(narrow.permission.width <= 48, `permission trigger must be icon-only: ${narrow.permission.width}`);
  assert.match(await page.getAttribute('#permission-picker', 'data-tooltip'), /工作区内修改/, 'the collapsed permission trigger keeps its mode in the tooltip');
  assert.match(await page.getAttribute('#permission-picker', 'aria-label'), /工作区内修改/, 'the collapsed permission trigger keeps its mode in the accessible name');
  assert.ok(narrow.model.width <= 132.5, `model trigger must enter its compact width: ${narrow.model.width}`);
  assert.ok(narrow.modelLabel.scrollWidth > narrow.modelLabel.clientWidth, 'a long model name must truncate, not wrap');
  assert.equal(await page.getAttribute('#model-picker', 'aria-label'), `选择模型，当前：${longModel}`, 'the full selection stays in the accessible name');
  assert.match(await page.getAttribute('#model-picker', 'data-tooltip'), new RegExp(longModel), 'the full selection stays in the tooltip');
  assert.ok(narrow.send.width >= 34, 'send stays visible and full size');
  assert.equal(narrow.usage.display, 'none', 'narrow composer hides the cumulative usage pill');
  assert.ok(narrow.compact.width > 0 && narrow.full.width === 0, 'narrow composer shows the compact reading');
  assert.equal(narrow.stats.height, 28, 'context stats keep a stable 28px row');
  const accounting = narrow.accountings.at(-1);
  const percent = expectedPercent(accounting);
  assert.notEqual(percent, null, 'the Runtime must have reported Context Accounting');
  assert.equal(narrow.compact.text.trim(), `${percent}%`, 'the compact reading must come from Context Accounting');
  // The compact reading must not be the cumulative usage ratio: the fixture's
  // prompt_tokens are small while the real request is large.
  const usage = narrow.usages.at(-1);
  evidence.context = { accounting, cumulative_usage: usage ?? null, percent, compact_text: narrow.compact.text.trim() };
  assertNoOverflow(narrow, 'narrow');
  // A statistics refresh must not resize the composer. Ask the Runtime for a
  // fresh accounting snapshot and compare the card's box before and after.
  const heightBefore = await page.evaluate(() => document.querySelector('#composer-card').getBoundingClientRect().height);
  const contextCount = await page.evaluate(() => globalThis.__narrowContext.length);
  const delivered = await page.evaluate(sessionId => window.desktop.deliver({ command_id: crypto.randomUUID(), session_id: sessionId, expected_version: null, issued_at: new Date().toISOString(), command: { type: 'query_context', session_id: sessionId, query_id: crypto.randomUUID() } }), id);
  assert.equal(delivered?.ok, true, `context refresh was not delivered: ${JSON.stringify(delivered)}`);
  await page.waitForFunction(count => (globalThis.__narrowContext?.length ?? 0) > count, contextCount, { timeout: 10000 });
  await page.waitForTimeout(100);
  const heightAfter = await page.evaluate(() => document.querySelector('#composer-card').getBoundingClientRect().height);
  assert.equal(heightAfter, heightBefore, `composer height changed while statistics updated: ${heightBefore} -> ${heightAfter}`);
  evidence.statistics_refresh = { height_before: heightBefore, height_after: heightAfter };
  // The detail panel keeps every number the compact pill dropped.
  await page.locator('#context-stats .context-pill').first().click();
  await page.waitForFunction(() => !!document.querySelector('.menu'));
  const menu = await page.locator('.menu').innerText();
  assert.match(menu, new RegExp(`上下文占用：${percent}%`));
  assert.match(menu, /有效输入容量|模型窗口/);
  assert.match(menu, /已用输入/);
  assert.match(menu, /Token 用量/);
  assert.match(menu, /输入：/);
  evidence.detail_menu = menu;
  await page.keyboard.press('Escape');
  // Back to the same window with the pane closed: the full layout returns.
  await page.locator('#header-right-toggle:visible').click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-open'));
  await page.waitForTimeout(250);
  const restored = await measure();
  assert.equal(restored.innerWidth, wide.innerWidth);
  assert.ok(restored.card.width > 576);
  assert.notEqual(restored.permissionLabel.display, 'none');
  assert.ok(restored.usage.width > 0);
  assertNoOverflow(restored, 'restored');
  Object.assign(evidence, { window_width: wide.innerWidth, wide, narrow, restored, page_errors: pageErrors, provider_requests: requests.length, passed: pageErrors.length === 0 });
  assert.deepEqual(pageErrors, []);
} catch (error) {
  evidence.error = error.message;
  if (page && !page.isClosed()) {
    evidence.last = await measure().catch(() => null);
    await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')).then(bytes => writeFile(path.join(output, 'composer-narrow-failure.png'), Buffer.from(bytes, 'base64'))).catch(() => {});
  }
  throw error;
} finally {
  await writeFile(path.join(output, 'composer-narrow-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify({ passed: evidence.passed, context: evidence.context, wide: evidence.wide && { main: evidence.wide.main.width, card: evidence.wide.card.width, permission_label: evidence.wide.permissionLabel.display, usage: evidence.wide.usage.display }, narrow: evidence.narrow && { main: evidence.narrow.main.width, card: evidence.narrow.card.width, permission_label: evidence.narrow.permissionLabel.display, model: evidence.narrow.model.width, compact: evidence.narrow.compact.text, heights: evidence.narrow.heights }, restored: evidence.restored && { card: evidence.restored.card.width, label: evidence.restored.permissionLabel.display }, error: evidence.error }, null, 2));
  if (app) await app.close();
  await new Promise(resolve => server.close(resolve));
}
