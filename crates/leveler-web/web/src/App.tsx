// 应用外壳：三栏布局 + RuntimeBridge 生命周期 + 全局快捷键
// （y/s/a/n 审批决策、Esc 取消当前回合；焦点在输入框时自动忽略）。
// 顶部状态栏：项目/分支 · 会话 · 视图切换（对话/改动） · 运行状态 · 上下文。

import { useEffect, useRef, useState } from 'react';
import { RuntimeBridge } from './lib/controller';
import { repoShortName } from './lib/format';
import { AppProvider, useAppDispatch, useAppState, type AppState } from './state/store';
import { BridgeProvider, useBridge } from './state/bridge';
import { ThemeMenu, SettingsButton } from './components/Appearance';
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

type StageView = 'chat' | 'diff';

function Shell() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const stateRef = useRef<AppState>(state);
  stateRef.current = state;
  const [bridge] = useState(() => new RuntimeBridge(dispatch, () => stateRef.current));
  const [view, setView] = useState<StageView>('chat');

  useEffect(() => {
    bridge.start();
    return () => bridge.dispose();
  }, [bridge]);

  // 重连恢复后补发排队消息
  const prevConnection = useRef(state.connection);
  useEffect(() => {
    if (prevConnection.current !== 'online' && state.connection === 'online') {
      bridge.flushQueue();
    }
    prevConnection.current = state.connection;
  }, [state.connection, bridge]);

  // 全局快捷键
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement;
      if (target.tagName === 'TEXTAREA' || target.tagName === 'INPUT' || e.isComposing) return;
      const st = stateRef.current;

      const decision = APPROVAL_KEYS[e.key];
      if (decision && st.current && st.current.pendingApprovals.length > 0) {
        e.preventDefault();
        bridge.decideApproval(st.current.pendingApprovals[0].id, decision);
        return;
      }
      if (e.key === 'Escape' && st.current?.turnActive) {
        bridge.cancelTurn();
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [bridge]);

  const title = state.current?.title ?? '新对话';
  const current = state.current;
  const project = repoShortName(current?.repository ?? state.repository);
  const branch = current?.branch ?? null;

  return (
    <BridgeProvider value={bridge}>
      <FileViewerProvider>
        <div className="deck">
          <Rail />
          <main className="stage">
            <header className="stage-head">
              <span className="sh-proj">
                {project}
                {branch && <span className="sh-branch">{branch}</span>}
              </span>
              <span className="sh-sep" />
              <span className="sh-title">{title}</span>
              <span className="spacer" />
              <span className="view-tabs">
                <button
                  className={`view-tab${view === 'chat' ? ' on' : ''}`}
                  onClick={() => setView('chat')}
                >
                  对话
                </button>
                <button
                  className={`view-tab${view === 'diff' ? ' on' : ''}`}
                  onClick={() => setView('diff')}
                >
                  改动
                </button>
              </span>
              <RunStatus />
              <LevelMeter />
              <span className="sh-sep" />
              <ThemeMenu />
              <SettingsButton />
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
        </div>
      </FileViewerProvider>
    </BridgeProvider>
  );
}

/** 运行状态行：当前活动标签 + 回合计时 + 停止；空闲时显示「就绪」。 */
function RunStatus() {
  const current = useAppState().current;
  const bridge = useBridge();
  const running = current?.turnActive ?? false;
  const startedAt = current?.turnStartedAt ?? null;
  const activity = current?.activity ?? null;
  const [, forceTick] = useState(0);

  // 运行中每秒刷新一次计时
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => forceTick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, [running]);

  if (!running) {
    return (
      <span className="sh-status">
        <i className="dot" />
        就绪
      </span>
    );
  }

  const elapsed = startedAt ? Math.max(0, Math.floor((Date.now() - startedAt) / 1000)) : 0;
  return (
    <span className="sh-status run">
      <i className="dot" />
      <span className="sh-activity">{activity ?? '运行中'}</span>
      <span className="sh-elapsed">{elapsed}s</span>
      <button className="sh-stop" title="取消当前回合 (Esc)" onClick={() => bridge.cancelTurn()}>
        ■ 停止
      </button>
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
