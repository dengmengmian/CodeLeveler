// 工具调用行：台阶轨样式。文件编辑（apply_patch）和 git_diff 直接把原始
// diff 渲染成真正的 DiffViewer；其余工具有 preview 时可展开纯文本输出。

import { useState } from 'react';
import type { ToolCallView } from '../state/store';
import { formatDuration, toolSummary } from '../lib/format';
import { DiffBlock, patchFromArguments } from './DiffBlock';

const GLYPH: Record<ToolCallView['status'], string> = {
  done: '✓',
  run: '◍',
  fail: '✗',
};

export function ToolCallRow({ tool }: { tool: ToolCallView }) {
  const [open, setOpen] = useState(false);
  const { verb, main } = toolSummary(tool.name, tool.arguments);

  // 原始 diff 直接进 DiffViewer（事实由原始 diff 负责，不靠模型重述）：
  // apply_patch 的 patch 在参数里，git_diff 的 unified diff 在 preview 里。
  const editDiff =
    tool.name === 'apply_patch' && tool.status !== 'fail' ? patchFromArguments(tool.arguments) : null;
  const gitDiff =
    tool.name === 'git_diff' && tool.preview && tool.preview.trim() !== '' ? tool.preview : null;
  const diffSource = editDiff ?? gitDiff;

  const expandable = !diffSource && tool.preview !== null && tool.preview !== '';

  return (
    <>
      <button
        className={`tool ${tool.status}`}
        onClick={() => expandable && setOpen((v) => !v)}
        style={expandable ? undefined : { cursor: 'default' }}
      >
        <span className="glyph">{GLYPH[tool.status]}</span>
        <span className="cmd">
          {verb} <span className="path">{main}</span>
        </span>
        {tool.durationMs !== null && <span className="dur">{formatDuration(tool.durationMs)}</span>}
        {expandable && <span className="expand-hint">{open ? '▲ 收起' : '▼ 输出'}</span>}
      </button>
      {diffSource && <DiffBlock source={diffSource} title={tool.name === 'git_diff' ? main : undefined} />}
      {open && expandable && <div className="tool-preview">{tool.preview}</div>}
    </>
  );
}
