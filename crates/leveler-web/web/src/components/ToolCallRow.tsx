// 工具调用行：紧凑无边框列表行（时间序执行明细）。
// 默认弱视觉权重：成功项不高亮；仅当前执行 / 失败项增强。
// 已确认的编辑改动走 ConfirmedDiffBlock：runtime 的 applied_diff 完整直接展示，
// 不折叠、不截断、不由参数重建。git_diff 的输出仍按工具输出处理（点击展开）；
// 失败命令默认展开末尾 30 行输出。

import { useState } from 'react';
import { useAppDispatch, type ToolCallView } from '../state/store';
import { formatDuration, toolSummary } from '../lib/format';
import { displayPreview, failureReason } from '../lib/executionRounds';
import { confirmedDiffs } from '../lib/conversationPresentation';
import { tailLines } from '../lib/toolstats';
import { DiffBlock, parseDiff } from './DiffBlock';
import { ConfirmedDiffBlock } from './ConfirmedDiffBlock';

const GLYPH: Record<ToolCallView['status'], string> = {
  done: '✓',
  run: '◍',
  fail: '✗',
  cancelled: '■',
  unknown: '◇',
};

/** 失败命令默认展开输出末尾行数 */
const FAIL_TAIL = 30;

export function ToolCallRow({ tool }: { tool: ToolCallView }) {
  const dispatch = useAppDispatch();
  const failed = tool.status === 'fail';
  const [open, setOpen] = useState(failed);
  const { verb, main } = toolSummary(tool.name, tool.arguments);

  // The runtime's CONFIRMED diff, when this call confirmed an edit. It is
  // displayed whole, here, with no gate — see ConfirmedDiffBlock.
  const [confirmed] = confirmedDiffs([tool]);
  // A read-only `git_diff` call reports a diff-shaped OUTPUT: that is output,
  // not a confirmed change, and it keeps the ordinary output affordance.
  const outputDiff =
    !confirmed && tool.name === 'git_diff' && tool.preview && tool.preview.trim() !== ''
      ? tool.preview
      : null;
  const diff = outputDiff ? parseDiff(outputDiff) : null;

  const preview = displayPreview(tool);
  const expandable = outputDiff !== null || preview !== null;

  return (
    <>
      <button
        className={`tool-row ${tool.status}`}
        onClick={() => expandable && setOpen((v) => !v)}
        style={expandable ? undefined : { cursor: 'default' }}
      >
        <span className="glyph">{GLYPH[tool.status]}</span>
        <span className="cmd">
          {verb}{' '}
          <span className="path">
            {diff && diff.files.length > 0
              ? diff.files.map((p) => (
                  <button
                    key={p}
                    type="button"
                    className="path-jump"
                    onClick={(e) => {
                      e.stopPropagation();
                      dispatch({ type: 'focus_diff', path: p });
                    }}
                  >
                    {p}
                  </button>
                ))
              : main}
          </span>
        </span>
        {failed && !open && <span className="tool-reason">{failureReason(tool.preview ?? '')}</span>}
        {diff && (
          <span className="tool-stat">
            <span className="add">+{diff.additions}</span>{' '}
            <span className="del">−{diff.deletions}</span>
          </span>
        )}
        {tool.durationMs !== null && <span className="dur">{formatDuration(tool.durationMs)}</span>}
        {expandable && (
          <span className="tool-acts">{open ? '收起' : failed ? '查看错误输出' : '输出'}</span>
        )}
      </button>
      {confirmed && <ConfirmedDiffBlock diff={confirmed} />}
      {open && outputDiff && (
        <DiffBlock source={outputDiff} title={tool.name === 'git_diff' ? main : undefined} />
      )}
      {open && !outputDiff && !confirmed && preview && (
        <div className="tool-preview">{failed ? tailLines(preview, FAIL_TAIL) : preview}</div>
      )}
    </>
  );
}
