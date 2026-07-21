// 真正的 Diff 渲染器：把原始 unified diff（git_diff 输出）或 apply_patch 文档
// 解析成结构化 DiffLine（含新旧行号、marker、类型），按语义着色渲染——
// 不是普通 code block。git_diff / apply_patch 的原始结果直接进这里，模型的
// 文字只作补充解释，不参与 diff 事实的重建。

import { useState } from 'react';
import { CopyButton } from './CopyButton';

type DiffLineType = 'addition' | 'deletion' | 'context' | 'hunk' | 'file' | 'meta';

interface DiffLine {
  type: DiffLineType;
  oldLine: number | null;
  newLine: number | null;
  content: string;
}

/** 从 apply_patch 工具参数（JSON {patch} 或裸文本）取 patch 文档。 */
export function patchFromArguments(args: string): string {
  try {
    const obj = JSON.parse(args) as { patch?: unknown };
    if (typeof obj.patch === 'string') return obj.patch;
  } catch {
    // 非 JSON：可能本身就是 patch 文本
  }
  return args;
}

const HUNK = /^@@+\s*-(\d+)(?:,\d+)?\s+\+(\d+)(?:,\d+)?\s*@@/;
const APPLY_FILE = /^\*\*\* (Update|Add|Delete) File: (.+?)(?: -> (.+))?\s*$/;
const VERB: Record<string, string> = { Update: 'M', Add: 'A', Delete: 'D' };

interface Parsed {
  lines: DiffLine[];
  additions: number;
  deletions: number;
  files: string[];
}

/** 解析 unified diff 或 apply_patch 文档。行号来自 `@@ -a +b @@`；apply_patch
 *  用锚点无行号时留空。 */
export function parseDiff(src: string): Parsed {
  const lines: DiffLine[] = [];
  const files: string[] = [];
  let additions = 0;
  let deletions = 0;
  let oldNo = 0;
  let newNo = 0;

  for (const raw of src.split('\n')) {
    const line = raw.replace(/\r$/, '');

    if (line.startsWith('*** Begin Patch') || line.startsWith('*** End Patch')) continue;

    const applyFile = APPLY_FILE.exec(line);
    if (applyFile) {
      const verb = VERB[applyFile[1]] ?? 'M';
      files.push(applyFile[2]);
      oldNo = 0;
      newNo = 0;
      lines.push({
        type: 'file',
        oldLine: null,
        newLine: null,
        content: `${verb}  ${applyFile[2]}${applyFile[3] ? ` → ${applyFile[3]}` : ''}`,
      });
      continue;
    }

    // git file headers → meta (dimmed), capture path for the header.
    if (line.startsWith('diff --git ') || line.startsWith('index ')) {
      lines.push({ type: 'meta', oldLine: null, newLine: null, content: line });
      continue;
    }
    const plusFile = /^\+\+\+ [ab]\/(.+)$/.exec(line);
    if (line.startsWith('--- ') || line.startsWith('+++ ')) {
      if (plusFile) files.push(plusFile[1]);
      lines.push({ type: 'meta', oldLine: null, newLine: null, content: line });
      continue;
    }

    const hunk = HUNK.exec(line);
    if (hunk) {
      oldNo = parseInt(hunk[1], 10);
      newNo = parseInt(hunk[2], 10);
      lines.push({ type: 'hunk', oldLine: null, newLine: null, content: line });
      continue;
    }
    if (line.trim() === '@@') continue; // apply_patch 空锚点

    if (line.startsWith('+')) {
      additions += 1;
      lines.push({ type: 'addition', oldLine: null, newLine: newNo || null, content: line.slice(1) });
      if (newNo) newNo += 1;
      continue;
    }
    if (line.startsWith('-')) {
      deletions += 1;
      lines.push({ type: 'deletion', oldLine: oldNo || null, newLine: null, content: line.slice(1) });
      if (oldNo) oldNo += 1;
      continue;
    }
    // context
    const content = line.startsWith(' ') ? line.slice(1) : line;
    lines.push({ type: 'context', oldLine: oldNo || null, newLine: newNo || null, content });
    if (oldNo) oldNo += 1;
    if (newNo) newNo += 1;
  }

  return { lines, additions, deletions, files };
}

function marker(type: DiffLineType): string {
  return type === 'addition' ? '+' : type === 'deletion' ? '-' : ' ';
}

const COLLAPSED_ROWS = 20;

export function DiffBlock({ source, title }: { source: string; title?: string }) {
  const [expanded, setExpanded] = useState(false);
  const { lines, additions, deletions, files } = parseDiff(source);
  if (lines.length === 0) return null;

  const headerPath = title ?? (files.length === 1 ? files[0] : files.length > 1 ? `${files.length} 个文件` : 'diff');
  const clipped = !expanded && lines.length > COLLAPSED_ROWS;
  const shown = clipped ? lines.slice(0, COLLAPSED_ROWS) : lines;

  return (
    <div className="diffv">
      <div className="diffv-head">
        <span className="diffv-path" title={files.join(', ') || headerPath}>
          {headerPath}
        </span>
        <span className="diffv-stat add">+{additions}</span>
        <span className="diffv-stat del">−{deletions}</span>
        <CopyButton className="diffv-copy" text={source} title="复制完整 diff" />
      </div>
      <div className="diffv-body">
        {shown.map((l, i) =>
          l.type === 'hunk' || l.type === 'meta' || l.type === 'file' ? (
            <div key={i} className={`diff-line diff-line--${l.type}`}>
              <span className="diff-code diff-code--full">{l.content || ' '}</span>
            </div>
          ) : (
            <div key={i} className={`diff-line diff-line--${l.type}`}>
              <span className="diff-old-line">{l.oldLine ?? ''}</span>
              <span className="diff-new-line">{l.newLine ?? ''}</span>
              <span className="diff-marker">{marker(l.type)}</span>
              <code className="diff-code">{l.content || ' '}</code>
            </div>
          ),
        )}
      </div>
      {lines.length > COLLAPSED_ROWS && (
        <button className="diffv-toggle" onClick={() => setExpanded((v) => !v)}>
          {expanded ? '收起' : `··· 展开剩余 ${lines.length - COLLAPSED_ROWS} 行 ···`}
        </button>
      )}
    </div>
  );
}
