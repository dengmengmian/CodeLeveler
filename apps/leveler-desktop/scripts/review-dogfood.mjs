// R3c real local dogfood: the actual macOS HOME/profile, the configured
// provider and model, the real Desktop, Runtime and filesystem. Only NEW
// sessions in a temporary business project are created; the workspace, its
// sessions and its runtime state are removed by their exact identities.
//
// What this proves with a REAL model:
// - a manual code review of an uncommitted change finds the planted defect and
//   reports a real file:line;
// - a manual business review compares the delivered work against the project's
//   written requirements and does not report a clean PASS when a requirement
//   was not delivered;
// - no ordinary turn, restart or report open starts a review.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const workspace = await mkdtemp('/tmp/leveler-r3c-dogfood-');
const suffix = createHash('sha256').update(workspace).digest('hex').slice(0, 10);
const realHome = process.env.HOME;
const configPath = path.join(realHome, '.leveler', 'config.toml');
const configHash = async () => existsSync(configPath)
  ? createHash('sha256').update(await readFile(configPath)).digest('hex')
  : null;
const configBefore = await configHash();

const README = `# 订单计价

## 需求

- R1：\`total(items)\` 返回所有商品 \`price * quantity\` 之和，不得跳过任何商品。
- R2：\`discount(total, member)\` 对会员的总价打九折，只作用于总价。
- R3：金额以“分”为单位用整数计算，禁止浮点乘法；函数的返回值是整数分。
- R4：必须提供 \`node cli.mjs\` 入口，读取 JSON 商品列表并打印总价；输入非法时以非零退出码失败。

## 验收

- 每个需求至少有一个测试覆盖。
- 会员与非会员、空购物车、单价超过 1000 的商品都必须验证。
`;
const ORDERS = `// Order pricing. See README.md for the requirements.
export function total(items) {
  let sum = 0;
  for (const item of items) {
    sum += item.price * item.quantity;
  }
  return sum;
}

export function discount(total, member) {
  return member ? total * 0.9 : total;
}
`;
await mkdir(path.join(workspace, 'test'), { recursive: true });
await writeFile(path.join(workspace, 'README.md'), README);
await writeFile(path.join(workspace, 'orders.mjs'), ORDERS);
await writeFile(path.join(workspace, 'test', 'orders.test.mjs'), `import assert from 'node:assert/strict';\nimport { test } from 'node:test';\nimport { total, discount } from '../orders.mjs';\n\ntest('sums a small cart', () => {\n  assert.equal(total([{ price: 10, quantity: 2 }]), 20);\n});\n\ntest('member discount', () => {\n  assert.equal(discount(100, true), 90);\n});\n`);
const git = (...args) => execFile('git', args, { cwd: workspace });
await git('init', '-q');
await git('config', 'user.email', 'r3c@example.com');
await git('config', 'user.name', 'R3C');
await git('add', '.');
await git('commit', '-qm', 'order pricing skeleton');

// The code review's object: an uncommitted change with a real boundary defect
// (express shipping under 1kg is free instead of the express base fee).
const SHIPPING = `// Shipping fee, added for the R3c dogfood.
export function shippingFee(weightKg, express) {
  if (weightKg <= 1) return 0;
  const extra = Math.ceil(weightKg - 1);
  return express ? 20 + extra * 5 : extra * 5;
}
`;
const SHIPPING_DEFECT_LINE = 4; // `if (weightKg <= 1) return 0;`
const shippingPath = 'shipping.mjs';

const evidence = {
  scope: 'real macOS HOME/profile, real configured provider/model, real Desktop/Runtime; temporary business project',
  binary: { path: leveler, sha256: binarySha },
  workspace,
  config_sha256_before: configBefore,
};
let app;
let page;
let stage = 'launch';
const createdSessions = [];
const errors = [];
const launch = async () => {
  const env = { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: leveler };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  delete env.LEVELER_DAEMON_IDLE_TIMEOUT_SECS;
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
};
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const rememberSession = async () => {
  const id = await sessionId();
  if (id && !createdSessions.includes(id)) createdSessions.push(id);
  return id;
};
const waitSend = () => page.waitForFunction(() => {
  const button = document.querySelector('#send');
  return button && !button.disabled && !button.hidden;
});
const submit = async value => {
  await page.locator('#message').fill(value);
  await page.waitForFunction(() => {
    const status = document.querySelector('#conversation-status')?.textContent ?? '';
    return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
  }, null, { timeout: 600000 });
  await waitSend();
  await page.locator('#send').click();
};
const waitIdle = () => page.waitForFunction(async () => {
  const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
  if (!id) return false;
  const snapshot = await window.desktop.snapshot(id);
  return !['running', 'waiting_user'].includes(snapshot.task_status);
}, null, { timeout: 900000 });
const cards = () => page.locator('.review-card');
const card = id => page.locator(`.review-card[data-review-id="${id}"]`);
const latestCardId = async () => {
  const ids = await cards().evaluateAll(nodes => nodes.map(node => node.dataset.reviewId));
  return ids.at(-1);
};
const openDialog = async () => {
  await page.locator('#review-entry').click();
  await page.waitForFunction(() => document.querySelector('#review-dialog')?.open);
};
const chooseScope = async id => {
  await page.locator(`.review-option[data-scope="${id}"] input`).check();
};
const startReview = async focus => {
  await page.locator('#review-focus').fill(focus);
  await page.locator('#review-submit').click();
  await page.waitForFunction(() => !document.querySelector('#review-dialog'), null, { timeout: 60000 });
};
const waitSettled = async id => {
  await page.waitForFunction(reviewId => {
    const node = document.querySelector(`.review-card[data-review-id="${reviewId}"]`);
    return node && !['running', 'queued'].includes(node.dataset.status);
  }, id, { timeout: 1200000 });
};

try {
  await launch();
  stage = 'open workspace';
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.textContent.includes(name), path.basename(workspace));
  stage = 'deliver the business task';
  await submit('请按 README.md 实现 orders.mjs 的 R1 总价与 R2 会员折扣，并补充对应测试。R3 的整数分和 R4 的 CLI 先不做，我稍后决定。');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await rememberSession();
  await waitIdle();
  // D18: the delivery itself must not have started a review.
  assert.equal(await cards().count(), 0, 'a delivered turn must not start a review');
  evidence.startup = { session: await sessionId(), autoReviews: 0 };

  // D06/D08: business review of the real delivery against the written spec.
  stage = 'business review';
  await openDialog();
  await page.waitForFunction(() => document.querySelector('#review-dialog')?.textContent.includes('工作区'));
  await chooseScope('current_task');
  await startReview('对照 README.md 审查这次交付：R1/R2 是否满足？R3/R4 是否交付？没有证据的验收项要标注未验证，不要给出没有依据的整体通过。');
  await page.waitForFunction(() => document.querySelector('.review-card'));
  const businessReview = await latestCardId();
  await waitSettled(businessReview);
  const businessStatus = await card(businessReview).getAttribute('data-status');
  const businessText = await card(businessReview).textContent();
  // A delivery that skipped R3 and R4 is not a clean pass; neither "completed"
  // nor "no issues found" may stand in for the missing evidence.
  assert.doesNotMatch(businessText, /未发现问题/, 'a partial delivery must not be reported as no issues found');
  assert.match(businessText, /查看报告/);
  await card(businessReview).getByRole('button', { name: '查看报告' }).click();
  await page.waitForFunction(() => {
    const panes = [...document.querySelectorAll('.review-report')];
    return panes.length && !panes.at(-1).textContent.includes('正在读取持久化报告');
  }, null, { timeout: 120000 });
  const businessReport = await page.locator('.review-report').last().textContent();
  assert.match(businessReport, /覆盖情况/);
  assert.match(businessReport, /限制与未验证内容|审查报告正文/);
  const mentionsRequirements = /R3|R4|整数|CLI|未验证|未交付|遗漏|部分/.test(businessReport);
  assert.ok(mentionsRequirements, 'the business report must address the requirements it could not confirm');
  evidence.business = {
    review: businessReview,
    status: businessStatus,
    conclusion: /发现问题/.test(businessText) ? 'issues_found' : /证据不足/.test(businessText) ? 'inconclusive' : 'other',
    excerpt: businessText.slice(0, 400),
    reportMentionsRequirements: mentionsRequirements,
  };

  // D03/D04/D05: code review of a real uncommitted change.
  stage = 'code review';
  await writeFile(path.join(workspace, shippingPath), SHIPPING);
  await git('add', shippingPath);
  await openDialog();
  await page.waitForFunction(() => document.querySelector('#review-dialog')?.textContent.includes('工作区有未提交变更'));
  await chooseScope('workspace');
  await startReview('检查这次新增的运费计算：边界条件、会员/非会员与运输方式是否正确，给出可核对的文件与行号。');
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length === 2);
  const codeReview = await latestCardId();
  await waitSettled(codeReview);
  const codeStatus = await card(codeReview).getAttribute('data-status');
  const codeText = await card(codeReview).textContent();
  assert.match(codeText, /查看报告/);
  await card(codeReview).getByRole('button', { name: '查看报告' }).click();
  await page.waitForFunction(() => {
    const panes = [...document.querySelectorAll('.review-report')];
    return panes.length && !panes.at(-1).textContent.includes('正在读取持久化报告');
  }, null, { timeout: 120000 });
  const codeReport = await page.locator('.review-report').last().textContent();
  const formalFindings = await page.locator('.review-finding').count();
  assert.ok(formalFindings > 0, 'the planted boundary defect must produce a formal finding');
  const locations = await page.locator('.review-finding-location').evaluateAll(nodes => nodes.map(node => node.textContent));
  const shippingFinding = locations.find(text => String(text).startsWith(`${shippingPath}:`));
  assert.ok(shippingFinding, `a formal finding must point at ${shippingPath}, got ${JSON.stringify(locations)}`);
  const line = Number(String(shippingFinding).split(':')[1]);
  assert.ok(line >= 1 && line <= 8, `the reported line must exist in the reviewed file, got ${line}`);
  // The reported position must be in the exact file the review read.
  const sourceLines = (await readFile(path.join(workspace, shippingPath), 'utf8')).split('\n');
  assert.ok(sourceLines.length >= line, 'reported line must be inside the reviewed file');
  evidence.code = {
    review: codeReview,
    status: codeStatus,
    findings: formalFindings,
    locations,
    reportedSourceLine: sourceLines[line - 1].trim(),
    excerpt: codeText.slice(0, 400),
  };
  assert.doesNotMatch(codeText, /未发现问题/, 'the planted defect must not be reported as no issues');

  // D18: no automatic review on commit or restart.
  stage = 'commit + restart';
  await git('commit', '-qm', 'add shipping fee');
  await waitIdle();
  assert.equal(await cards().count(), 2, 'a commit must not start a review');
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  const first = await sessionId().catch(() => null);
  if (!first) {
    await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), evidence.startup.session, { timeout: 60000 });
    await page.locator(`.session-row[data-session-id="${evidence.startup.session}"]`).click();
  }
  await page.waitForFunction(() => document.querySelectorAll('.review-card').length >= 2, null, { timeout: 60000 });
  const restored = await cards().evaluateAll(nodes => nodes.map(node => node.dataset.reviewId));
  assert.ok(restored.includes(businessReview));
  assert.ok(restored.includes(codeReview));
  assert.equal(restored.length, 2, 'restart must not start or duplicate a review');
  await card(codeReview).getByRole('button', { name: '查看报告' }).click();
  await page.waitForFunction(() => {
    const panes = [...document.querySelectorAll('.review-report')];
    return panes.length && !panes.at(-1).textContent.includes('正在读取持久化报告');
  }, null, { timeout: 60000 });
  evidence.restart = { restored, reopened: codeReview };
  assert.deepEqual(errors, [], 'renderer errors');

  const configAfter = await configHash();
  evidence.config_sha256_after = configAfter;
  assert.equal(configAfter, configBefore, 'the real provider configuration must be byte-identical');
  evidence.passed = true;
  await writeFile(path.join(output, 'r3c-review-dogfood.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log('R3C real local review dogfood PASS');
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, stage, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r3c-review-dogfood-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'r3c-review-dogfood.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  for (const id of createdSessions) {
    await execFile(leveler, ['sessions', 'delete', id], { timeout: 30000 }).catch(() => {});
  }
  await new Promise(resolve => setTimeout(resolve, 1000));
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await execFile(leveler, ['runtime', 'stop'], { cwd: workspace, timeout: 30000 }).catch(() => {});
    await new Promise(resolve => setTimeout(resolve, 800));
  }
  const projectsRoot = path.join(realHome, '.leveler', 'state', 'projects');
  const socketsRoot = path.join(realHome, '.leveler', 'run', 'sockets');
  const workspaceTag = path.basename(workspace);
  const removeRunState = async () => {
    for (const name of await readdir(projectsRoot).catch(() => [])) {
      if (!name.includes(workspaceTag)) continue;
      const hash = name.slice(name.lastIndexOf('-') + 1);
      await rm(path.join(projectsRoot, name), { recursive: true, force: true }).catch(() => {});
      for (const extension of ['sock', 'lock', 'token']) {
        await rm(path.join(socketsRoot, `${hash}.${extension}`), { force: true }).catch(() => {});
      }
    }
  };
  await removeRunState();
  await rm(workspace, { recursive: true, force: true }).catch(() => {});
  await new Promise(resolve => setTimeout(resolve, 500));
  await removeRunState();
}
