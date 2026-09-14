// A notice the runtime wrote into the model's context as a user-role message:
// a child settled, a delegation was lost or resumed. Not something the user
// typed, so it renders as a collapsed runtime note, not a user prompt.

import { useState } from 'react';
import { MessageBody } from './MessageBody';

export function RuntimeNotice({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const [first, ...rest] = text.split('\n');
  const title = (first ?? '').replace(/^#+\s*/, '');
  return (
    <div className="runtime-notice">
      <button
        type="button"
        className="runtime-notice-toggle"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span>
          {open ? '▾' : '▸'} {title}
        </span>
      </button>
      {open && (
        <div className="runtime-notice-body">
          <MessageBody text={rest.join('\n')} streaming={false} />
        </div>
      )}
    </div>
  );
}
