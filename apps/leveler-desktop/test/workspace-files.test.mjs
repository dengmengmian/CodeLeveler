import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, symlink, rm, realpath } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { WorkspaceFiles, MAX_READ_BYTES } = require('../src/workspace-files.cjs');

async function fixture() {
  const temporary = await mkdtemp(path.join(os.tmpdir(), 'leveler-files-'));
  const root = path.join(temporary, 'workspace');
  await mkdir(root);
  await mkdir(path.join(root, 'src', 'nested'), { recursive: true });
  await mkdir(path.join(root, 'empty'));
  await writeFile(path.join(root, 'README.md'), '# 标题\n\n真实内容\n');
  await writeFile(path.join(root, 'src', 'main.js'), 'export const value = 1;\n');
  await writeFile(path.join(root, 'src', 'nested', '中文 文件名.txt'), '含空格和中文\n');
  await writeFile(path.join(root, 'binary.bin'), Buffer.from([0, 1, 2, 3]));
  await writeFile(path.join(root, 'large.txt'), 'x'.repeat(MAX_READ_BYTES + 32));
  const outside = path.join(temporary, 'outside.txt');
  await writeFile(outside, 'outside\n');
  await symlink(outside, path.join(root, 'escape-link'));
  await symlink(path.join(root, 'src'), path.join(root, 'inside-link'));
  return { temporary, root: await realpath(root), outside };
}

test('workspace files list directories first and read real UTF-8 content', async () => {
  const { temporary, root } = await fixture();
  try {
    const files = new WorkspaceFiles({ root });
    const listing = await files.list('');
    const names = listing.entries.map(entry => entry.name);
    assert.ok(names.includes('README.md'));
        assert.equal(listing.entries[0].kind, 'directory');
    assert.equal(listing.entries.find(entry => entry.name === 'README.md').kind, 'file');
    assert.equal(listing.entries.find(entry => entry.name === 'empty').kind, 'directory');
    const nested = await files.list('src');
    assert.deepEqual(nested.entries.map(entry => entry.name), ['nested', 'main.js']);
    const read = await files.read('src/nested/中文 文件名.txt');
    assert.equal(read.text, '含空格和中文\n');
    assert.equal(read.truncated, false);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
});

test('workspace files reject traversal, absolute escapes and external symlinks', async () => {
  const { temporary, root, outside } = await fixture();
  try {
    const files = new WorkspaceFiles({ root });
    await assert.rejects(() => files.list('../'), /路径超出工作区/);
    await assert.rejects(() => files.read('../../etc/passwd'), /路径超出工作区/);
    await assert.rejects(() => files.read(outside), /路径超出工作区/);
    await assert.rejects(() => files.read('escape-link'), /路径超出工作区/);
    const listing = await files.list('');
    const escape = listing.entries.find(entry => entry.name === 'escape-link');
    assert.equal(escape.kind, 'file');
    assert.equal(escape.external, true);
    const inside = listing.entries.find(entry => entry.name === 'inside-link');
    assert.equal(inside.external, false);
    const viaLink = await files.list('inside-link');
    assert.ok(viaLink.entries.some(entry => entry.name === 'main.js'));
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
});

test('workspace files cap large reads and refuse binary content', async () => {
  const { temporary, root } = await fixture();
  try {
    const files = new WorkspaceFiles({ root });
    const large = await files.read('large.txt');
    assert.equal(large.truncated, true);
    assert.equal(large.size, MAX_READ_BYTES + 32);
    assert.equal(large.text.length, MAX_READ_BYTES);
    await assert.rejects(() => files.read('binary.bin'), /二进制文件/);
    await assert.rejects(() => files.list('README.md'), /不是目录/);
    await assert.rejects(() => files.read('empty'), /不是普通文件/);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
});
