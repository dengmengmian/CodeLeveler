// Permanent regression: the analysis rows follow the TUI's information order —
// completed analysis, then the answer text, then the live analysis, then the
// bottom running line. Synthetic JSONL bridge; real Electron renderer.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, chmod } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-thought-');
const wrapper = path.join(temporary, 'bridge');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
await writeFile(wrapper, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(path.join(root, 'test/fixtures/thought-layout.cjs'))}\n`);
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
  page.on('pageerror', error => errors.push(error.message));
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1100, 900));
  await page.waitForFunction(() => document.querySelector('.session-row[data-session-id="thoughts"]'));
  await page.locator('.session-row[data-session-id="thoughts"]').click();
  await page.waitForFunction(() => document.querySelector('.thought-row.thought-live .thought-preview')?.textContent.includes('The user repeated'));
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const view = await page.evaluate(() => {
    const list = document.querySelector('#conversation');
    const markers = [...list.querySelectorAll('.thought-row, .message.assistant, .running-row')]
      .map(node => ({ cls: node.className, text: (node.textContent || '').trim().slice(0, 60) }));
    const children = [...list.children].map(node => node.className);
    const completed = list.querySelector('.thought-row:not(.thought-live)');
    const live = list.querySelector('.thought-row.thought-live');
    const narration = list.querySelector('.message.assistant.narration');
    return {
      markers,
      completedRows: list.querySelectorAll('.thought-row:not(.thought-live)').length,
      liveLabel: live?.querySelector('.thought-label')?.textContent ?? null,
      livePreview: live?.querySelector('.thought-preview')?.textContent ?? null,
      liveHasBody: !!live?.querySelector('.thought-body'),
      narrationText: narration?.textContent?.trim() ?? null,
      headings: [...list.querySelectorAll('.process-heading, .process-group')].length,
      last: children.at(-1) ?? null,
      beforeLast: children.at(-2) ?? null,
      runningText: list.querySelector('.running-row .running-text')?.textContent ?? null,
      runningBorder: getComputedStyle(list.querySelector('.running-row')).borderTopWidth,
      stop: (() => {
        const button = document.querySelector('#stop');
        const glyph = button?.querySelector('.stop-glyph');
        if (!button || !glyph) return null;
        const style = getComputedStyle(glyph);
        return {
          hidden: button.hidden,
          width: parseFloat(style.width),
          height: parseFloat(style.height),
          radius: parseFloat(style.borderRadius),
          background: style.backgroundColor,
          svgChildren: button.querySelectorAll('svg').length,
        };
      })(),
    };
  });
  evidence.view = view;
  assert.equal(view.completedRows, 0, 'a settled Thought still occupies a conversation row');
  assert.equal(view.headings, 0, 'the removed work-steps wrapper is still rendered');
  assert.match(view.narrationText ?? '', /好，我把运营端 MCP 验证固化成回归用例/, 'interim narration is not painted as prose');
  assert.equal(view.liveLabel, '正在分析请求');
  assert.match(view.livePreview, /The user repeated the same question/, 'live analysis does not show the latest reasoning line');
  assert.equal(view.liveHasBody, false, 'live analysis renders its full body');
  assert.match(view.runningText ?? '', /git fetch --all --prune/, 'the running line does not name the command that is executing');
  assert.notEqual(view.runningBorder, '0px', 'the running line has no divider above it');
  assert.ok(view.stop && !view.stop.hidden, 'the running turn exposes no stop control');
  assert.ok(view.stop.width >= 8 && view.stop.height >= 8, `the stop glyph is too small: ${JSON.stringify(view.stop)}`);
  assert.ok(view.stop.radius > 0, 'the stop glyph is not a rounded block');
  assert.equal(view.stop.svgChildren, 0, 'the stop control still draws an outlined icon');
  const order = view.markers.map(marker => marker.cls);
  const answerAt = order.findIndex(cls => cls.includes('message assistant'));
  const liveAt = order.findIndex(cls => cls.includes('thought-live'));
  const runningAt = order.findIndex(cls => cls.includes('running-row'));
  assert.ok(answerAt >= 0, `narration is missing: ${JSON.stringify(order)}`);
  assert.ok(liveAt > answerAt, `live analysis is not after the narration: ${JSON.stringify(order)}`);
  assert.equal(runningAt, order.length - 1, `the running line is not last: ${JSON.stringify(order)}`);
  assert.equal(liveAt, runningAt - 1, `live analysis is not directly above the running line: ${JSON.stringify(order)}`);
  assert.equal(view.last, 'running-row');
  assert.ok(view.beforeLast.includes('thought-live'), `live analysis is not directly above the running line in the child list: ${JSON.stringify(view)}`);
  await page.locator('#conversation').screenshot({ path: path.join(output, 'analysis-order.png') });
  assert.deepEqual(errors, []);
  evidence.passed = true;
  await writeFile(path.join(output, 'thought-layout-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.error = error.message;
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'thought-layout-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'thought-layout-evidence.json'), JSON.stringify({ ...evidence, page_errors: errors }, null, 2) + '\n');
  throw error;
} finally {
  if (application) await application.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
