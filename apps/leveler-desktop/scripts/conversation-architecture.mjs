// Real Electron renderer + synthetic JSONL runtime peer.
//
// This is the conversation information-architecture acceptance: narration,
// collapsed execution groups, a failure that stays visible while folded, an
// approval that never hides inside a fold, confirmed Diff ownership in the
// Workbench, the turn receipt, running feedback, and durable-history resume.
// The peer is a fixture; no provider, Runtime execution or tool result is
// claimed by this script.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, chmod } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-architecture-');
const wrapper = path.join(temporary, 'bridge');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
await writeFile(wrapper, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(path.join(root, 'test/fixtures/conversation-architecture.cjs'))}\n`);
await chmod(wrapper, 0o700);

let application;
let page;
const errors = [];
const evidence = { scope: 'test-only synthetic JSONL bridge; real Electron renderer', passed: false };

async function shot(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, `${name}.png`) });
}
async function send(text) {
  await page.locator('#message').fill(text);
  await page.locator('#message').press('Enter');
}
async function settled() {
  await page.waitForFunction(() => ['已回答', '已完成'].includes(document.querySelector('#conversation-status')?.textContent), null, { timeout: 30000 });
}
function conversationShape() {
  return page.evaluate(() => [...document.querySelector('#conversation').children].map(node => {
    if (node.classList.contains('message') && node.classList.contains('user')) return 'user';
    if (node.classList.contains('message') && node.classList.contains('narration')) return 'narration';
    if (node.classList.contains('message') && node.classList.contains('final')) return 'final';
    if (node.classList.contains('execution-group')) return `execution:${node.dataset.variant ?? [...node.classList].find(name => name.startsWith('execution-'))?.slice('execution-'.length)}`;
    if (node.classList.contains('tool-row')) return 'tool';
    if (node.classList.contains('turn-receipt')) return 'receipt';
    if (node.classList.contains('turn-error-row')) return 'error';
    return node.className;
  }));
}

try {
  application = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: wrapper },
  });
  page = await application.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.stack ?? error.message));
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1240, 900));
  await page.waitForFunction(() => document.querySelector('.session-row[data-session-id="architecture"]'));
  await page.locator('.session-row[data-session-id="architecture"]').click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent === '就绪');

  // ── 1. Pure chat: no execution group, no fabricate. ─────────────────────
  await send('纯聊天');
  await settled();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant.final')].some(node => node.textContent.includes('这是纯聊天回答')));
  let shape = await conversationShape();
  evidence.pureChatShape = shape;
  assert.deepEqual(shape, ['user', 'final', 'receipt']);
  assert.equal(await page.locator('.execution-group').count(), 0, 'a pure chat grew an execution card');
  const firstReceipt = page.locator('.turn-receipt').first();
  assert.equal(await firstReceipt.getAttribute('data-status'), 'answered');
  assert.match(await firstReceipt.innerText(), /回答结束/);
  assert.match(await firstReceipt.innerText(), /轮/);

  // ── 2. Narration, exploration, commands, failure and a real edit. ───────
  await send('调查配置和测试');
  await settled();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant.final')].some(node => node.textContent.includes('已定位问题')));
  shape = await conversationShape();
  evidence.investigationShape = shape;
  assert.deepEqual(shape, [
    'user', 'final', 'receipt',
    'user', 'narration', 'execution:tools', 'narration', 'execution:edit', 'final', 'receipt',
  ]);
  // One process group per contiguous run: the exploration and command steps of
  // the turn share one collapsed row instead of repeating "执行 N 项".
  const commands = page.locator('.execution-group.execution-tools').last();
  const edit = page.locator('.execution-group.execution-edit').last();
  assert.equal(await commands.evaluate(node => node.open), false, 'the process group is not collapsed');
  assert.equal(await edit.evaluate(node => node.open), false, 'the edit group is not collapsed');
  const commandSummary = await commands.locator('.execution-summary').innerText();
  assert.match(commandSummary, /已读取文件/);
  assert.match(commandSummary, /执行了命令/);
  assert.doesNotMatch(commandSummary, /执行 \d+ 项/);
  assert.match(commandSummary, /1 项失败/);
  assert.match(commandSummary, /test mapping::recommended \.\.\. FAILED/, 'a folded command hides its failure');
  assert.match(await edit.locator('.execution-label').innerText(), /修改 3 个文件 · \+4 \/ -4/);
  assert.equal(await edit.locator('.execution-hint').innerText(), '查看变更');
  assert.equal(await page.locator('.message.user').count(), 2);
  assert.equal(await page.locator('.turn-receipt').count(), 2);
  assert.equal(await page.locator('.answer-file').count(), 0, 'a duplicate answer file card exists');
  assert.equal(await page.locator('.confirmed-diff').count(), 0, 'a full Diff is painted in the conversation');
  await shot('architecture-collapsed');

  // Opening a group reveals real rows; it must stay open across later updates
  // and must not move the reader's scroll position.
  await commands.scrollIntoViewIfNeeded();
  const scrollBeforeExpand = await page.evaluate(() => document.querySelector('#conversation-scroll').scrollTop);
  await commands.locator('.execution-summary').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.execution-group.execution-tools')].at(-1)?.open);
  const scrollAfterExpand = await page.evaluate(() => document.querySelector('#conversation-scroll').scrollTop);
  assert.ok(Math.abs(scrollAfterExpand - scrollBeforeExpand) <= 2, `expanding moved the reader from ${scrollBeforeExpand} to ${scrollAfterExpand}`);
  await page.waitForFunction(() => document.querySelector('.execution-group.execution-tools .tool-row[data-tool-id="c2"]'));
  const expandedCommands = await page.evaluate(() => {
    const groups = [...document.querySelectorAll('.execution-group.execution-tools')];
    const group = groups.at(-1);
    return {
      rows: [...group.querySelectorAll('.tool-row')].map(row => ({ id: row.dataset.toolId, status: row.dataset.status, text: row.querySelector('.tool-summary').innerText })),
      collapsedRowsBefore: groups.slice(0, -1).reduce((sum, item) => sum + item.querySelectorAll('.tool-row').length, 0),
    };
  });
  evidence.expandedCommands = expandedCommands;
  assert.deepEqual(expandedCommands.rows.map(row => row.id), ['r1', 'r2', 's1', 'c1', 'c2']);
  const commandRows = expandedCommands.rows.filter(row => row.id.startsWith('c'));
  assert.equal(commandRows[0].status, 'ok');
  assert.equal(commandRows[1].status, 'failed');
  assert.match(commandRows[1].text, /cargo test -p demo/);
  assert.match(commandRows[1].text, /退出码 1/);
  assert.equal(expandedCommands.collapsedRowsBefore, 0, 'a collapsed group painted its child rows');
  // A command's copied output is the command's own text, never the runtime's
  // execution rows.
  await page.locator('.tool-row[data-tool-id="c1"] .tool-summary').click();
  await page.waitForSelector('.tool-row[data-tool-id="c1"] .tool-terminal-output');
  await page.locator('.tool-row[data-tool-id="c1"] .tool-copy').click();
  await page.waitForTimeout(150);
  assert.equal(await application.evaluate(({ clipboard }) => clipboard.readText()), '## main');
  await shot('architecture-expanded-command');

  // ── 3. Approval: one decision entry, visible while the tool waits. ──────
  await send('审批写入');
  await page.waitForFunction(() => document.querySelector('#takeover')?.children.length === 1);
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent === '等待你处理');
  await page.waitForFunction(() => document.querySelectorAll('.message.user').length === 3
    && document.querySelector('.tool-row[data-tool-id="w1"]'));
  const approval = await page.evaluate(() => {
    const writing = document.querySelector('.tool-row[data-tool-id="w1"]');
    return {
      decisionCards: document.querySelectorAll('#takeover .approval-card').length,
      inTranscript: document.querySelectorAll('#conversation .approval-card').length,
      rowStatus: writing?.dataset.status ?? null,
      rowText: writing?.querySelector('.tool-summary')?.innerText ?? null,
      commandGroupStillOpen: [...document.querySelectorAll('.execution-group.execution-tools')].at(-1)?.open ?? null,
    };
  });
  evidence.approval = approval;
  assert.equal(approval.decisionCards, 1, 'the approval has no single decision entry');
  assert.equal(approval.inTranscript, 0, 'the approval is duplicated into the transcript');
  assert.match(approval.rowText ?? '', /等待审批/, 'the waiting tool is invisible');
  assert.equal(approval.commandGroupStillOpen, true, 'a later event closed the group the reader opened');
  await shot('architecture-approval');

  await page.getByRole('button', { name: '拒绝', exact: true }).click();
  await settled();
  await page.waitForFunction(() => document.querySelector('#takeover')?.children.length === 0);
  assert.equal(await page.locator('.tool-row[data-tool-id="w1"]').getAttribute('data-status'), 'failed');
  assert.match(await page.locator('.tool-row[data-tool-id="w1"] .tool-summary').innerText(), /denied by user/);
  assert.equal(await page.getByRole('button', { name: '拒绝', exact: true }).count(), 0);
  assert.equal(await page.locator('.execution-group.execution-tools').last().evaluate(node => node.open), true, 'the reader-opened group closed after approval resolution');

  // ── 4. A running turn keeps live feedback while its group stays folded. ─
  await send('运行中');
  await page.waitForFunction(() => document.querySelector('.tool-row[data-tool-id="run-1"]'));
  const running = await page.evaluate(() => ({
    rowStatus: document.querySelector('.tool-row[data-tool-id="run-1"]')?.dataset.status,
    rowText: document.querySelector('.tool-row[data-tool-id="run-1"] .tool-summary')?.innerText ?? null,
    running: document.querySelector('.running-row .running-text')?.textContent ?? null,
    rowOpen: document.querySelector('.tool-row[data-tool-id="run-1"]')?.open ?? null,
  }));
  evidence.running = running;
  assert.equal(running.rowStatus, 'running');
  assert.match(running.rowText ?? '', /运行命令/);
  assert.match(running.rowText ?? '', /sleep 2/);
  assert.match(running.running ?? '', /sleep 2/, 'the live status does not name the running command');
  assert.equal(running.rowOpen, false, 'a running single command auto-opened its details');
  await shot('architecture-running');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant.final')].some(node => node.textContent.includes('命令执行完成')), null, { timeout: 30000 });
  await settled();
  await page.waitForFunction(() => !document.querySelector('.running-row'));
  assert.equal(await page.locator('.turn-receipt').last().getAttribute('data-status'), 'answered');

  // ── 5. Confirmed Diff → Workbench; workspace Diff is a different source. ─
  await edit.locator('.execution-toggle').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.execution-group.execution-edit')].at(-1)?.open);
  await page.waitForFunction(() => document.querySelectorAll('.execution-group.execution-edit .execution-file-row').length === 3);
  const fileRows = await page.evaluate(() => [...document.querySelectorAll('.execution-group.execution-edit .execution-file-row')].map(node => node.innerText));
  evidence.fileRows = fileRows;
  assert.equal(fileRows.length, 3, `the edit summary lists ${fileRows.length} files`);
  await page.locator('.execution-group.execution-edit .execution-file-row').filter({ hasText: 'src/b.ts' }).last().click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-open'));
  await page.waitForFunction(() => document.querySelector('.review-scope')?.textContent.includes('已确认'));
  const confirmedView = await page.evaluate(() => ({
    scope: document.querySelector('.review-scope')?.textContent ?? '',
    file: document.querySelector('.review-path')?.textContent ?? '',
    counts: document.querySelector('.review-counts')?.textContent ?? '',
    lines: [...document.querySelectorAll('.review-body .review-line')].map(node => node.innerText),
    source: document.querySelector('.review-file')?.title ?? null,
  }));
  evidence.confirmedView = { ...confirmedView, lines: confirmedView.lines.length };
  assert.match(confirmedView.scope, /历史证据/);
  assert.equal(confirmedView.file, 'src/b.ts');
  assert.match(confirmedView.counts, /\+2/);
  assert.ok(confirmedView.lines.some(line => line.includes('new-b1')), 'the selected file is not located in the diff');
  assert.ok(confirmedView.lines.some(line => line.includes('old-b2')));
  // The confirmed viewer keeps every file, and every line, without a cap.
  await page.locator('.review-file').click();
  await page.waitForFunction(() => document.querySelectorAll('.menu .menu-item').length >= 3);
  assert.equal(await page.locator('.menu .menu-item').count(), 3);
  await page.keyboard.press('Escape');
  await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-open'));

  // `/diff` opens the same viewer with the WORKSPACE source.
  await page.locator('#message').fill('/diff');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('.review-scope')?.textContent.includes('当前工作区'));
  await page.waitForFunction(() => document.querySelector('.review-path')?.textContent === 'src/workspace-only.ts');
  const workspaceView = await page.evaluate(() => ({
    scope: document.querySelector('.review-scope')?.textContent ?? '',
    file: document.querySelector('.review-path')?.textContent ?? '',
    lines: document.querySelectorAll('.review-body .review-line').length,
  }));
  evidence.workspaceView = workspaceView;
  assert.match(workspaceView.scope, /当前工作区 Git 变更/);
  assert.equal(workspaceView.file, 'src/workspace-only.ts');
  assert.ok(workspaceView.lines >= 3);
  // Copy the complete patch from the viewer.
  await page.locator('.review-tools button[aria-label="复制完整 diff"]').click();
  await page.waitForTimeout(150);
  const copiedDiff = await application.evaluate(({ clipboard }) => clipboard.readText());
  assert.match(copiedDiff, /\+workspace/, 'the copied Diff is not the selected patch');
  await shot('architecture-workspace-diff');
  await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();

  // Only the final answer carries message actions (reference AssistantNodeView
  // renders none for interim narration); copying it must be undecorated.
  const finals = await page.locator('.message.assistant.final').allInnerTexts();
  await page.locator('.message.assistant.final').last().locator('.message-copy').first().click();
  await page.waitForTimeout(150);
  assert.equal(await application.evaluate(({ clipboard }) => clipboard.readText()), finals.at(-1));

  // ── 6. Desktop widths. ──────────────────────────────────────────────────
  await page.waitForFunction(() => !document.querySelector('.toast'));
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(960, 600));
  await page.waitForFunction(() => innerWidth === 960 && innerHeight === 600);
  if (await page.evaluate(() => document.body.classList.contains('sidebar-open'))) await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.body.classList.contains('sidebar-open'));
  await page.waitForFunction(() => document.querySelector('#sidebar').getBoundingClientRect().right <= 0);
  evidence.narrowLayout = await page.evaluate(() => ({
    body: document.body.className,
    sidebarTransform: getComputedStyle(document.querySelector('#sidebar')).transform,
    sidebarWidth: getComputedStyle(document.querySelector('#sidebar')).width,
    mainLeft: document.querySelector('#main').getBoundingClientRect().left,
  }));
  await page.evaluate(() => { const scroller = document.querySelector('#conversation-scroll'); scroller.scrollTop = scroller.scrollHeight; });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, '960×600 overflows horizontally');
  await shot('architecture-960x600');
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(700, 900));
  await page.waitForFunction(() => innerWidth === 700);
  await page.evaluate(() => { const scroller = document.querySelector('#conversation-scroll'); scroller.scrollTop = scroller.scrollHeight; });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'the narrow window overflows horizontally');
  await shot('architecture-narrow');
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1240, 900));
  await page.waitForFunction(() => innerWidth === 1240);

  // ── 7. Durable-history resume: whole order, no duplicates. ──────────────
  const beforeReload = await conversationShape();
  const beforeCounts = await page.evaluate(() => ({
    users: document.querySelectorAll('.message.user').length,
    groups: document.querySelectorAll('.execution-group').length,
    edits: document.querySelectorAll('.execution-group.execution-edit').length,
    receipts: document.querySelectorAll('.turn-receipt').length,
    finals: document.querySelectorAll('.message.assistant.final').length,
    rows: document.querySelectorAll('.tool-row').length,
  }));
  await page.reload();
  await page.waitForFunction(() => document.querySelector('.session-row[data-session-id="architecture"]'));
  await page.locator('.session-row[data-session-id="architecture"]').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant.final')].some(node => node.textContent.includes('命令执行完成')));
  await page.waitForFunction(() => document.querySelectorAll('.turn-receipt').length >= 4);
  const afterReload = await conversationShape();
  const afterCounts = await page.evaluate(() => ({
    users: document.querySelectorAll('.message.user').length,
    groups: document.querySelectorAll('.execution-group').length,
    edits: document.querySelectorAll('.execution-group.execution-edit').length,
    receipts: document.querySelectorAll('.turn-receipt').length,
    finals: document.querySelectorAll('.message.assistant.final').length,
    rows: document.querySelectorAll('.tool-row').length,
  }));
  const { rows: beforeRows, ...beforeStable } = beforeCounts;
  const { rows: afterRows, ...afterStable } = afterCounts;
  evidence.resume = { beforeCounts, afterCounts, sameShape: JSON.stringify(beforeReload) === JSON.stringify(afterReload) };
  assert.deepEqual(afterStable, beforeStable, 'resume duplicated or dropped conversation items');
  assert.ok(afterRows <= beforeRows, `resume painted more collapsed rows (${afterRows} > ${beforeRows})`);
  assert.deepEqual(afterReload, beforeReload, 'resume changed the event order');
  assert.equal(await page.locator('.message.assistant.narration').count(), 2, 'narration was duplicated on replay');
  await shot('architecture-resume');

  assert.deepEqual(errors, []);
  evidence.passed = true;
  await writeFile(path.join(output, 'conversation-architecture-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.error = error.message;
  if (page && !page.isClosed()) {
    evidence.observed = await page.evaluate(() => ({
      shape: [...document.querySelector('#conversation').children].map(node => node.className),
      text: document.querySelector('#conversation')?.innerText.slice(0, 1200),
      status: document.querySelector('#conversation-status')?.textContent,
    })).catch(observationError => ({ observationError: observationError.message }));
    await page.screenshot({ path: path.join(output, 'architecture-failure.png') }).catch(() => {});
  }
  await writeFile(path.join(output, 'conversation-architecture-evidence.json'), JSON.stringify({ ...evidence, page_errors: errors }, null, 2) + '\n');
  throw error;
} finally {
  if (application) await application.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
