// 工具调用行：台阶轨样式；有 preview 时可点击展开。

import { useState } from 'react';
import type { ToolCallView } from '../state/store';
import { formatDuration, toolSummary } from '../lib/format';

const GLYPH: Record<ToolCallView['status'], string> = {
  done: '✓',
  run: '◍',
  fail: '✗',
};

export function ToolCallRow({ tool }: { tool: ToolCallView }) {
  const [open, setOpen] = useState(false);
  const { verb, main } = toolSummary(tool.name, tool.arguments);
  const expandable = tool.preview !== null && tool.preview !== '';

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
      {open && expandable && <div className="tool-preview">{tool.preview}</div>}
    </>
  );
}
