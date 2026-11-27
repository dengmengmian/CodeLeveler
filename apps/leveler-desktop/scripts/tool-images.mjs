// R5 tool images + history images: a real Runtime archives the bytes a tool
// returned, the Desktop shows the same picture at its real size, and a
// reopened session (and a deleted store object) says the truth about what is
// available. The provider is test-only; the tool, media store, Runtime and
// Electron surface are the real ones.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { deflateSync } from 'node:zlib';
import { mkdtemp, mkdir, writeFile, readdir, unlink } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-tool-images-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const profile = path.join(temporary, 'profile');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });

const CRC = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
const crc32 = buffer => { let c = 0xffffffff; for (const byte of buffer) c = CRC[(c ^ byte) & 0xff] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };
const chunk = (type, data) => { const length = Buffer.alloc(4); length.writeUInt32BE(data.length); const body = Buffer.concat([Buffer.from(type, 'ascii'), data]); const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body)); return Buffer.concat([length, body, crc]); };
function png(width, height, [r, g, b]) {
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(width, 0); ihdr.writeUInt32BE(height, 4); ihdr[8] = 8; ihdr[9] = 6;
  const rows = [];
  for (let y = 0; y < height; y += 1) { const row = Buffer.alloc(1 + width * 4); for (let x = 0; x < width; x += 1) { row[1 + x * 4] = r; row[2 + x * 4] = g; row[3 + x * 4] = b; row[4 + x * 4] = 255; } rows.push(row); }
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(Buffer.concat(rows))), chunk('IEND', Buffer.alloc(0))]);
}
const redPath = path.join(workspace, 'red-4x3.png');
const bluePath = path.join(workspace, 'blue-3x4.png');
const pastedPath = path.join(temporary, 'pasted-6x4.png');
await writeFile(redPath, png(4, 3, [230, 20, 20]));
await writeFile(bluePath, png(3, 4, [20, 20, 230]));
await writeFile(pastedPath, png(6, 4, [20, 200, 40]));

const requests = [];
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const messages = body.messages ?? [];
  // A tool-returned image is injected as an image-only user message; it must
  // not steal the marker from the person's request that preceded it.
  const textOf = message => typeof message.content === 'string' ? message.content
    : Array.isArray(message.content) ? message.content.filter(part => part.type === 'text').map(part => part.text ?? '').join('') : '';
  const lastUser = messages.map((message, index) => [message, index])
    .filter(([message]) => message.role === 'user' && textOf(message).trim() !== '')
    .at(-1);
  const text = lastUser ? textOf(lastUser[0]) : '';
  const afterUser = lastUser ? messages.slice(lastUser[1] + 1) : [];
  const hasToolResult = afterUser.some(message => message.role === 'tool');
  const toolNames = (body.tools ?? []).map(tool => tool.function?.name);
  requests.push({ text, toolNames, hasToolResult });
  const send = (delta, finish) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const frame = (payload, finish) => `data: ${JSON.stringify({ id: 'tool-images', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: payload, finish_reason: finish }] })}\n\n`;
    response.write(frame(delta, null));
    response.write(frame({}, finish));
    response.end('data: [DONE]\n\n');
  };
  if (text.includes('TOOL_IMAGES')) {
    // The Media pack is opt-in, exactly as a real model discovers it: ask for
    // it, then use the tool it exposes on the following request.
    if (!hasToolResult) {
      send({ tool_calls: [{ index: 0, id: 'r5-enable-media', type: 'function', function: { name: 'capability', arguments: JSON.stringify({ action: 'enable', id: 'media' }) } }] }, 'tool_calls');
      return;
    }
    const askedForImage = afterUser.some(message => message.role === 'assistant'
      && (message.tool_calls ?? []).some(call => call.function?.name === 'view_image'));
    if (!askedForImage && toolNames.includes('view_image')) {
      send({ tool_calls: [{ index: 0, id: 'r5-view-image', type: 'function', function: { name: 'view_image', arguments: JSON.stringify({ paths: ['red-4x3.png', 'blue-3x4.png'] }) } }] }, 'tool_calls');
      return;
    }
    send({ content: 'TOOL_IMAGES_DONE' }, 'stop');
    return;
  }
  if (text.includes('IMG_HISTORY')) { send({ content: 'IMG_HISTORY_DONE' }, 'stop'); return; }
  send({ content: 'SEED_OK' }, 'stop');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\nvision=true\n`, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'real Electron/Runtime/tool execution/media store; isolated home/workspace; test-only SSE provider' };
const errors = [];
let app;
let page;
const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${profile}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
};
const close = async () => {
  // Electron's graceful close can wait on the runtime subscription; this test
  // owns the window and exits it directly, with a bound so cleanup can't hang
  // the suite.
  try {
    await Promise.race([
      app?.evaluate(({ app }) => app.exit(0)),
      new Promise(resolve => setTimeout(resolve, 5000)),
    ]);
  } catch { /* already closed */ }
  app = null;
};
const waitAnswer = text => page.waitForFunction(value => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(value)), text, { timeout: 120000 });
const openSession = async () => {
  await page.waitForFunction(() => document.querySelectorAll('.session-row').length > 0, null, { timeout: 60000 }).catch(() => {});
  const selected = page.locator('.session-row[aria-selected="true"]');
  if (await selected.count() === 0) {
    const rows = page.locator('.session-row');
    if (await rows.count() === 0) throw new Error('no session row after restart');
    await rows.first().click();
  }
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'), null, { timeout: 60000 });
  await page.waitForFunction(() => document.querySelectorAll('.message').length > 0, null, { timeout: 60000 });
};

try {
  await launch();
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('SEED tool image session');
  await page.locator('#send').click();
  await waitAnswer('SEED_OK');

  // The model asks to see two images in one call.
  await page.locator('#message').fill('TOOL_IMAGES 看两个文件');
  await page.locator('#send').click();
  await waitAnswer('TOOL_IMAGES_DONE');

  // Tool rows are built when their execution group opens.
  await page.evaluate(() => {
    for (const group of document.querySelectorAll('details.execution-group:not([open])')) group.querySelector('.execution-summary')?.click();
  });
  const toolRow = page.locator('.tool-row[data-tool-name="view_image"]').last();
  await toolRow.waitFor({ timeout: 60000 });
  assert.match(await toolRow.locator('.tool-image-badge').innerText(), /图片 2/);
  await toolRow.locator('summary').click();
  await page.waitForFunction(() => document.querySelectorAll('.tool-row[data-tool-name="view_image"] .tool-images img.message-image-thumb').length >= 2, null, { timeout: 60000 });
  const cardImages = await page.locator('.tool-row[data-tool-name="view_image"] .tool-images img.message-image-thumb').evaluateAll(images => images.map(image => [image.naturalWidth, image.naturalHeight]));
  assert.deepEqual(cardImages.map(size => size.join('x')).sort(), ['3x4', '4x3'], `real dimensions from the stored bytes: ${cardImages}`);
  evidence.tool_card = { images: cardImages };

  await page.locator('.tool-row[data-tool-name="view_image"] .tool-images img.message-image-thumb').first().click();
  await page.waitForSelector('#image-lightbox[open] img.image-lightbox-image');
  await page.waitForFunction(() => document.querySelector('#image-lightbox img.image-lightbox-image')?.naturalWidth > 0);
  const lightbox = await page.locator('#image-lightbox img.image-lightbox-image').evaluate(image => [image.naturalWidth, image.naturalHeight]);
  assert.deepEqual(lightbox, [4, 3], 'the first tool image opens at full size');
  await page.screenshot({ path: path.join(output, 'r5-tool-image-lightbox.png') });
  await page.locator('#image-lightbox .modal-actions button', { hasText: '关闭' }).click();
  evidence.lightbox = lightbox;

  // A user image becomes a real thumbnail of the stored attachment.
  await app.evaluate(async ({ clipboard, ClipboardItem }, filePath) => {
    const { readFileSync } = process.getBuiltinModule('node:fs');
    const blob = new Blob([readFileSync(filePath)], { type: 'image/png' });
    await clipboard.write([new ClipboardItem({ 'image/png': blob })]);
  }, pastedPath);
  await page.locator('#message').focus();
  await page.keyboard.press('Meta+V');
  await page.waitForFunction(() => document.querySelectorAll('.attachment-chip').length === 1, null, { timeout: 30000 });
  await page.waitForFunction(() => document.querySelector('.attachment-chip')?.dataset.attachmentStatus === 'ready', null, { timeout: 60000 });
  await page.locator('#message').fill('IMG_HISTORY 发送一张图');
  await page.locator('#send').click();
  await waitAnswer('IMG_HISTORY_DONE');
  const historyMessage = page.locator('.message.user').filter({ hasText: 'IMG_HISTORY' });
  await page.waitForFunction(() => {
    const row = [...document.querySelectorAll('.message.user')].find(node => node.textContent.includes('IMG_HISTORY'));
    return row?.querySelector('.message-images img.message-image-thumb')?.naturalWidth > 0;
  }, null, { timeout: 60000 });
  const userImage = await historyMessage.locator('.message-images img.message-image-thumb').first().evaluate(image => [image.naturalWidth, image.naturalHeight]);
  assert.deepEqual(userImage, [6, 4], 'the live history thumbnail is the pasted picture');
  const userSha = await historyMessage.locator('.message-image-tile').first().getAttribute('data-sha256');
  assert.match(userSha, /^[a-f0-9]{64}$/);
  evidence.user_image = { dimensions: userImage, sha256: userSha };
  await page.screenshot({ path: path.join(output, 'r5-history-image.png') });

  // Restart: both the tool image and the user image are reopened from the
  // durable record and the media store.
  await close();
  await launch();
  await openSession();
  await page.waitForFunction(() => document.querySelector('details.execution-group'), null, { timeout: 60000 });
  const group = page.locator('details.execution-group').first();
  if (!(await group.evaluate(node => node.open))) await group.locator('.execution-summary').click();
  await page.waitForFunction(() => {
    const row = [...document.querySelectorAll('.tool-row[data-tool-name="view_image"]')].at(-1);
    if (!row) return false;
    if (!row.open) row.querySelector('summary')?.click();
    return row.querySelectorAll('.tool-images img.message-image-thumb').length >= 2;
  }, null, { timeout: 60000 });
  const reopenedTool = await page.locator('.tool-row[data-tool-name="view_image"] .tool-images img.message-image-thumb').evaluateAll(images => images.map(image => [image.naturalWidth, image.naturalHeight]));
  assert.deepEqual(reopenedTool.map(size => size.join('x')).sort(), ['3x4', '4x3'], 'tool images survive a restart');
  await page.waitForFunction(() => {
    const row = [...document.querySelectorAll('.message.user')].find(node => node.textContent.includes('IMG_HISTORY'));
    return row?.querySelector('.message-images img.message-image-thumb')?.naturalWidth > 0;
  }, null, { timeout: 60000 });
  const reopenedUser = await page.locator('.message.user').filter({ hasText: 'IMG_HISTORY' }).locator('.message-images img.message-image-thumb').first().evaluate(image => [image.naturalWidth, image.naturalHeight]);
  assert.deepEqual(reopenedUser, [6, 4], 'the user image survives a restart');
  evidence.restart = { tool: reopenedTool, user: reopenedUser };
  await page.screenshot({ path: path.join(output, 'r5-images-after-restart.png') });

  // The stored object is gone: the ref still exists, and the tile says the
  // picture is unavailable instead of drawing a blank or a lookalike.
  const mediaFiles = [];
  for (const project of await readdir(path.join(home, 'state/projects'))) {
    const media = path.join(home, 'state/projects', project, 'media');
    try { for (const name of await readdir(media)) mediaFiles.push(path.join(media, name)); } catch { /* no media */ }
  }
  const target = mediaFiles.find(file => path.basename(file).startsWith(userSha));
  assert.ok(target, `the media store holds the user image: ${mediaFiles.length} files`);
  await unlink(target);
  await close();
  await launch();
  await openSession();
  await page.waitForFunction(() => {
    const row = [...document.querySelectorAll('.message.user')].find(node => node.textContent.includes('IMG_HISTORY'));
    return row?.querySelector('.message-image-tile.unavailable');
  }, null, { timeout: 60000 });
  const unavailable = await page.locator('.message.user').filter({ hasText: 'IMG_HISTORY' }).locator('.message-image-tile.unavailable').first().innerText();
  assert.match(unavailable, /图片不可用/);
  evidence.missing_media = { tile: unavailable };
  await page.screenshot({ path: path.join(output, 'r5-image-unavailable.png') });

  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r5-images-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, requests: requests.slice(-6) }, null, 2));
  process.exitCode = 1;
} finally {
  await close();
  await new Promise(resolve => server.close(resolve));
}
