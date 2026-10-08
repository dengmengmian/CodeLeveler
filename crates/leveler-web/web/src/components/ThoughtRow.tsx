// Completed Thought row (折叠的思考) and the live Thinking row.
//
// Reasoning is never assistant prose. A running segment is the live row
// `◆ 思考中…` with its body; a completed segment is ONE folded
// `◆ 思考 · Ns` block whose body comes back when the reader opens it. The
// duration is the runtime's own measurement (`reasoning_completed`), never a
// number the UI derives from wall-clock painting.

import { useState } from 'react';
import { CopyButton } from './CopyButton';
import type { FoldedThought } from '../lib/conversationPresentation';

/** `0.1s`-style duration, the same shape the reference paints. */
export function thoughtDuration(elapsedMs: number): string {
  if (elapsedMs < 1000) return '<0.1s';
  const seconds = elapsedMs / 1000;
  if (seconds < 10) return `${seconds.toFixed(1)}s`;
  return `${Math.round(seconds)}s`;
}

export function ThoughtRow({ thought }: { thought: FoldedThought }) {
  const [open, setOpen] = useState(false);
  return (
    <div className={`thought ${open ? 'open' : 'folded'}`} data-thought-id={thought.id}>
      <button
        type="button"
        className="thought-head"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className="thought-glyph" aria-hidden="true">
          ◆
        </span>
        <span className="thought-label">思考 · {thoughtDuration(thought.elapsedMs)}</span>
        {open && <CopyButton className="copy-btn-compact" text={thought.body} />}
      </button>
      {open && <div className="thought-body">{thought.body}</div>}
    </div>
  );
}

/** The live segment: visible while the model is thinking, never a finished one. */
export function ThinkingRow({ text }: { text: string }) {
  return (
    <div className="thought live">
      <div className="thought-head">
        <span className="thought-glyph" aria-hidden="true">
          ◆
        </span>
        <span className="thought-label">思考中…</span>
      </div>
      {text.trim() !== '' && <div className="thought-body">{text}</div>}
    </div>
  );
}
