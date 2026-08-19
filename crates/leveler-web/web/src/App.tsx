// 应用外壳：三栏布局 + RuntimeBridge 生命周期 + 全局快捷键。
// 顶栏：任务身份 | 运行状态 | 上下文 · 更多。

import { useEffect, useRef, useState } from 'react';
import { RuntimeBridge } from './lib/controller';
import { formatElapsed, repoShortName } from './lib/format';
import { headerWaitingCue, inspectorMode } from './lib/inspectorModel';
import { presentTurnEnd } from './lib/turn';
import { AppProvider, useAppDispatch, useAppState, type AppState } from './state/store';
import { BridgeProvider, useBridge } from './state/bridge';
import { MoreMenu } from './components/Appearance';
import { Composer } from './components/Composer';
import { DiffView } from './components/DiffView';
import { FileViewerProvider } from './components/FileViewer';
import { Hero } from './components/Hero';
import { Inspector } from './components/Inspector';
import { LevelMeter } from './components/LevelMeter';
import { Rail } from './components/Rail';
import { Timeline } from './components/Timeline';
import type { ApprovalDecision } from './types/protocol';

const APPROVAL_KEYS: Record<string, ApprovalDecision> = {
  y: 'approve_once',
  s: 'approve_session',
  a: 'approve_always',
  n: 'deny',
};

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.tagName === 'TEXTAREA' || target.tagName === 'INPUT' || target.isContentEditable;
}

function Shell() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const stateRef = useRef<AppState>(state);
  stateRef.current = state;
  const [bridge] = useState(() => new RuntimeBridge(dispatch, () => stateRef.current));
  const view = state.stageView;
  const setView = (v: 'chat' | 'diff') => dispatch({ type: 'stage_view', view: v });

  useEffect(() => {
    if (window.matchMedia('(max-width: 1279px)').matches) {
      dispatch({ type: 'set_inspector', open: false });
    }
    if (window.matchMedia('(max-width: 899px)').matches && stateRef.current.railOpen) {
      dispatch({ type: 'toggle_rail' });
    }
  }, [dispatch]);

  useEffect(() => {
    bridge.start();
    return () => bridge.dispose();
  }, [bridge]);

  const prevConnection = useRef(state.connection);
  useEffect(() => {
    if (prevConnection.current !== 'online' && state.connection === 'online') {
      bridge.flushQueue();
    }
    prevConnection.current = state.connection;
  }, [state.connection, bridge]);

  const mode = inspectorMode(state.current);
  useEffect(() => {
    if (mode === 'waiting') dispatch({ type: 'set_inspector', open: true });
  }, [mode, dispatch]);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const st = stateRef.current;
      const meta = e.metaKey || e.ctrlKey;

      if (e.key === 'Escape') {
        if (st.current?.turnActive && !isTypingTarget(e.target)) {
          bridge.cancelTurn();
        }
        return;
      }

      if (meta && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        document.querySelector<HTMLTextAreaElement>('.composer textarea')?.focus();
        return;
      }
      if (meta && e.shiftKey && e.key.toLowerCase() === 'd') {
        e.preventDefault();
        dispatch({ type: 'stage_view', view: 'diff' });
        return;
      }
      if (meta && e.shiftKey && e.key.toLowerCase() === 't') {
        e.preventDefault();
        dispatch({ type: 'set_inspector', open: true });
        dispatch({ type: 'stage_view', view: 'chat' });
        return;
      }
      if (meta && e.key.toLowerCase() === 'b') {
        e.preventDefault();
        dispatch({ type: 'toggle_rail' });
        return;
      }
      if (meta && e.key.toLowerCase() === 'i') {
        e.preventDefault();
        dispatch({ type: 'toggle_inspector' });
        return;
      }

      if (isTypingTarget(e.target) || e.isComposing) return;
      const decision = APPROVAL_KEYS[e.key];
      if (decision && st.current && st.current.pendingApprovals.length > 0) {
        e.preventDefault();
        bridge.decideApproval(st.current.pendingApprovals[0].id, decision);
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [bridge, dispatch]);

  const title = state.current?.title ?? '新对话';
  const current = state.current;
  const project = repoShortName(current?.repository ?? state.repository);
  const branch = current?.branch ?? null;

  return (
    <BridgeProvider value={bridge}>
      <FileViewerProvider>
        <div
          className={`deck${state.railOpen ? '' : ' rail-off'}${state.inspectorOpen ? '' : ' insp-off'}`}
        >
          <Rail />
          <main className="stage">
            <header className="stage-head">
              <button
                type="button"
                className="chrome-toggle"
                title="切换侧栏 (⌘B)"
                aria-label="切换侧栏"
                onClick={() => dispatch({ type: 'toggle_rail' })}
              >
                ☰
              </button>
              <span className="sh-identity">
                <span className="sh-proj">{project}</span>
                {branch && <span className="sh-branch">{branch}</span>}
                <span className="sh-title">{title}</span>
              </span>
              <span className="view-tabs">
                <button
                  type="button"
                  className={`view-tab${view === 'chat' ? ' on' : ''}`}
                  onClick={() => setView('chat')}
                >
                  对话
                </button>
                <button
                  type="button"
                  className={`view-tab${view === 'diff' ? ' on' : ''}`}
                  onClick={() => setView('diff')}
                >
                  改动
                </button>
              </span>
              <RunStatus />
              <span className="sh-right">
                <LevelMeter />
                <MoreMenu />
                <button
                  type="button"
                  className="chrome-toggle"
                  title="任务面板 (⌘I)"
                  aria-label="切换任务面板"
                  onClick={() => dispatch({ type: 'toggle_inspector' })}
                >
                  ▤
                </button>
              </span>
            </header>
            {view === 'diff' && !state.draft ? (
              <DiffView />
            ) : state.draft ? (
              <Hero />
            ) : (
              <Timeline />
            )}
            <Composer />
          </main>
          <Inspector />
          <div
            className="chrome-scrim"
            onClick={() => {
              dispatch({ type: 'set_inspector', open: false });
              if (state.railOpen) dispatch({ type: 'toggle_rail' });
            }}
          />
        </div>
      </FileViewerProvider>
    </BridgeProvider>
  );
}

function RunStatus() {
  const current = useAppState().current;
  const dispatch = useAppDispatch();
  const bridge = useBridge();
  const mode = inspectorMode(current);
  const waiting = headerWaitingCue(current);
  const startedAt = current?.turnStartedAt ?? null;
  const [, forceTick] = useState(0);

  useEffect(() => {
    if (mode !== 'running' && mode !== 'waiting') return;
    const timer = setInterval(() => forceTick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, [mode]);

  if (waiting) {
    return (
      <button
        type="button"
        className="sh-status wait"
        title="打开任务面板"
        aria-label={`${waiting.label}，打开任务面板`}
        onClick={() => dispatch({ type: 'set_inspector', open: true })}
      >
        <span aria-hidden="true">{waiting.glyph}</span>
        {waiting.label}
      </button>
    );
  }

  if (mode === 'running') {
    const elapsed = startedAt ? Math.max(0, Math.floor((Date.now() - startedAt) / 1000)) : 0;
    return (
      <span className="sh-status run">
        <i className="dot" />
        <span className="sh-activity">{current?.activity ?? '正在运行'}</span>
        <span className="sh-elapsed">{formatElapsed(elapsed)}</span>
        <button type="button" className="sh-stop" title="取消当前回合 (Esc)" onClick={() => bridge.cancelTurn()}>
          停止
        </button>
      </span>
    );
  }

  if (mode === 'terminal' && current?.lastTurn) {
    const p = presentTurnEnd(current.lastTurn);
    return (
      <span className={`sh-status term tone-${p.tone}`}>
        <span aria-hidden="true">{p.glyph}</span>
        {p.label}
      </span>
    );
  }

  return (
    <span className="sh-status">
      <i className="dot" />
      就绪
    </span>
  );
}

export function App() {
  return (
    <AppProvider>
      <Shell />
    </AppProvider>
  );
}
