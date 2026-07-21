// 统一复制按钮：双重方块图标（线性、克制），用于消息与代码块。
// 默认 muted 灰；hover 提对比 + 轻背景；复制成功后短暂显示「已复制」（方案 A）。

import { useRef, useState } from 'react';

/** 双重方块「复制」图标（16px，currentColor 描边）。 */
function CopyIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" strokeWidth="1.8" />
      <path
        d="M6 15H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v1"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
      />
    </svg>
  );
}

export function CopyButton({
  text,
  title = '复制',
  className = '',
}: {
  text: string;
  title?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  const onClick = () => {
    void navigator.clipboard.writeText(text);
    setCopied(true);
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1500);
  };

  return (
    <button
      className={`copy-btn${copied ? ' copied' : ''}${className ? ` ${className}` : ''}`}
      title={title}
      onClick={onClick}
    >
      {copied ? <span className="copy-done">已复制</span> : <CopyIcon />}
    </button>
  );
}

/** 惰性取值版：用于代码块（复制时才读 innerText）。 */
export function CopyButtonLazy({
  getText,
  title = '复制代码',
  className = '',
}: {
  getText: () => string;
  title?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  const onClick = () => {
    void navigator.clipboard.writeText(getText());
    setCopied(true);
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1500);
  };

  return (
    <button
      className={`copy-btn${copied ? ' copied' : ''}${className ? ` ${className}` : ''}`}
      title={title}
      onClick={onClick}
    >
      {copied ? <span className="copy-done">已复制</span> : <CopyIcon />}
    </button>
  );
}
