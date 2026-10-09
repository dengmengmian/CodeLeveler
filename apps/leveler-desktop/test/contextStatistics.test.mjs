// Cross-surface contract for context statistics — desktop conformance.
//
// The desktop context page is part of the single-file renderer, which installs
// its DOM at import time and therefore cannot be imported under `node --test`.
// Its contract is pinned two ways instead:
//
//   1. the page must read exactly the runtime `ContextAccounting` fields the
//      shared corpus carries and must never derive its own percentage;
//   2. the corpus itself must stay internally consistent with the semantics
//      every surface renders (projected input over the effective input
//      capacity, the runtime's soft/hard numbers, the classifier's fold state).
//
// The live/serve checks in `test/presentation.test.mjs` etc. still exercise the
// renderer in a real DOM; this file only guards the statistics contract.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const desktopRoot = fileURLToPath(new URL('..', import.meta.url));
const repoRoot = fileURLToPath(new URL('../../..', import.meta.url));
const corpusDir = path.join(repoRoot, 'testdata', 'context_statistics', 'v1');

const renderer = readFileSync(path.join(desktopRoot, 'src', 'renderer.mjs'), 'utf8');

/** The `settingsSection==='context'` branch of `renderSettingsRead`. */
function contextBlock() {
  const start = renderer.indexOf("settingsSection==='context'");
  assert.ok(start >= 0, 'the desktop context page no longer exists');
  const end = renderer.indexOf('settingsSection===', start + 1);
  assert.ok(end > start, 'cannot delimit the desktop context page');
  return renderer.slice(start, end);
}

function cases() {
  const files = readdirSync(corpusDir)
    .filter((file) => file.endsWith('.json'))
    .sort();
  assert.ok(files.length > 0, 'the context statistics corpus is empty');
  return files.map((file) => {
    const raw = readFileSync(path.join(corpusDir, file), 'utf8');
    return JSON.parse(raw);
  });
}

test('the desktop context page reads the runtime accounting, field by field', () => {
  const block = contextBlock();
  for (const field of [
    'used_tokens',
    'input_capacity_tokens',
    'compact_at_tokens',
    'fold_state',
    'context_window_tokens',
    'output_reservation_tokens',
    'headroom_tokens',
  ]) {
    assert.ok(block.includes(field), `the context page stopped reading ${field}`);
  }
});

test('the desktop never derives a compaction percentage from the model window', () => {
  const block = contextBlock();
  // No arithmetic over used/window: both axes are printed as raw runtime facts.
  assert.ok(!/used_tokens\s*[*/]/.test(block), 'compaction must not be computed in the UI');
  assert.ok(
    !/context_window_tokens\s*[*/]/.test(block),
    'the window must not become a computed denominator',
  );
  // The window is explicitly the display-only axis.
  assert.ok(block.includes('模型窗口（仅展示口径）'), 'the window axis must be named as such');
  assert.ok(block.includes('有效输入容量（压缩口径）'), 'the compaction axis must be named as such');
});

test('the shared corpus pins the semantics the desktop consumes', () => {
  for (const c of cases()) {
    const a = c.accounting;
    if (c.expect.axis === 'compaction') {
      assert.ok(a, `${c.id}: a compaction case needs an accounting`);
      assert.equal(a.used_tokens, c.expect.used_tokens, `${c.id}: projected input`);
      assert.equal(
        a.input_capacity_tokens,
        c.expect.input_capacity_tokens,
        `${c.id}: effective input capacity`,
      );
      assert.equal(
        a.input_capacity_tokens,
        c.expect.hard_capacity,
        `${c.id}: hard capacity is the effective capacity`,
      );
      assert.equal(a.compact_at_tokens, c.expect.compact_at_tokens, `${c.id}: soft threshold`);
      assert.equal(a.fold_state, c.expect.fold_state, `${c.id}: classifier state`);
    } else {
      assert.ok(
        a === null || a.input_capacity_tokens === null,
        `${c.id}: a non-compaction case resolves no capacity`,
      );
    }
  }
});
