// Diff 审阅工作区：文件导航 + 带行号的 unified patch。不引入第二套路由。

export type PatchKind = 'add' | 'del' | 'ctx' | 'hunk' | 'meta';

export interface PatchLine {
  kind: PatchKind;
  oldNo: number | null;
  newNo: number | null;
  text: string;
}

const COLLAPSE_AT = 400;

export function shouldCollapsePatch(lineCount: number): boolean {
  return lineCount >= COLLAPSE_AT;
}

export function nextPath(files: readonly string[], current: string | null): string | null {
  if (files.length === 0) return null;
  const i = current ? files.indexOf(current) : -1;
  return files[(i + 1) % files.length] ?? null;
}

export function prevPath(files: readonly string[], current: string | null): string | null {
  if (files.length === 0) return null;
  const i = current ? files.indexOf(current) : 0;
  return files[(i - 1 + files.length) % files.length] ?? null;
}

export function numberPatch(patch: string): PatchLine[] {
  const out: PatchLine[] = [];
  let oldNo = 0;
  let newNo = 0;
  for (const raw of patch.split('\n')) {
    if (raw.startsWith('@@')) {
      const m = /@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(raw);
      if (m) {
        oldNo = Number(m[1]);
        newNo = Number(m[2]);
      }
      out.push({ kind: 'hunk', oldNo: null, newNo: null, text: raw });
      continue;
    }
    if (
      raw.startsWith('diff ') ||
      raw.startsWith('index ') ||
      raw.startsWith('--- ') ||
      raw.startsWith('+++ ') ||
      raw.startsWith('new file') ||
      raw.startsWith('deleted file')
    ) {
      out.push({ kind: 'meta', oldNo: null, newNo: null, text: raw });
      continue;
    }
    if (raw.startsWith('+')) {
      out.push({ kind: 'add', oldNo: null, newNo: newNo++, text: raw });
      continue;
    }
    if (raw.startsWith('-')) {
      out.push({ kind: 'del', oldNo: oldNo++, newNo: null, text: raw });
      continue;
    }
    out.push({ kind: 'ctx', oldNo: oldNo++, newNo: newNo++, text: raw });
  }
  return out;
}
