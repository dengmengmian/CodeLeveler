// Live thinking disclosure. TUI: ▸ 思考 · N 行, collapsed by default, 24-line cap.

import { ChevronDown, ChevronRight } from 'lucide-react';
import { useState } from 'react';
import { CTRL_ICON } from '../lib/icons';
import {
  capReasoning,
  reasoningLines,
  reasoningRemainderLabel,
  reasoningToggleLabel,
} from '../lib/reasoning';

export function ReasoningDisclosure({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const lines = reasoningLines(text);
  if (lines.length === 0) return null;
  const { visible, remainder } = capReasoning(lines);

  return (
    <div className="rs-think">
      <button
        type="button"
        className="rs-think-toggle"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        {open ? (
          <ChevronDown {...CTRL_ICON} aria-hidden="true" />
        ) : (
          <ChevronRight {...CTRL_ICON} aria-hidden="true" />
        )}
        {reasoningToggleLabel(lines.length)}
      </button>
      {open && (
        <div className="rs-think-body">
          {visible.map((line, i) => (
            <div key={i}>{line}</div>
          ))}
          {remainder > 0 && <div className="rs-think-more">{reasoningRemainderLabel(remainder)}</div>}
        </div>
      )}
    </div>
  );
}
