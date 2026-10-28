// Permanent regression: a turn reads as one chronological conversation.
// Narration sits beside the execution groups it introduced, every group is
// collapsed by default, the confirmed Diff lives in the Workbench (never as a
// large block in the transcript), and a finished turn shows its receipt instead
// of a running line. Synthetic JSONL bridge; real Electron renderer.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, chmod } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
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
  await page.waitForFunction(() => document.querySelector('.execution-group.execution-edit')
    && document.querySelector('.message.assistant.final'));
  // Let the runtime's settling session_updated arrive after the terminal.
  await page.waitForTimeout(900);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));

  const view = await page.evaluate(() => {
    const list = document.querySelector('#conversation');
    const children = [...list.children].map(node => node.className);
    const narration = list.querySelector('.message.assistant.narration');
    const final = list.querySelector('.message.assistant.final');
    const edit = list.querySelector('.execution-group.execution-edit');
    const receipt = list.querySelector('.turn-receipt');
    return {
      children,
      narrationText: narration?.textContent?.trim() ?? null,
      finalText: final?.textContent?.trim() ?? null,
      editCollapsed: edit ? !edit.open : null,
      editLabel: edit?.querySelector('.execution-label')?.textContent ?? null,
      editHint: edit?.querySelector('.execution-hint')?.textContent ?? null,
      editNarrationParent: !!edit?.contains(narration),
      finalInGroup: !!list.querySelector('.execution-group')?.contains(final),
      toolRows: list.querySelectorAll('.tool-row[data-tool-id="t1"]').length,
      inlineDiffs: list.querySelectorAll('.confirmed-diff').length,
      answerFiles: list.querySelectorAll('.answer-file').length,
      processHeadings: list.querySelectorAll('.process-heading').length,
      running: !!list.querySelector('.running-row'),
      receiptText: receipt?.innerText ?? null,
      receiptStatus: receipt?.dataset.status ?? null,
      status: document.querySelector('#conversation-status')?.textContent ?? null,
    };
  });
  evidence.view = view;
  assert.equal(view.toolRows, 0, 'a collapsed execution group painted its tool rows anyway');
  assert.equal(view.inlineDiffs, 0, 'the full Diff is re-presented inline');
  assert.equal(view.answerFiles, 0, 'a duplicate answer file card exists');
  assert.equal(view.processHeadings, 0, 'the removed work-steps wrapper is still rendered');
  assert.equal(view.editCollapsed, true, 'the execution group is not collapsed by default');
  assert.match(view.editLabel ?? '', /修改 1 个文件 · \+1 \/ -1/);
  assert.equal(view.editHint, '查看变更');
  assert.match(view.narrationText ?? '', /会话记录已写入/);
  assert.equal(view.editNarrationParent, false, 'narration is folded into an execution group');
  assert.match(view.finalText ?? '', /已写入 leveler-chat-20261021-000850\.md/);
  assert.equal(view.finalInGroup, false, 'the final answer is folded into an execution group');
  assert.equal(view.running, false, 'a finished turn still shows the running row');
  assert.equal(view.receiptStatus, 'answered');
  assert.match(view.receiptText ?? '', /回答结束/);
  assert.match(view.receiptText ?? '', /轮/);
  assert.match(view.receiptText ?? '', /次工具调用/);

  // The modification summary opens the existing Changes panel, not a new
  // viewer, and the complete confirmed patch is painted there.
  await page.locator('.execution-group.execution-edit > .execution-summary').click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-open'));
  await page.waitForFunction(() => document.querySelector('.review-scope')?.textContent.includes('已确认'));
  const changes = await page.evaluate(() => ({
    scope: document.querySelector('.review-scope')?.textContent ?? '',
    file: document.querySelector('.review-path')?.textContent ?? '',
    counts: document.querySelector('.review-counts')?.textContent ?? '',
    lines: document.querySelectorAll('.review-body .review-line').length,
    status: document.querySelector('.review-status')?.textContent ?? null,
    add: document.querySelector('.review-line.added .text')?.textContent ?? null,
    remove: document.querySelector('.review-line.removed .text')?.textContent ?? null,
  }));
  evidence.changes = changes;
  assert.match(changes.scope, /历史证据/);
  assert.equal(changes.file, 'leveler-chat-20261021-000850.md');
  assert.match(changes.counts, /\+1/);
  assert.ok(changes.lines >= 4, `the complete diff is not painted: ${changes.lines}`);
  assert.equal(changes.status, null, `the diff viewer reported: ${changes.status}`);
  assert.equal(changes.add, 'new line');
  assert.equal(changes.remove, 'old line');

  // Closing the panel restores the conversation's width.
  await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-open'));

  // Expanding the modification group reveals its files and the real tool row.
  await page.locator('.execution-group.execution-edit .execution-toggle').click();
  await page.waitForFunction(() => document.querySelector('.execution-group.execution-edit')?.open);
  await page.waitForFunction(() => document.querySelector('.execution-file-row'));
  const expanded = await page.evaluate(() => ({
    file: document.querySelector('.execution-file-path')?.textContent ?? null,
    stat: document.querySelector('.execution-file-row')?.innerText ?? '',
    tool: !!document.querySelector('.execution-group.execution-edit .tool-row[data-tool-id="t1"]'),
  }));
  evidence.expanded = expanded;
  assert.equal(expanded.file, 'leveler-chat-20261021-000850.md');
  assert.match(expanded.stat, /\+1/);
  assert.equal(expanded.tool, true);
  await page.locator('#conversation').screenshot({ path: path.join(output, 'presentation-parity.png') });
  assert.deepEqual(errors, []);
  evidence.passed = true;
  await writeFile(path.join(output, 'presentation-parity-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.error = error.message;
  if (page && !page.isClosed()) evidence.observed = await page.evaluate(() => ({
    classes: [...document.querySelectorAll('#conversation *')].map(node => node.className).filter(value => typeof value === 'string' && value.includes('message')),
    text: document.querySelector('#conversation')?.innerText.slice(0, 800),
  })).catch(captureError => ({ captureError: captureError.message }));
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'presentation-parity-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'presentation-parity-evidence.json'), JSON.stringify({ ...evidence, page_errors: errors }, null, 2) + '\n');
  throw error;
} finally {
  if (application) await application.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
