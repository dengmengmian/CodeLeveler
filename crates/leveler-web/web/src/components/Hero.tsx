// 空状态 hero：品牌 + 项目选择器。列出聚合层注册的项目（无聚合层时
// 回退为当前仓库单项）；选择决定新对话落在哪个项目（draftProject）。

import { useEffect, useRef, useState } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { BrandMark } from './BrandMark';
import { repoShortName } from '../lib/format';

export function Hero() {
  const state = useAppState();
  const bridge = useBridge();
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

  // 当前选择：draftProject ?? 当前仓库
  const selected = state.draftProject ?? state.repository;
  const projects = state.projects;

  return (
    <div className="hero">
      <BrandMark />
      <div className="h-word">CodeLeveler</div>
      <div className="h-sub">还没有消息 —— 在下方输入开始对话</div>
      <div className="h-proj" ref={wrapRef}>
        <button className="h-proj-btn" onClick={() => setOpen((v) => !v)}>
          <span>⌂</span>
          <span>{repoShortName(selected)}</span>
          <span className="caret">▼</span>
        </button>
        <div className="pop" hidden={!open}>
          <div className="pop-head">选择项目 · 新对话将在此工作区创建</div>
          {projects.length === 0 && (
            <button
              className="pop-item sel"
              onClick={() => {
                bridge.newDraft();
                setOpen(false);
              }}
            >
              <span className="desc">
                {repoShortName(state.repository)}
                <span className="ppath">{state.repository || '当前仓库'}</span>
              </span>
            </button>
          )}
          {projects.map((p) => (
            <button
              key={p.path}
              className={`pop-item${p.path === selected ? ' sel' : ''}`}
              disabled={p.status === 'offline'}
              onClick={() => {
                bridge.newDraft(p.path);
                setOpen(false);
              }}
            >
              <span className="desc">
                {p.name}
                <span className="ppath">
                  {p.path}
                  {p.status !== 'online' ? ` · ${p.status === 'starting' ? '启动中' : '离线'}` : ''}
                </span>
              </span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
