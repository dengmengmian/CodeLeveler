// Real Electron renderer + synthetic runtime peer: the two duplicate-display
// regressions fixed in R6-A-D.
//
// The peer is a fixture (no provider/tool execution is claimed); the renderer,
// reducer, merge and DOM are the real ones. This is the visual companion to
// the deterministic reducer tests: one final answer, one turn error after the
// terminal-time history reload.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, chmod } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-r6a-duplicates-');
const wrapper = path.join(temporary, 'bridge');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
await writeFile(wrapper, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(path.join(root, 'test/fixtures/conversation-architecture.cjs'))}\n`);
await chmod(wrapper, 0o700);
const evidence = { scope: 'test-only synthetic JSONL bridge; real Electron renderer + real reducers', results: [] };
let application;
let page;
const errors = [];
async function shot(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, `r6a-${name}.png`) });
  evidence.results.push({ screenshot: `r6a-${name}.png` });
}
async function send(text) {
  await page.locator('#message').fill(text);
  await page.locator('#message').press('Enter');
}
async function settled() {
  await page.waitForFunction(() => ['已回答', '已完成', '已失败'].includes(document.querySelector('#conversation-status')?.textContent), null, { timeout: 30000 });
}
try {
  application = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: wrapper },
  });
  page = await application.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.stack ?? error.message));
  await page.waitForFunction(() => document.querySelector('.session-row[data-session-id="architecture"]'));
  await page.locator('.session-row[data-session-id="architecture"]').click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent === '就绪');

  await send('重复答案');
  await settled();
  await page.waitForTimeout(1500); // let the terminal-time history reload land
  const answer = await page.evaluate(() => ({
    finals: [...document.querySelectorAll('#conversation .message.assistant.final')].map(node => node.innerText),
    narrations: [...document.querySelectorAll('#conversation .message.assistant.narration')].map(node => node.innerText),
    assistants: document.querySelectorAll('#conversation .message.assistant').length,
  }));
  evidence.duplicate_answer = answer;
  await shot('duplicates-final-answer');
  assert.equal(answer.finals.length, 1, 'exactly one final answer');
  assert.match(answer.finals[0], /唯一的最终答案/);
  assert.equal(answer.narrations.length, 1, 'the earlier block is narration');

  await send('失败重载');
  // The failed terminal's label is the receipt's own status, not a page label.
  await page.waitForFunction(() => [...document.querySelectorAll('#conversation .turn-receipt')].some(node => node.dataset.status === 'failed'), null, { timeout: 30000 });
  await page.waitForTimeout(2500); // terminal → refreshSnapshot + loadHistory
  const failure = await page.evaluate(() => ({
    errors: [...document.querySelectorAll('#conversation .turn-error-row')].map(node => node.innerText),
    receipts: [...document.querySelectorAll('#conversation .turn-receipt')].map(node => node.innerText),
  }));
  evidence.turn_error = failure;
  await shot('duplicates-turn-error');
  assert.equal(failure.errors.length, 1, 'the same durable failure appears once after the reload');
  assert.match(failure.errors[0], /synthetic provider exploded/);
  evidence.passed = errors.length === 0;
} catch (error) {
  evidence.error = error.stack ?? error.message;
  evidence.passed = false;
  if (page && !page.isClosed()) await shot('duplicates-failure').catch(() => {});
} finally {
  if (application) await application.close().catch(() => {});
  console.log(JSON.stringify(evidence, null, 2));
  process.exit(evidence.passed ? 0 : 1);
}
