// R4 real-Agent file workflow: create, modify, rename, delete, directory,
// image creation and refresh. Real Electron/bridge/runtime/workspace/Git; the
// model is a scripted local SSE provider issuing real tool calls.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { deflateSync } from 'node:zlib';
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-files-agent-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });

// A tiny valid PNG encoder so dimensions are a real decoded fact, not a tile.
const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
function crc32(buffer) {
  let c = 0xffffffff;
  for (const byte of buffer) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const length = Buffer.alloc(4); length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}
function png(width, height, [r, g, b]) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0); ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
  const rows = [];
  for (let y = 0; y < height; y += 1) {
    const row = Buffer.alloc(1 + width * 4);
    for (let x = 0; x < width; x += 1) {
      row[1 + x * 4] = r; row[2 + x * 4] = g; row[3 + x * 4] = b; row[4 + x * 4] = 255;
    }
    rows.push(row);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(Buffer.concat(rows))),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}
await writeFile(path.join(workspace, 'existing.txt'), 'original\n');
await writeFile(path.join(workspace, 'delete-me.txt'), 'delete me\n');
await writeFile(path.join(workspace, 'base.png'), png(4, 2, [220, 40, 40]));
await writeFile(path.join(workspace, 'base2.png'), png(3, 5, [40, 80, 220]));
await writeFile(path.join(workspace, 'large.png'), png(2400, 1600, [30, 140, 90]));
execFileSync('git', ['init', '-q'], { cwd: workspace });
execFileSync('git', ['add', '.'], { cwd: workspace });
execFileSync('git', ['-c', 'user.email=fixture@example.test', '-c', 'user.name=Fixture', 'commit', '-qm', 'baseline'], { cwd: workspace });

const requests = [];
const providerErrors = [];
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  let body = {};
  try { body = JSON.parse(raw); } catch { /* probes */ }
  const messages = body.messages ?? [];
  const textOf = message => typeof message?.content === 'string' ? message.content : JSON.stringify(message?.content ?? '');
  const userTexts = messages.filter(message => message.role === 'user').map(textOf);
  const last = userTexts.at(-1) ?? '';
  requests.push({ last });
  const tools = (body.tools ?? []).map(tool => tool.function?.name);
  if (!tools.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const stream = (delta, finish) => `data: ${JSON.stringify({ id: 'files', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  const text = value => { response.write(stream({ content: value }, null)); response.write(stream({}, 'stop')); response.end('data: [DONE]\n\n'); };
  const call = (name, args) => { response.write(stream({ role: 'assistant', tool_calls: [{ index: 0, id: `f-${Date.now()}-${Math.random()}`, type: 'function', function: { name, arguments: JSON.stringify(args) } }] }, null)); response.write(stream({}, 'tool_calls')); response.end('data: [DONE]\n\n'); };
  // Count tool results belonging to the current request only.
  const lastUserIndex = messages.findLastIndex(message => message.role === 'user');
  const toolTurns = messages.slice(lastUserIndex + 1).filter(message => message.role === 'tool').length;
  if (last.includes('SEED')) { text('SEED_OK'); return; }
  if (last.includes('IMAGE_SWAP')) {
    if (toolTurns === 0) call('run_command', { program: '/bin/sh', args: ['-c', 'cp base2.png agent.png'] });
    else text('IMAGE_SWAP_DONE');
    return;
  }
  if (last.includes('FILE_OPS')) {
    // write_file refuses to overwrite a file this window never read; that is
    // a real product safety rule, so the fixture reads before it edits.
    if (toolTurns === 0) call('read_file', { path: 'existing.txt' });
    else if (toolTurns === 1) call('write_file', { path: 'existing.txt', content: 'modified\n' });
    else if (toolTurns === 2) call('write_file', { path: 'created.txt', content: 'fresh\n' });
    else if (toolTurns === 3) call('run_command', { program: '/bin/sh', args: ['-c', 'mkdir -p newdir && mv created.txt newdir/renamed.txt'] });
    else if (toolTurns === 4) call('run_command', { program: '/bin/sh', args: ['-c', 'rm delete-me.txt'] });
    else if (toolTurns === 5) call('run_command', { program: '/bin/sh', args: ['-c', 'cp base.png agent.png'] });
    else text('FILE_OPS_DONE');
    return;
  }
  text('PLAIN_OK');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });

const evidence = { scope: 'real Agent tool calls + real Desktop file tree/preview/Git diff' };
const errors = [];
let app;
let page;
const send = async value => { await page.locator('#message').fill(value); await page.locator('#send').click(); };
const waitAnswer = async value => page.waitForFunction(text => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(text)), value, { timeout: 120000 });
const showFiles = async () => {
  const tab = page.locator('.pane-tab').filter({ hasText: /^文件$/ }).first();
  if (await tab.count()) await tab.click();
};
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}');
    stored.defaultPermission = 'full_access';
    localStorage.setItem('codeleveler.settings.v1', JSON.stringify(stored));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await send('SEED files session');
  await waitAnswer('SEED_OK');

  // Open the tree and a text/image preview before the Agent changes anything.
  await page.keyboard.press('Meta+P');
  await page.waitForSelector('.files-row[data-path="existing.txt"]');
  await page.locator('.files-row[data-path="existing.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('original'));
  await showFiles();
  await page.locator('.files-row[data-path="base.png"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-image')?.naturalWidth === 4);
  assert.deepEqual(await page.locator('.preview-image').evaluate(image => [image.naturalWidth, image.naturalHeight]), [4, 2]);
  await showFiles();
  await page.locator('.files-row[data-path="delete-me.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('delete me'));
  await page.locator('.pane-tab').filter({ hasText: 'existing.txt' }).first().click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('original'));

  // One real turn performs create + modify + directory + rename + delete,
  // then copies a real PNG into the workspace.
  await send('FILE_OPS 执行文件操作');
  await waitAnswer('FILE_OPS_DONE');
  await showFiles();
  await page.waitForFunction(() => !document.querySelector('.files-row[data-path="delete-me.txt"]') && document.querySelector('.files-row[data-path="newdir"]') && document.querySelector('.files-row[data-path="existing.txt"]'), null, { timeout: 30000 });
  const tree = await page.evaluate(() => [...document.querySelectorAll('.files-row')].map(row => row.dataset.path));
  assert.ok(tree.includes('newdir'), JSON.stringify(tree));
  assert.ok(!tree.includes('delete-me.txt'), 'the deleted file must leave the tree');
  // The modified file's open preview must show the new bytes automatically.
  await page.locator('.pane-tab').filter({ hasText: 'existing.txt' }).first().click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('modified'), null, { timeout: 30000 });
  // The open preview of the deleted file must show the real failure, not the
  // old bytes.
  await page.locator('.pane-tab').filter({ hasText: 'delete-me.txt' }).first().click();
  await page.waitForFunction(() => /不存在|无法读取/.test(document.querySelector('.preview-status')?.textContent ?? document.querySelector('.preview-body')?.textContent ?? ''), null, { timeout: 30000 });
  const deletedPreview = await page.locator('.preview-status, .preview-body').first().innerText();
  assert.doesNotMatch(deletedPreview, /delete me/);
  await showFiles();
  await page.locator('.files-row[data-path="newdir"]').click();
  await page.waitForSelector('.files-row[data-path="newdir/renamed.txt"]');
  await page.locator('.files-row[data-path="newdir/renamed.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('fresh'));
  await showFiles();
  await page.locator('.files-row[data-path="agent.png"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-image')?.naturalWidth === 4, null, { timeout: 30000 });
  assert.deepEqual(await page.locator('.preview-image').evaluate(image => [image.naturalWidth, image.naturalHeight]), [4, 2]);
  const imageBox = await page.locator('.preview-image').boundingBox();
  assert.ok(imageBox && Math.abs(imageBox.width / imageBox.height - 2) < 0.06, `image was cropped: ${JSON.stringify(imageBox)}`);
  // A large image decodes without freezing the client: the files tab remains
  // responsive and its natural size is observed within a bounded time.
  await showFiles();
  const largeStart = Date.now();
  await page.locator('.files-row[data-path="large.png"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-image')?.naturalWidth === 2400, null, { timeout: 20000 });
  evidence.large_image_ms = Date.now() - largeStart;
  await showFiles();
  await page.locator('.files-row[data-path="newdir"]').click();
  evidence.large_image_responsive = true;
  evidence.agent_files = { tree, deleted_preview: deletedPreview.slice(0, 80), image: [4, 2], aspect: 2, large_ms: evidence.large_image_ms };
  await page.screenshot({ path: path.join(output, 'files-agent.png') });

  // A second Agent turn replaces the image while its preview tab is open.
  await showFiles();
  await page.locator('.files-row[data-path="agent.png"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-image')?.naturalWidth === 4);
  await send('IMAGE_SWAP 替换图片');
  await waitAnswer('IMAGE_SWAP_DONE');
  await page.waitForFunction(() => document.querySelector('.preview-image')?.naturalWidth === 3, null, { timeout: 30000 });
  assert.deepEqual(await page.locator('.preview-image').evaluate(image => [image.naturalWidth, image.naturalHeight]), [3, 5]);
  evidence.image_refresh = [3, 5];

  // The Git diff is the runtime's own view of the same workspace changes,
  // opened through the real `/diff` command and rendered by the changes view.
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled);
  await page.locator('#message').fill('/diff');
  await page.locator('#message').press('Enter');
  await page.waitForSelector('.review-file', { timeout: 30000 });
  // Ask the runtime for the workspace's current truth through the visible
  // refresh control; the first render may have been requested while the
  // previous action was still settling.
  await page.locator('button[aria-label="重新读取工作区变更"]').first().click();
  await page.waitForTimeout(1200);
  await page.locator('.review-file').first().click();
  await page.waitForSelector('.menu .menu-item');
  const diffRows = await page.locator('.menu .menu-item').allInnerTexts();
  const diffText = diffRows.join('\n');
  assert.match(diffText, /renamed\.txt/);
  assert.match(diffText, /existing\.txt \+1 −1/);
  assert.match(diffText, /delete-me\.txt \+0 −1/);
  assert.match(diffText, /agent\.png/);
  await page.keyboard.press('Escape');
  evidence.diff = diffRows;

  // Restart: the same file still opens with the latest bytes.
  await app.evaluate(({ app }) => app.exit(0));
  await new Promise(resolve => setTimeout(resolve, 1200));
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForSelector('.session-row');
  await page.locator('.session-row').first().click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await page.keyboard.press('Meta+P');
  await page.waitForSelector('.files-row[data-path="newdir"]');
  await showFiles();
  await page.locator('.files-row[data-path="newdir"]').click();
  await page.waitForSelector('.files-row[data-path="newdir/renamed.txt"]');
  await page.locator('.files-row[data-path="newdir/renamed.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('fresh'));
  evidence.restart_preview = true;

  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'files-agent-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, providerErrors, requests: requests.slice(-5) }, null, 2));
  process.exitCode = 1;
} finally {
  try { await app?.close(); } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
}
