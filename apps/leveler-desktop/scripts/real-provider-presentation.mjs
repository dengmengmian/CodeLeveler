// Real Provider acceptance for the conversation information architecture.
//
// The isolated HOME links the user's real config (read-only use) and never
// copies or prints provider keys. The model is asked to read, run a successful
// and a failing command, and edit one file; the assertions check the rendered
// conversation, not just the model's claim.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, symlink, rm } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-real-presentation-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(home);
await mkdir(workspace);
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'README.md'), '# Real provider presentation fixture\n\nThis file is inspected, not edited.\n');
await writeFile(path.join(workspace, 'notes.txt'), 'ORIGINAL_NOTE\n');
await writeFile(path.join(workspace, 'second.txt'), 'SECOND_ORIGINAL\n');
execFileSync('git', ['init', '-q'], { cwd: workspace });
execFileSync('git', ['add', '.'], { cwd: workspace });
execFileSync('git', ['-c', 'user.email=fixture@example.test', '-c', 'user.name=Fixture', 'commit', '-qm', 'fixture'], { cwd: workspace });
await symlink(path.join(os.homedir(), '.leveler', 'config.toml'), path.join(home, 'config.toml'));

const evidence = { scope: 'real configured provider, isolated HOME via config symlink, temporary workspace', passed: false };
let app;
let page;
const errors = [];
async function settled() {
  await page.waitForFunction(() => ['已回答', '已完成', '失败', '已取消', '已中断'].includes(document.querySelector('#conversation-status')?.textContent), null, { timeout: 300000 });
}
async function allowApprovals(deadline) {
  while (Date.now() < deadline) {
    const allow = page.getByRole('button', { name: '允许一次', exact: true });
    if (await allow.count()) {
      evidence.approvalsSeen = (evidence.approvalsSeen ?? 0) + 1;
      if (!evidence.approvalSingleEntry) {
        evidence.approvalSingleEntry = await page.evaluate(() => document.querySelectorAll('#takeover .approval-card').length === 1
          && document.querySelectorAll('#conversation .approval-card').length === 0);
      }
      await allow.first().click();
    }
    const status = await page.evaluate(() => document.querySelector('#conversation-status')?.textContent);
    if (['已回答', '已完成', '失败', '已取消', '已中断'].includes(status)) return;
    await page.waitForTimeout(1500);
  }
}
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'electron')}`],
    env: {
      ...process.env,
      ELECTRON_RUN_AS_NODE: undefined,
      LEVELER_HOME: home,
      LEVELER_CONFIG_DIR: undefined,
      LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'),
      LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10',
    },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.stack ?? error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const sessionId = await page.evaluate(async () => {
    const folder = await window.desktop.chooseFolder();
    return (await window.desktop.createTask(folder.id, crypto.randomUUID())).session.id;
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), sessionId);
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  // Request approval is the mode that makes the approval path observable.
  await page.locator('#permission-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: '需批准' }).first().waitFor({ timeout: 20000 });
  await page.locator('.menu .menu-item').filter({ hasText: '需批准' }).first().click();
  await page.waitForFunction(() => document.querySelector('#permission-picker')?.textContent.includes('需批准'), null, { timeout: 20000 });
  evidence.permissionMode = await page.locator('#permission-picker').innerText();
  await page.locator('#message').fill([
    '请使用工具实际完成下面的工作，不要只在回答里描述：',
    '1) 读取 README.md。',
    '2) 运行一条会成功的命令：git status --short。',
    '3) 运行一条会失败的命令：git rev-parse --verify definitely-not-a-real-ref。',
    '4) 把 notes.txt 的内容改成：ORIGINAL_NOTE 后面追加一行 UPDATED_BY_MODEL。',
    '5) 最后回答一句，并包含 REAL_PROVIDER_PRESENTATION_DONE。',
    '只修改 notes.txt，不要修改其他文件。',
  ].join('\n'));
  await page.locator('#send').click();
  await allowApprovals(Date.now() + 300000);
  await settled();
  const turnStatus = await page.locator('#conversation-status').innerText();
  evidence.turnStatus = turnStatus;
  if (['已回答', '已完成'].includes(turnStatus)) {
    evidence.modelMarker = await page.waitForFunction(
      () => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('REAL_PROVIDER_PRESENTATION_DONE')),
      null, { timeout: 15000 },
    ).then(() => true).catch(() => false);
    evidence.finalAnswerObserved = await page.locator('.message.assistant.final').count() > 0;
    if (!evidence.modelMarker) evidence.notVerified = [...(evidence.notVerified ?? []), 'model completion marker'];
  } else {
    // The provider can legitimately end a turn incomplete or failed. The
    // conversation must still be inspected; the missing answer is recorded,
    // not hidden behind a passing claim.
    evidence.finalAnswerObserved = false;
    evidence.notVerified = [`final answer (turn ended ${turnStatus})`];
  }
  await page.screenshot({ path: path.join(output, 'real-provider-presentation.png') });

  const view = await page.evaluate(() => {
    const list = document.querySelector('#conversation');
    return {
      shape: [...list.children].map(node => {
        if (node.classList.contains('message') && node.classList.contains('user')) return 'user';
        if (node.classList.contains('message') && node.classList.contains('narration')) return 'narration';
        if (node.classList.contains('message') && node.classList.contains('final')) return 'final';
        if (node.classList.contains('execution-group')) return `execution:${node.dataset.variant}`;
        if (node.classList.contains('tool-row')) return 'tool';
        if (node.classList.contains('turn-receipt')) return 'receipt';
        return node.className;
      }),
      groups: [...list.querySelectorAll('.execution-group')].map(node => ({
        variant: node.dataset.variant,
        open: node.open,
        label: node.querySelector('.execution-label')?.textContent ?? '',
        action: node.querySelector('.execution-action')?.textContent ?? '',
        status: node.querySelector('.execution-status')?.textContent ?? '',
        failure: node.querySelector('.execution-failure')?.textContent ?? '',
        hint: node.querySelector('.execution-hint')?.textContent ?? '',
      })),
      toolRows: [...list.querySelectorAll('.tool-row')].map(node => ({
        id: node.dataset.toolId,
        status: node.dataset.status,
        text: node.querySelector('.tool-summary')?.innerText ?? '',
      })),
      receipts: [...list.querySelectorAll('.turn-receipt')].map(node => ({ status: node.dataset.status, text: node.innerText })),
      inlineDiffs: list.querySelectorAll('.confirmed-diff').length,
      answerFiles: list.querySelectorAll('.answer-file').length,
    };
  });
  evidence.view = view;
  assert.ok(view.groups.length > 0 || view.toolRows.length > 0, 'the real model produced no observable execution');
  assert.ok(view.groups.every(group => group.open === false), 'an execution group was open by default');
  assert.ok(view.receipts.length > 0, 'the turn has no terminal receipt');
  if (evidence.finalAnswerObserved) assert.ok(view.receipts.some(receipt => ['answered', 'completed', 'completed_with_warnings'].includes(receipt.status)), 'the answered turn has no answered receipt');
  else assert.ok(view.receipts.some(receipt => ['incomplete', 'failed', 'cancelled', 'no_final_answer'].includes(receipt.status)), 'a non-answered turn does not state its own terminal');
  assert.equal(view.inlineDiffs, 0, 'a full Diff was painted in the conversation');
  assert.equal(view.answerFiles, 0, 'a duplicate answer file card exists');

  async function expandAndReadRows() {
    await page.evaluate(() => { for (const group of document.querySelectorAll('.execution-group')) group.open = true; });
    await page.waitForFunction(() => document.querySelectorAll('.tool-row').length > 0, null, { timeout: 20000 });
    return page.evaluate(() => [...document.querySelectorAll('.tool-row')].map(node => ({
      id: node.dataset.toolId,
      status: node.dataset.status,
      text: node.querySelector('.tool-summary')?.innerText ?? '',
      details: node.querySelector('.tool-details')?.innerText ?? '',
    })));
  }
  let rows = await expandAndReadRows();
  evidence.expandedRows = rows;
  let failedObserved = rows.some(row => row.status === 'failed')
    || view.groups.some(group => group.status.includes('失败'));
  if (!failedObserved) {
    // The first prompt asked for a failing command; the model may have chosen
    // a succeeding one. Make the failure explicit in a dedicated turn and
    // observe the rendered result rather than trusting the answer.
    await page.locator('#message').fill('只调用一次 run_command，不要读文件、不要改文件：命令必须是 `git rev-parse --verify definitely-not-a-real-ref`，它一定会失败。执行后只回答 FAIL_COMMAND_DONE。');
    await page.locator('#send').click();
    await allowApprovals(Date.now() + 180000);
    await settled();
    await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('FAIL_COMMAND_DONE')), null, { timeout: 30000 }).catch(() => {});
    rows = await expandAndReadRows();
    evidence.expandedRowsAfterFailureTurn = rows;
    failedObserved = rows.some(row => row.status === 'failed')
      || await page.evaluate(() => [...document.querySelectorAll('.execution-group')].some(group =>
        (group.querySelector('.execution-label')?.textContent ?? '').includes('失败')
        || (group.querySelector('.execution-status')?.textContent ?? '').includes('失败')
        || (group.querySelector('.execution-failure')?.textContent ?? '') !== ''));
  }
  evidence.failedCommandObserved = failedObserved;
  if (!failedObserved) evidence.notVerified = ['controlled failing command'];
  if (evidence.approvalsSeen) evidence.approvalSingleEntry = await page.evaluate(() => document.querySelectorAll('#conversation .approval-card').length === 0);
  else evidence.notVerified = [...(evidence.notVerified ?? []), 'approval request'];

  // The edit's confirmed Diff must open in the existing Changes viewer, and
  // the workspace Diff must remain a different source.
  const edit = page.locator('.execution-group.execution-edit').last();
  if (await edit.count()) {
    await edit.locator('.execution-summary').click();
    await page.waitForFunction(() => document.querySelector('.review-scope')?.textContent.includes('已确认'), null, { timeout: 20000 });
    const confirmed = await page.evaluate(() => ({
      scope: document.querySelector('.review-scope')?.textContent ?? '',
      file: document.querySelector('.review-path')?.textContent ?? '',
      lines: document.querySelectorAll('.review-body .review-line').length,
      count: document.querySelector('.review-counts')?.textContent ?? '',
    }));
    evidence.confirmed = confirmed;
    assert.match(confirmed.scope, /历史证据/);
    assert.ok(confirmed.lines > 0, 'the confirmed change viewer painted no Diff');
    await page.screenshot({ path: path.join(output, 'real-provider-presentation-diff.png') });
    await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();
    await page.waitForFunction(() => !document.body.classList.contains('rightbar-open'));
  } else evidence.confirmed = 'NOT OBSERVED: the model produced no edit group';

  await page.locator('#message').fill('/diff');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('.review-scope')?.textContent.includes('当前工作区'), null, { timeout: 20000 });
  for (let attempt = 0; attempt < 4; attempt += 1) {
    if (await page.locator('.review-body .review-line').count()) break;
    const refresh = page.locator('.review-tools button[aria-label="重新读取工作区变更"]');
    if (await refresh.count() && await refresh.isEnabled()) await refresh.click();
    await page.waitForTimeout(1500);
  }
  await page.waitForFunction(() => document.querySelector('.review-path')?.textContent !== '选择文件'
    && document.querySelectorAll('.review-body .review-line').length > 0, null, { timeout: 30000 });
  evidence.workspace = await page.evaluate(() => ({
    scope: document.querySelector('.review-scope')?.textContent ?? '',
    file: document.querySelector('.review-path')?.textContent ?? '',
    lines: document.querySelectorAll('.review-body .review-line').length,
  }));
  assert.match(evidence.workspace.scope, /当前工作区 Git 变更/);
  assert.equal(evidence.workspace.file, 'notes.txt');
  assert.ok(evidence.workspace.lines > 0, 'the workspace viewer painted no Diff');
  await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first().click();

  const notes = await readFile(path.join(workspace, 'notes.txt'), 'utf8');
  evidence.notes = notes;
  assert.match(notes, /UPDATED_BY_MODEL/, 'the real model did not edit notes.txt');

  // Durable-history resume: same final text, no duplicated user/assistant rows.
  await page.evaluate(() => { for (const group of document.querySelectorAll('.execution-group')) group.open = false; });
  await page.waitForFunction(() => [...document.querySelectorAll('.execution-group')].every(group => !group.open));
  const before = await page.evaluate(() => {
    const clone = document.querySelector('#conversation').cloneNode(true);
    clone.querySelectorAll('.turn-usage').forEach(node => node.remove());
    return {
      users: document.querySelectorAll('.message.user').length,
      finals: document.querySelectorAll('.message.assistant.final').length,
      groups: document.querySelectorAll('.execution-group').length,
      receipts: document.querySelectorAll('.turn-receipt').length,
      record: [...document.querySelector('#conversation').children]
        .filter(node => !node.classList.contains('turn-usage'))
        .map(node => {
          if (node.classList.contains('execution-group')) return `group:${node.querySelector('.execution-summary')?.textContent.replace(/\s+/g, ' ')}`;
          if (node.classList.contains('message')) return `${node.className}:${node.textContent.trim().slice(0, 160)}`;
          if (node.classList.contains('turn-receipt')) return `receipt:${node.dataset.status}:${node.querySelector('.turn-receipt-stats')?.textContent}`;
          return node.className;
        }),
      text: clone.textContent.replace(/\s+/g, ' ').trim(),
    };
  });
  await page.reload();
  await page.waitForFunction(() => document.querySelector('.session-row[data-session-id]'));
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(({ users }) => document.querySelectorAll('.message.user').length >= users
    && document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…',
  { users: before.users }, { timeout: 60000 });
  await page.waitForFunction(({ receipts, groups }) => document.querySelectorAll('.turn-receipt').length >= receipts
    && document.querySelectorAll('.execution-group').length >= groups,
  { receipts: before.receipts, groups: before.groups }, { timeout: 60000 });
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  const after = await page.evaluate(() => {
    const clone = document.querySelector('#conversation').cloneNode(true);
    clone.querySelectorAll('.turn-usage').forEach(node => node.remove());
    return {
      users: document.querySelectorAll('.message.user').length,
      finals: document.querySelectorAll('.message.assistant.final').length,
      groups: document.querySelectorAll('.execution-group').length,
      receipts: document.querySelectorAll('.turn-receipt').length,
      record: [...document.querySelector('#conversation').children]
        .filter(node => !node.classList.contains('turn-usage'))
        .map(node => {
          if (node.classList.contains('execution-group')) return `group:${node.querySelector('.execution-summary')?.textContent.replace(/\s+/g, ' ')}`;
          if (node.classList.contains('message')) return `${node.className}:${node.textContent.trim().slice(0, 160)}`;
          if (node.classList.contains('turn-receipt')) return `receipt:${node.dataset.status}:${node.querySelector('.turn-receipt-stats')?.textContent}`;
          return node.className;
        }),
      text: clone.textContent.replace(/\s+/g, ' ').trim(),
    };
  });
  evidence.resume = {
    before: { users: before.users, finals: before.finals, groups: before.groups, receipts: before.receipts },
    after: { users: after.users, finals: after.finals, groups: after.groups, receipts: after.receipts },
    sameRecord: JSON.stringify(before.record) === JSON.stringify(after.record),
    sameText: before.text === after.text,
  };
  assert.deepEqual(
    { users: after.users, finals: after.finals, groups: after.groups, receipts: after.receipts },
    { users: before.users, finals: before.finals, groups: before.groups, receipts: before.receipts },
    'resume duplicated or dropped rows',
  );
  assert.deepEqual(after.record, before.record, 'resume changed the conversation order');
  await page.screenshot({ path: path.join(output, 'real-provider-presentation-resume.png') });

  assert.deepEqual(errors, []);
  evidence.page_errors = errors;
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.error = error.message;
  evidence.page_errors = errors;
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'real-provider-presentation-failure.png') }).catch(() => {});
  console.error(JSON.stringify(evidence, null, 2));
  throw error;
} finally {
  await writeFile(path.join(output, 'real-provider-presentation-evidence.json'), JSON.stringify(evidence, null, 2) + '\n').catch(() => {});
  if (app) await app.close().catch(() => {});
  await rm(temporary, { recursive: true, force: true }).catch(() => {});
}
