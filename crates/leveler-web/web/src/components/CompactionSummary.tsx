// Context compaction is stored as a User message with the runtime prefix.
// Presentation: collapsed disclosure + shared Markdown. Not a user prompt.

import { Archive } from 'lucide-react';
import { useState } from 'react';
import { CTRL_ICON } from '../lib/icons';
import { compactionBodyMarkdown } from '../lib/presentationKind';
import { MessageBody } from './MessageBody';

export function CompactionSummary({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const body = compactionBodyMarkdown(text);

  return (
    <div className="compaction-summary">
      <button
        type="button"
        className="compaction-toggle"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <Archive {...CTRL_ICON} aria-hidden="true" />
        <span>{open ? '▾' : '▸'} 上下文摘要 · 已压缩</span>
      </button>
      {open && (
        <div className="compaction-body">
          <MessageBody text={body} streaming={false} />
        </div>
      )}
    </div>
  );
}
