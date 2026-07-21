// 澄清卡：agent 中途提问。选项一键回答，也可自由输入；空答案 = 跳过。

import { useState } from 'react';
import type { UiClarificationRequest } from '../types/protocol';
import { useBridge } from '../state/bridge';

export function ClarificationCard({ request }: { request: UiClarificationRequest }) {
  const bridge = useBridge();
  const [answer, setAnswer] = useState('');
  const shortId = request.id.length > 10 ? `${request.id.slice(0, 10)}…` : request.id;

  const submit = (text: string) => bridge.answerClarification(request.id, text);

  return (
    <div className="clar">
      <div className="c-head">
        <span>?</span> CLARIFICATION
        <span style={{ marginLeft: 'auto', color: 'var(--text-tertiary)' }}>{shortId}</span>
      </div>
      <div className="c-body">
        {request.question}
        {request.options.length > 0 && (
          <div className="c-options">
            {request.options.map((opt) => (
              <button key={opt} className="abtn" onClick={() => submit(opt)}>
                {opt}
              </button>
            ))}
          </div>
        )}
      </div>
      <div className="c-input-row">
        <input
          value={answer}
          placeholder="输入回答，留空表示跳过"
          onChange={(e) => setAnswer(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') submit(answer);
          }}
        />
        <button className="abtn primary" onClick={() => submit(answer)}>
          回答
        </button>
        <button className="abtn" onClick={() => submit('')}>
          跳过
        </button>
      </div>
    </div>
  );
}
