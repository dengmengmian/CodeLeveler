// Infer add/delete/modify from existing UiDiffFile (no protocol status field).

import type { UiDiffFile } from '../types/protocol';

export type ChangeKind = 'added' | 'deleted' | 'modified';

export function inferChangeKind(file: UiDiffFile): ChangeKind {
  const patch = file.patch ?? '';
  if (/^--- \/dev\/null/m.test(patch)) return 'added';
  if (/^\+\+\+ \/dev\/null/m.test(patch)) return 'deleted';
  if (!patch) {
    if (file.removed === 0 && file.added > 0) return 'added';
    if (file.added === 0 && file.removed > 0) return 'deleted';
  }
  return 'modified';
}

export function groupChangeFiles(files: readonly UiDiffFile[]): {
  added: UiDiffFile[];
  deleted: UiDiffFile[];
  modified: UiDiffFile[];
} {
  const added: UiDiffFile[] = [];
  const deleted: UiDiffFile[] = [];
  const modified: UiDiffFile[] = [];
  for (const f of files) {
    const kind = inferChangeKind(f);
    if (kind === 'added') added.push(f);
    else if (kind === 'deleted') deleted.push(f);
    else modified.push(f);
  }
  return { added, deleted, modified };
}

/** Exact path in the current diff, or a unique suffix. Never guesses. */
export function linkedChangePath(detail: string, files: readonly string[]): string | null {
  const d = detail.trim();
  if (!d) return null;
  if (files.includes(d)) return d;
  const hits = files.filter((p) => p.endsWith(`/${d}`));
  return hits.length === 1 ? hits[0] : null;
}

export function artifactTotals(files: readonly UiDiffFile[]): {
  files: number;
  added: number;
  removed: number;
} {
  return {
    files: files.length,
    added: files.reduce((n, f) => n + f.added, 0),
    removed: files.reduce((n, f) => n + f.removed, 0),
  };
}
