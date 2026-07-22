// 工具调用行：紧凑无边框列表行（时间序执行明细）。
// 默认弱视觉权重：成功项不高亮；仅当前执行 / 失败项增强。
// 文件编辑（apply_patch）与 git_diff 显示「M path +N −M」轻量摘要，完整 Diff 点击后展开；
// 其余工具有输出时 hover 出现「输出」入口；失败命令默认展开末尾 30 行输出。

import { useState } from 'react';
import type { ToolCallView } from '../state/store';
import { formatDuration, toolSummary } from '../lib/format';
import { tailLines } from '../lib/toolstats';
import { DiffBlock, parseDiff, patchFromArguments } from './DiffBlock';

const GLYPH: Record<ToolCallView['status'], string> = {
  done: '✓',
  run: '◍',
  fail: '✗',
};

/** 失败命令默认展开输出末尾行数 */
const FAIL_TAIL = 30;

export function ToolCallRow({ tool }: { tool: ToolCallView }) {
  const failed = tool.status === 'fail';
  const [open, setOpen] = useState(failed);
  const { verb, main } = toolSummary(tool.name, tool.arguments);

  // 原始 diff → 轻量统计摘要；完整 DiffBlock 按需展开（事实由原始 diff 负责）。
  const editDiff =
    tool.name === 'apply_patch' && tool.status !== 'fail' ? patchFromArguments(tool.arguments) : null;
  const gitDiff =
    tool.name === 'git_diff' && tool.preview && tool.preview.trim() !== '' ? tool.preview : null;
  const diffSource = editDiff ?? gitDiff;
  const diff = diffSource ? parseDiff(diffSource) : null;

  const preview = tool.preview !== null && tool.preview !== '' ? tool.preview : null;
  const expandable = diffSource !== null || preview !== null;

  return (
    <>
      <button
        className={`tool-row ${tool.status}`}
        onClick={() => expandable && setOpen((v) => !v)}
        style={expandable ? undefined : { cursor: 'default' }}
      >
        <span className="glyph">{GLYPH[tool.status]}</span>
        <span className="cmd">
          {verb} <span className="path">{diff && diff.files.length > 0 ? diff.files.join(', ') : main}</span>
        </span>
        {diff && (
          <span className="tool-stat">
            <span className="add">+{diff.additions}</span>{' '}
            <span className="del">−{diff.deletions}</span>
          </span>
        )}
        {tool.durationMs !== null && <span className="dur">{formatDuration(tool.durationMs)}</span>}
        {expandable && (
          <span className="tool-acts">{open ? '收起' : failed ? '查看错误输出' : diff ? '查看改动' : '输出'}</span>
        )}
      </button>
      {open && diffSource && (
        <DiffBlock source={diffSource} title={tool.name === 'git_diff' ? main : undefined} />
      )}
      {open && !diffSource && preview && (
        <div className="tool-preview">{failed ? tailLines(preview, FAIL_TAIL) : preview}</div>
      )}
    </>
  );
}
