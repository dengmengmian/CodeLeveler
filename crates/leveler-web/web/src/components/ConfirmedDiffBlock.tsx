// Confirmed Diff row: the runtime's applied result, in full, directly.
//
// The strongest presentation contract in the product. A confirmed edit's diff
// is NOT behind a click, NOT summarized as "+N −M", NOT capped at N rows and
// NOT reconstructed from the tool's arguments: `applied_diff` is the runtime's
// result and it is displayed whole.
//
// The runtime's confirmed patch is kept when the tool's own preview says
// something else: the preview is the tool's report, the diff is the change.

import { DiffBlock } from './DiffBlock';
import { CopyButton } from './CopyButton';
import type { ConfirmedDiff } from '../lib/conversationPresentation';

export function ConfirmedDiffBlock({ diff }: { diff: ConfirmedDiff }) {
  return (
    <div className="confirmed-diff" data-tool-id={diff.toolId}>
      <div className="confirmed-diff-head">
        <span className="confirmed-diff-title">已确认改动</span>
        <span className="confirmed-diff-stat add">+{diff.added}</span>
        <span className="confirmed-diff-stat del">−{diff.removed}</span>
        <CopyButton className="copy-btn-compact" text={diff.patch} title="复制完整 diff" />
      </div>
      <DiffBlock source={diff.patch} full />
    </div>
  );
}
