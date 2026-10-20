'use strict';
// Read-only Workspace browsing for the desktop Files pane.
//
// The Main process owns the root: it comes from a Runtime snapshot, never from
// the renderer. Every request is resolved against that canonical root and then
// re-checked after symlink resolution, so a link cannot widen the boundary.
const { promises: fsp } = require('node:fs');
const path = require('node:path');

const MAX_ENTRIES = 500;
const MAX_READ_BYTES = 1024 * 1024;
const MAX_PATH_LENGTH = 4096;

class WorkspaceFiles {
  constructor({ root }) {
    if (typeof root !== 'string' || !root) throw new Error('当前会话没有可读取的工作区');
    this.root = root;
  }

  /** Reject absolute-outside and traversal paths before touching the disk. */
  candidate(relative) {
    if (typeof relative !== 'string' || relative.length > MAX_PATH_LENGTH || relative.includes('\0')) {
      throw new Error('无效的文件路径');
    }
    const resolved = path.resolve(this.root, relative);
    this.assertInside(resolved);
    return resolved;
  }

  assertInside(candidate) {
    const relative = path.relative(this.root, candidate);
    if (relative === '..' || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
      throw new Error('路径超出工作区');
    }
  }

  /** Resolve the real path (following links) and enforce the same boundary. */
  async realEntry(relative) {
    const candidate = this.candidate(relative);
    let real;
    try {
      real = await fsp.realpath(candidate);
    } catch {
      throw new Error('文件或目录不存在');
    }
    this.assertInside(real);
    return { absolute: real, relative: path.relative(this.root, real) };
  }

  async list(relative = '') {
    const entry = await this.realEntry(relative);
    const stat = await fsp.stat(entry.absolute);
    if (!stat.isDirectory()) throw new Error('不是目录');
    const names = await fsp.readdir(entry.absolute, { withFileTypes: true });
    const entries = [];
    for (const name of names.slice(0, MAX_ENTRIES)) {
      const absolute = path.join(entry.absolute, name.name);
      const child = path.join(entry.relative, name.name);
      let kind = name.isDirectory() ? 'directory' : 'file';
      let external = false;
      let symlink = name.isSymbolicLink();
      if (symlink) {
        try {
          const real = await fsp.realpath(absolute);
          const rel = path.relative(this.root, real);
          external = rel === '..' || rel.startsWith(`..${path.sep}`) || path.isAbsolute(rel);
          if (!external) kind = (await fsp.stat(real)).isDirectory() ? 'directory' : 'file';
        } catch {
          external = true;
        }
      }
      entries.push({ name: name.name, path: child, kind, symlink, external });
    }
    entries.sort((left, right) => {
      if (left.kind !== right.kind) return left.kind === 'directory' ? -1 : 1;
      return left.name.localeCompare(right.name, 'zh-CN');
    });
    return { path: entry.relative, entries, truncated: names.length > MAX_ENTRIES };
  }

  async read(relative) {
    const entry = await this.realEntry(relative);
    const stat = await fsp.stat(entry.absolute);
    if (!stat.isFile()) throw new Error('不是普通文件');
    const length = Math.min(stat.size, MAX_READ_BYTES);
    const handle = await fsp.open(entry.absolute, 'r');
    try {
      const buffer = Buffer.alloc(length);
      const { bytesRead } = await handle.read(buffer, 0, length, 0);
      const content = buffer.subarray(0, bytesRead);
      if (content.includes(0)) throw new Error('二进制文件暂不支持预览');
      const truncated = stat.size > MAX_READ_BYTES;
      const decoder = new TextDecoder('utf-8', { fatal: true });
      let text;
      try {
        text = decoder.decode(content);
      } catch {
        if (!truncated) throw new Error('文件不是有效的 UTF-8 文本');
        // A byte limit can split a multi-byte character; drop at most the
        // three trailing bytes before declaring the file unreadable.
        let trimmed = content;
        for (let attempt = 0; attempt < 3 && text === undefined; attempt += 1) {
          trimmed = trimmed.subarray(0, trimmed.length - 1);
          try { text = decoder.decode(trimmed); } catch { /* keep trimming */ }
        }
        if (text === undefined) throw new Error('文件不是有效的 UTF-8 文本');
      }
      return {
        path: entry.relative,
        size: stat.size,
        mtimeMs: stat.mtimeMs,
        text,
        truncated,
      };
    } finally {
      await handle.close();
    }
  }
}

module.exports = { WorkspaceFiles, MAX_ENTRIES, MAX_READ_BYTES };
