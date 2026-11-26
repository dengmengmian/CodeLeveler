import { test } from 'node:test';
import assert from 'node:assert/strict';
import { captureDraft, persistableDrafts, secretReferences } from '../src/secret-review.mjs';

const finding = (content, value, key = null) => {
  const start_utf16 = content.indexOf(value);
  return { start: 0, end: 0, start_utf16, end_utf16: start_utf16 + value.length, level: 'high_confidence', kind: 'ProviderKey', key, masked: `•••••• (${value.length} chars)` };
};

test('captureDraft replaces UTF-16 spans and keeps every other byte', () => {
  const content = '请写入 .env：API_KEY=sk-abcdefghijklmnop1234 然后重启';
  const value = 'sk-abcdefghijklmnop1234';
  let id = 0;
  const { content: captured, captures } = captureDraft(content, [finding(content, value, 'API_KEY')], () => `id-${++id}`);
  assert.equal(captures.length, 1);
  assert.equal(captures[0].value, value);
  assert.equal(captures[0].key_hint, 'API_KEY');
  assert.equal(captured.split(captures[0].placeholder).length - 1, 1);
  assert.equal(captured.replace(captures[0].placeholder, value), content);
  assert.doesNotMatch(captured, /sk-abcdefghijklmnop1234/);
});

test('captureDraft handles several findings in one message', () => {
  const first = 'sk-aaaaaaaaaaaaaaaa1111';
  const second = 'ghp_bbbbbbbbbbbbbbbb2222';
  const content = `one=${first} two=${second}`;
  let id = 0;
  const { content: captured, captures } = captureDraft(content, [finding(content, first, 'A'), finding(content, second, 'B')], () => `id-${++id}`);
  assert.equal(captures.length, 2);
  assert.notEqual(captures[0].placeholder, captures[1].placeholder);
  let restored = captured;
  for (const capture of captures) restored = restored.replace(capture.placeholder, capture.value);
  assert.equal(restored, content);
});

test('captureDraft refuses spans that do not belong to the message', () => {
  const content = 'plain text';
  assert.throws(() => captureDraft(content, [{ start_utf16: 2, end_utf16: 99 }], () => 'id'));
  assert.throws(() => captureDraft(content, [{ start_utf16: 4, end_utf16: 4 }], () => 'id'));
  assert.throws(() => captureDraft(content, [{ start_utf16: 0, end_utf16: 5 }, { start_utf16: 3, end_utf16: 8 }], () => 'id'));
});

test('persistableDrafts writes only text whose exact clean scan is known', () => {
  const drafts = new Map([['a', 'clean text'], ['b', 'candidate text'], ['c', 'unscanned']]);
  const clean = new Map([['a', 'clean text'], ['b', 'older text']]);
  assert.deepEqual(persistableDrafts(drafts, clean), { a: 'clean text' });
});

test('secretReferences finds each captured reference once', () => {
  const token = `[secret:sec_${'a'.repeat(32)}]`;
  const other = `[secret:sec_${'b'.repeat(32)}]`;
  assert.deepEqual(secretReferences(`x ${token} ${token} ${other} [secret:sec_bad]`), [`sec_${'a'.repeat(32)}`, `sec_${'b'.repeat(32)}`]);
  assert.deepEqual(secretReferences('no tokens'), []);
});
