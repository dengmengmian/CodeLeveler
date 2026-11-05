// The desktop renderer is served through a protocol allowlist. A local module
// missing from that list makes the whole UI blank, so the list is checked
// against the renderer's real imports instead of being maintained by hand.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(fileURLToPath(new URL('..', import.meta.url)));

test('every local module imported by the renderer is served by the desktop protocol', () => {
  const renderer = readFileSync(path.join(root, 'src/renderer.mjs'), 'utf8');
  const main = readFileSync(path.join(root, 'src/main.cjs'), 'utf8');
  const allowlist = /const assets=new Set\(\[([^\]]*)\]\)/.exec(main);
  assert.ok(allowlist, 'desktop asset allowlist not found');
  const assets = new Set([...allowlist[1].matchAll(/'([^']+)'/g)].map(match => match[1]));
  const imports = [...renderer.matchAll(/from '\.\/([^']+\.mjs)'/g)].map(match => match[1]);
  assert.ok(imports.includes('schedules.mjs'));
  for (const name of imports) assert.ok(assets.has(name), `${name} must be in the desktop asset allowlist`);
});
