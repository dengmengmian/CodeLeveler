// R4 image input: clipboard paste, multiple images, removal, and the actual
// multimodal bytes the provider receives. Historical messages expose only the
// Runtime's image count; that limit is asserted instead of faked.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { deflateSync } from 'node:zlib';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-image-input-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
const CRC = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
const crc32 = buffer => { let c = 0xffffffff; for (const byte of buffer) c = CRC[(c ^ byte) & 0xff] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };
const chunk = (type, data) => { const length = Buffer.alloc(4); length.writeUInt32BE(data.length); const body = Buffer.concat([Buffer.from(type, 'ascii'), data]); const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body)); return Buffer.concat([length, body, crc]); };
function png(width, height, [r, g, b]) {
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(width, 0); ihdr.writeUInt32BE(height, 4); ihdr[8] = 8; ihdr[9] = 6;
  const rows = [];
  for (let y = 0; y < height; y += 1) { const row = Buffer.alloc(1 + width * 4); for (let x = 0; x < width; x += 1) { row[1 + x * 4] = r; row[2 + x * 4] = g; row[3 + x * 4] = b; row[4 + x * 4] = 255; } rows.push(row); }
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(Buffer.concat(rows))), chunk('IEND', Buffer.alloc(0))]);
}
const imageA = png(4, 3, [240, 20, 20]);
const imageB = png(3, 4, [20, 20, 240]);
const fileA = path.join(temporary, 'a.png'); const fileB = path.join(temporary, 'b.png');
await writeFile(fileA, imageA); await writeFile(fileB, imageB);

const requests = [];
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw); const tools = (body.tools ?? []).map(tool => tool.function?.name);
  const messages = body.messages ?? [];
  const lastUser = [...messages].reverse().find(message => message.role === 'user');
  // Only this turn's message; earlier turns' images stay in the context.
  const images = Array.isArray(lastUser?.content) ? lastUser.content.filter(part => part.type === 'image_url').map(part => part.image_url?.url ?? '') : [];
  const text = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser?.content ?? '');
  requests.push({ text, images });
  if (!tools.length) { response.writeHead(200, { 'content-type': 'application/json' }); response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] })); return; }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const chunk = (delta, finish) => JSON.stringify({ id: 'img', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] });
  const answer = text.includes('IMG_TWO') ? 'IMG_TWO_DONE' : text.includes('IMG_ONE') ? 'IMG_ONE_DONE' : 'SEED_OK';
  response.write(`data: ${chunk({ content: answer }, null)}\n\ndata: ${chunk({}, 'stop')}\n\ndata: [DONE]\n\n`);
  response.end();
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\nvision=true\n`, { mode: 0o600 });

const evidence = { scope: 'real clipboard paste + native picker seam + real Runtime multimodal request' };
const errors = [];
let app;
let page;
const cleanup = async () => {
  try { await app?.evaluate(async ({ clipboard }) => { clipboard.clear(); }); } catch { /* optional */ }
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
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('SEED image session');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('SEED_OK'));

  const settle = () => page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled);
  const pasteImage = async file => {
    await settle();
    await app.evaluate(async ({ clipboard, ClipboardItem }, filePath) => {
      const { readFileSync } = process.getBuiltinModule('node:fs');
      const blob = new Blob([readFileSync(filePath)], { type: 'image/png' });
      await clipboard.write([new ClipboardItem({ 'image/png': blob })]);
    }, file);
    // A paste that lands while another client action is still settling is
    // refused with a visible reason; the user retries, and so does this check.
    for (let attempt = 0; attempt < 3; attempt += 1) {
      await settle();
      const before = await page.locator('.attachment-chip').count();
      await page.locator('#message').focus();
      await page.keyboard.press('Meta+V');
      try {
        await page.waitForFunction(count => document.querySelectorAll('.attachment-chip').length > count, before, { timeout: 8000 });
        return;
      } catch {
        const error = await page.locator('#composer-error').innerText().catch(() => '');
        if (!error.includes('未完成')) throw new Error(`paste failed: ${error}`);
      }
    }
    throw new Error('paste did not import an image after retries');
  };
  const pickImage = async file => {
    await app.evaluate(({ dialog }, path) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [path] }); }, file);
    await page.locator('#attachment-add').click();
    await page.getByRole('option', { name: '添加图片', exact: true }).click();
  };
  const chips = () => page.locator('.attachment-chip');
  const ready = () => page.waitForFunction(() => [...document.querySelectorAll('.attachment-chip')].every(chip => chip.dataset.attachmentStatus === 'ready'));

  // Paste one image, then add a second through the picker: two attachments.
  await pasteImage(fileA);
  await page.waitForFunction(() => document.querySelectorAll('.attachment-chip').length === 1);
  await ready();
  await pickImage(fileB);
  await page.waitForFunction(() => document.querySelectorAll('.attachment-chip').length === 2);
  await ready();
  assert.equal(await chips().count(), 2);
  await page.locator('#message').fill('IMG_TWO 两张图片');
  await page.locator('#send').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('IMG_TWO_DONE')), null, { timeout: 60000 });
  const two = requests.find(item => item.text.includes('IMG_TWO'));
  assert.equal(two.images.length, 2);
  // The Runtime normalizes image bytes before the provider call; the product
  // contract is that the model receives the same pictures, so compare the
  // decoded dimensions rather than a byte hash.
  const dimensions = url => {
    assert.match(url, /^data:image\/png;base64,/, `runtime preserved a decodable PNG: ${url.slice(0, 32)}`);
    const bytes = Buffer.from(url.slice(url.indexOf(',') + 1), 'base64');
    assert.equal(bytes.subarray(1, 4).toString('ascii'), 'PNG');
    return [bytes.readUInt32BE(16), bytes.readUInt32BE(20)];
  };
  const sizes = two.images.map(dimensions).map(size => size.join('x')).sort();
  assert.deepEqual(sizes, ['3x4', '4x3']);
  evidence.paste_and_multi = { attachments: 2, image_parts: two.images.length, dimensions: sizes };

  // Add two more, remove one, then send: exactly one image reaches the model.
  await pickImage(fileA);
  await page.waitForFunction(() => document.querySelectorAll('.attachment-chip').length === 1);
  await ready();
  await pickImage(fileB);
  await page.waitForFunction(() => document.querySelectorAll('.attachment-chip').length === 2);
  await ready();
  await chips().nth(1).locator('.attachment-remove').click();
  await page.waitForFunction(() => document.querySelectorAll('.attachment-chip').length === 1);
  await page.locator('#message').fill('IMG_ONE 一张图片');
  await page.locator('#send').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('IMG_ONE_DONE')), null, { timeout: 60000 });
  const one = requests.find(item => item.text.includes('IMG_ONE'));
  assert.equal(one.images.length, 1);
  assert.deepEqual(dimensions(one.images[0]), [4, 3], 'the remaining image is still the pasted one');
  evidence.remove_before_send = { image_parts: one.images.length, dimensions: [4, 3] };

  // Historical messages expose the Runtime's count only: two tiles for the
  // two-image turn, and the tile says content is not available.
  const twoMessage = page.locator('.message.user').filter({ hasText: 'IMG_TWO' });
  assert.equal(await twoMessage.locator('.message-image-tile').count(), 2);
  const label = await twoMessage.locator('.message-image-tile').first().getAttribute('aria-label');
  assert.match(label, /运行时只提供数量/);
  evidence.history_limit = { tiles: 2, label };
  await page.screenshot({ path: path.join(output, 'image-input.png') });

  // Plain text paste is not intercepted.
  await app.evaluate(({ clipboard }) => { clipboard.writeText('PLAIN_PASTE_TEXT'); });
  await page.locator('#message').focus();
  await page.keyboard.press('Meta+V');
  await page.waitForFunction(() => document.querySelector('#message').value.includes('PLAIN_PASTE_TEXT'));
  assert.equal(await chips().count(), 0);
  evidence.text_paste_untouched = true;

  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'image-input-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, requests: requests.slice(-4) }, null, 2));
  process.exitCode = 1;
} finally {
  await cleanup();
  try { await app?.close(); } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
}
