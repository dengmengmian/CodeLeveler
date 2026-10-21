// Permanent regression: one turn's confirmed file change is presented once per
// semantic surface, a finished turn is not left running, and the final answer
// is separated from the tool process. Synthetic JSONL bridge; real renderer.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, chmod } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-parity-');
const wrapper = path.join(temporary, 'bridge');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
await writeFile(wrapper, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(path.join(root, 'test/fixtures/presentation-turn.cjs'))}\n`);
await chmod(wrapper, 0o700);

let application;
let page;
const errors = [];
const evidence = { scope: 'test-only synthetic JSONL bridge; real Electron renderer', passed: false };
try {
  application = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: wrapper },
  });
  page = await application.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.stack ?? error.message));
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1100, 900));
  await page.waitForFunction(() => document.querySelector('.session-row[data-session-id="parity"]'));
  await page.locator('.session-row[data-session-id="parity"]').click();
  await page.waitForFunction(() => document.querySelector('.tool-row[data-tool-id="t1"]')
    && document.querySelectorAll('.message.assistant').length >= 2);
  // Let the runtime's settling session_updated arrive after the terminal.
  await page.waitForTimeout(900);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const toolRow = page.locator('.tool-row[data-tool-id="t1"]');
  if (!(await toolRow.evaluate(node => node.open))) await toolRow.locator('.tool-summary').click();
  await page.waitForSelector('.tool-row[data-tool-id="t1"] .confirmed-diff');
  const view = await page.evaluate(() => {
    const list = document.querySelector('#conversation');
    const fileCard = list.querySelectorAll('.answer-file');
    const names = [...fileCard].map(node => node.querySelector('.answer-file-name')?.textContent ?? '');
    const toolRows = list.querySelectorAll('.tool-row[data-tool-id="t1"]');
    const rounds = list.querySelectorAll('.execution-round');
    const finalMessage = list.querySelector('.message.assistant:not(.narration)');
    const process = list.querySelector('.process-group');
    return {
      answerFiles: fileCard.length,
      answerFileNames: names,
      toolRows: toolRows.length,
      rounds: rounds.length,
      diffBlocks: list.querySelectorAll('.tool-row[data-tool-id="t1"] .confirmed-diff').length,
      workspaceCards: list.querySelectorAll('.changed-files-card').length,
      running: !!list.querySelector('.running-row'),
      finalText: finalMessage?.textContent?.trim() ?? null,
      finalInsideProcess: !!process?.contains(finalMessage),
      narrationInsideProcess: !!process?.querySelector('.message.narration'),
      status: document.querySelector('#conversation-status')?.textContent ?? null,
    };
  });
  evidence.view = view;
  const file = 'leveler-chat-20261021-000850.md';
  assert.equal(view.answerFiles, 1, `the confirmed file change is presented ${view.answerFiles} times: ${JSON.stringify(view.answerFileNames)}`);
  assert.equal(view.answerFileNames[0], file);
  assert.equal(view.toolRows, 1, `the tool call is presented ${view.toolRows} times`);
  assert.equal(view.diffBlocks, 1, 'the confirmed diff is rendered inside the tool row exactly once');
  assert.equal(view.workspaceCards, 0, 'a workspace diff is re-presented inside the transcript');
  assert.equal(view.running, false, 'a finished turn still shows the running row');
  assert.match(view.finalText ?? '', /已写入 leveler-chat-20261021-000850\.md/);
  assert.equal(view.finalInsideProcess, false, 'the final answer is folded into the tool process');
  assert.equal(view.narrationInsideProcess, true, 'interim narration is not grouped with the tool process');
  await page.locator('#conversation').screenshot({ path: path.join(output, 'presentation-parity.png') });
  assert.deepEqual(errors, []);
  evidence.passed = true;
  await writeFile(path.join(output, 'presentation-parity-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.error = error.message;
  if (page && !page.isClosed()) evidence.observed = await page.evaluate(() => ({
    classes: [...document.querySelectorAll('#conversation *')].map(node => node.className).filter(value => typeof value === 'string' && value.includes('message')),
    text: document.querySelector('#conversation')?.innerText.slice(0, 400),
  })).catch(captureError => ({ captureError: captureError.message }));
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'presentation-parity-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'presentation-parity-evidence.json'), JSON.stringify({ ...evidence, page_errors: errors }, null, 2) + '\n');
  throw error;
} finally {
  if (application) await application.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
