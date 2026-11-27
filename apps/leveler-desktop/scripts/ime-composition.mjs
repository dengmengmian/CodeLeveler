// Real Chromium IME composition: Enter that commits a composition must not
// send a message. CDP drives the same composition pipeline the OS IME uses.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-ime-');
const home = path.join(temporary, 'home');
await mkdir(home, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n', { mode: 0o600 });
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
const evidence = { scope: 'real Chromium composition pipeline through CDP; no provider call is expected' };
let app;
let page;
const errors = [];
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.evaluate(() => {
    globalThis.__compositions = [];
    const input = document.querySelector('#message');
    for (const type of ['compositionstart', 'compositionupdate', 'compositionend']) input.addEventListener(type, event => globalThis.__compositions.push({ type, data: event.data }));
  });
  await page.locator('#message').click();
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Input.imeSetComposition', { text: 'ni hao', selectionStart: 6, selectionEnd: 6 });
  await page.waitForTimeout(150);
  const beforeEnter = await page.evaluate(() => ({ value: document.querySelector('#message').value, composing: globalThis.__compositions.map(item => item.type) }));
  assert.equal(beforeEnter.value, 'ni hao');
  assert.ok(beforeEnter.composing.includes('compositionstart'));
  // The OS IME commits the candidate on Enter. The keydown that follows must
  // never reach submitComposer.
  await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 36 });
  await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 36 });
  await page.waitForTimeout(500);
  const after = await page.evaluate(() => ({
    value: document.querySelector('#message').value,
    composing: globalThis.__compositions.map(item => item.type),
    sessions: document.querySelectorAll('.session-row').length,
    home: document.body.classList.contains('home'),
    users: document.querySelectorAll('.message.user').length,
  }));
  assert.equal(after.users, 0, 'a composition Enter must not create a user message');
  assert.equal(after.sessions, 0, 'a composition Enter must not create a session');
  assert.ok(after.value.includes('ni hao'), `committed text must stay in the composer: ${after.value}`);
  evidence.composition = { before: beforeEnter, after };
  // Commit the composition the way an IME confirms a candidate, then a second
  // Enter reaches the normal submit path. This profile has no provider, so the
  // real result is the provider-setup refusal, never a fabricated session.
  await cdp.send('Input.insertText', { text: 'ni hao' });
  await page.waitForFunction(() => globalThis.__compositions.some(item => item.type === 'compositionend'), null, { timeout: 5000 });
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent?.includes('Provider'), null, { timeout: 10000 });
  evidence.enter_after_commit = await page.locator('#composer-error').innerText();
  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'ime-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors }, null, 2));
  process.exitCode = 1;
} finally {
  try { await app?.close(); } catch { /* the test owns the app */ }
}
