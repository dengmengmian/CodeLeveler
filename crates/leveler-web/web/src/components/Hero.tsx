// 空状态 hero：品牌 + 项目选择器。本期只有一个项目（当前 runtime 的仓库），
// 下拉照常渲染；“添加新项目”入口隐藏（多项目不在本期）。

import { useEffect, useRef, useState } from 'react';
import { useAppState } from '../state/store';
import { BrandMark } from './BrandMark';
import { repoShortName } from '../lib/format';

export function Hero() {
  const repository = useAppState().repository;
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('click', onDocClick);
    return () => document.removeEventListener('click', onDocClick);
  }, [open]);

  return (
    <div className="hero">
      <BrandMark />
      <div className="h-word">CODELEVELER</div>
      <div className="h-sub">还没有消息 —— 在下方输入开始对话</div>
      <div className="h-proj" ref={wrapRef}>
        <button className="h-proj-btn" onClick={() => setOpen((v) => !v)}>
          <span>⌂</span>
          <span>{repoShortName(repository)}</span>
          <span className="caret">▼</span>
        </button>
        <div className="pop" hidden={!open}>
          <div className="pop-head">选择项目 · 新对话将在此工作区创建</div>
          <button className="pop-item sel" onClick={() => setOpen(false)}>
            <span className="desc">
              {repoShortName(repository)}
              <span className="ppath">{repository || '当前仓库'}</span>
            </span>
          </button>
          {/* TODO(多项目)：任意 repo root 不在本期，隐藏“添加新项目”入口 */}
          <button className="pop-item" hidden>
            <span className="cmd" style={{ width: 'auto' }}>
              ＋ 添加新项目…
            </span>
          </button>
        </div>
      </div>
    </div>
  );
}
