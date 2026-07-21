// 空状态 hero：品牌 + 项目选择器。列出聚合层注册的项目（无聚合层时
// 回退为当前仓库单项）；选择决定新对话落在哪个项目（draftProject）。

import { useEffect, useRef, useState } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { BrandMark } from './BrandMark';
import { repoShortName, formatRelative } from '../lib/format';

/** 空状态快捷操作：点击把起手语注入输入框，用户补全后发送。 */
const QUICK_ACTIONS: ReadonlyArray<{ label: string; hint: string; seed: string }> = [
  { label: '分析当前项目', hint: '架构与主要风险', seed: '分析当前项目的架构与主要风险，并给出改进建议。' },
  { label: '修复一个问题', hint: '定位并修复', seed: '我遇到一个问题需要修复：' },
  { label: '实现新功能', hint: '从需求到实现', seed: '我想实现一个新功能：' },
  { label: '检查代码改动', hint: '评审当前 diff', seed: '检查当前的代码改动并做一次评审。' },
];

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
  const recent = state.sessions.slice(0, 5);

  return (
    <div className="hero">
      <div className="h-mark">
        <BrandMark />
      </div>
      <div className="h-word">CodeLeveler</div>
      <div className="h-sub">选一个快捷操作，或在下方直接告诉 Agent 要做什么</div>
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

      <div className="h-actions">
        {QUICK_ACTIONS.map((a) => (
          <button key={a.label} className="h-action" onClick={() => bridge.seedComposer(a.seed)}>
            <span className="ha-label">{a.label}</span>
            <span className="ha-hint">{a.hint}</span>
          </button>
        ))}
      </div>

      {recent.length > 0 && (
        <div className="h-recent">
          <div className="h-recent-head">最近会话</div>
          {recent.map((s) => (
            <button key={s.id} className="h-recent-item" onClick={() => bridge.selectSession(s.id)}>
              <span className="hr-goal">{s.goal || '未命名会话'}</span>
              <span className="hr-time">{formatRelative(s.updated_at)}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
