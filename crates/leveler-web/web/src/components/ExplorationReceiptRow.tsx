// Exploration receipt (探索回执): consecutive read-only calls as ONE reversible fold.
//
// The reference collapses consecutive read-only exploration into a single
// `▸ 读取 N 个文件 · 搜索 M 次` receipt. Collapsed, the receipt stands in for its
// members — including the completed Thoughts the fold was hiding, which is what
// "FoldedThought" means. Opening it restores the real member chronology; the
// fold hides, it never destroys.

import { useState } from 'react';
import { explorationEntries, type ExplorationReceipt, type FoldedThought } from '../lib/conversationPresentation';
import { ThoughtRow } from './ThoughtRow';

const STATUS_GLYPH: Record<string, string> = {
  run: '◍',
  done: '✓',
  fail: '✗',
  cancelled: '■',
  unknown: '◇',
};

export function ExplorationReceiptRow({
  receipt,
  hiddenThoughts = [],
}: {
  receipt: ExplorationReceipt;
  /** Completed Thoughts whose arrival sits inside this fold's span. */
  hiddenThoughts?: FoldedThought[];
}) {
  const [open, setOpen] = useState(false);
  return (
    <div className={`exploration ${open ? 'open' : 'folded'}`}>
      <button
        type="button"
        className="exploration-head"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className="exploration-glyph" aria-hidden="true">
          {open ? '▾' : '▸'}
        </span>
        <span className="exploration-label">{receipt.label}</span>
        {!open && hiddenThoughts.length > 0 && (
          <span className="exploration-thoughts">· {hiddenThoughts.length} 个思考</span>
        )}
      </button>
      {open && (
        <div className="exploration-members">
          {explorationEntries(receipt, hiddenThoughts).map(entry => {
            if (entry.kind === 'thought') return <ThoughtRow key={entry.thought.id} thought={entry.thought} />;
            const member = entry.member;
            return (
            <div key={member.id} className={`exploration-member ${member.status}`}>
              <span className="exploration-member-glyph" aria-hidden="true">
                {STATUS_GLYPH[member.status] ?? '◇'}
              </span>
              <span className="exploration-member-name">{member.name}</span>
              {member.target !== '' && <span className="exploration-member-target">{member.target}</span>}
            </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
