// 左栏：项目工作台导航。顶部品牌 + 新对话入口；面板切换：
// 会话（按仓库分组的会话列表）/ 文件（仓库文件树，前缀过滤）/
// 搜索（内容搜索）/ Git（工作区改动）。数据走 REST，按当前会话定位仓库；
// 底部 daemon 连接状态。

import { useCallback, useEffect, useMemo, useState } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { BrandMark } from './BrandMark';
import { useOpenFile } from './FileViewer';
import { gitStatus, listFiles, searchFiles, type GitStatus, type SearchMatch } from '../lib/api';
import { formatRelative, repoShortName, statusDot } from '../lib/format';
import type { UiSessionSummary } from '../types/protocol';

interface ProjectGroup {
  repository: string;
  sessions: UiSessionSummary[];
}

type Panel = 'sessions' | 'files' | 'search' | 'git';

const PANELS: ReadonlyArray<readonly [Panel, string]> = [
  ['sessions', '会话'],
  ['files', '文件'],
  ['search', '搜索'],
  ['git', 'Git'],
];

export function Rail() {
  const state = useAppState();
  const bridge = useBridge();
  const [panel, setPanel] = useState<Panel>('sessions');

  return (
    <aside className="rail">
      <div className="brand">
        <BrandMark />
        <div>
          <div className="name">CodeLeveler</div>
          <div className="ver">web · v0.1</div>
        </div>
      </div>

      <button className="rail-new" onClick={() => bridge.newDraft()}>
        ＋ 新对话
      </button>

      <div className="rail-tabs">
        {PANELS.map(([key, label]) => (
          <button
            key={key}
            className={`rail-tab${panel === key ? ' on' : ''}`}
            onClick={() => setPanel(key)}
          >
            {label}
          </button>
        ))}
      </div>

      {panel === 'sessions' && <SessionsPanel />}
      {panel === 'files' && <FilesPanel />}
      {panel === 'search' && <SearchPanel />}
      {panel === 'git' && <GitPanel />}

      <div className="rail-foot">
        <span className={`led${state.connection === 'online' ? '' : ' off'}`} />
        <span>
          Daemon · {state.connection === 'online' ? '已连接' : '重连中'} · {window.location.host}
        </span>
      </div>
    </aside>
  );
}

// ── 会话面板 ────────────────────────────────────────────────────────

function SessionsPanel() {
  const state = useAppState();
  const bridge = useBridge();
  const [closed, setClosed] = useState<ReadonlySet<string>>(new Set());

  // 按 repository 分组；保持首见顺序，组内按 updated_at 倒序（新的在前）
  const groups = useMemo<ProjectGroup[]>(() => {
    const map = new Map<string, UiSessionSummary[]>();
    for (const s of state.sessions) {
      const repo = s.repository ?? state.repository ?? '';
      const list = map.get(repo);
      if (list) list.push(s);
      else map.set(repo, [s]);
    }
    return [...map.entries()].map(([repository, sessions]) => ({
      repository,
      sessions: [...sessions].sort((a, b) => b.updated_at.localeCompare(a.updated_at)),
    }));
  }, [state.sessions, state.repository]);

  const toggle = (repo: string) => {
    setClosed((prev) => {
      const next = new Set(prev);
      if (next.has(repo)) next.delete(repo);
      else next.add(repo);
      return next;
    });
  };

  return (
    <div className="sessions">
      {groups.length === 0 && (
        <div className="rail-empty">
          还没有会话。
          <br />
          点击「＋ 新对话」或在下方输入开始。
        </div>
      )}
      {groups.map((g) => (
        <div className={`proj${closed.has(g.repository) ? ' closed' : ''}`} key={g.repository || '_'}>
          <button className="proj-head" onClick={() => toggle(g.repository)}>
            <span className="fold">▼</span>
            <span className="pmeta">
              <span className="pname">{repoShortName(g.repository)}</span>
              <span className="ppath">{g.repository || '当前仓库'}</span>
            </span>
            <span className="pcount">{g.sessions.length}</span>
            <span
              className="p-add"
              role="button"
              title="在此项目中新建对话"
              onClick={(e) => {
                e.stopPropagation();
                bridge.newDraft();
              }}
            >
              ＋
            </span>
          </button>
          <div className="proj-sessions">
            {g.sessions.map((s) => {
              const dot = statusDot(s.status);
              return (
                <button
                  key={s.id}
                  className={`sess${state.current?.id === s.id ? ' active' : ''}`}
                  onClick={() => bridge.selectSession(s.id)}
                >
                  <div className="t">{s.goal || '未命名会话'}</div>
                  <div className="m">
                    <i className={`dot ${dot.cls}`} />
                    <span>{dot.label}</span>
                    <span>·</span>
                    <span>{formatRelative(s.updated_at)}</span>
                  </div>
                </button>
              );
            })}
          </div>
        </div>
      ))}
    </div>
  );
}

// ── 共用：面板数据加载 ───────────────────────────────────────────────

function useSessionId(): string | null {
  return useAppState().current?.id ?? null;
}

function NoSession() {
  return <div className="rail-empty">进入会话后可用 —— 面板数据按当前会话定位仓库。</div>;
}

// ── 文件面板 ────────────────────────────────────────────────────────

function FilesPanel() {
  const sessionId = useSessionId();
  const openFile = useOpenFile();
  const [files, setFiles] = useState<string[] | null>(null);
  const [filter, setFilter] = useState('');
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    if (!sessionId) return;
    listFiles(sessionId)
      .then((r) => {
        setFiles(r.files);
        setError(null);
      })
      .catch((err: unknown) => setError(err instanceof Error ? err.message : String(err)));
  }, [sessionId]);

  useEffect(() => {
    setFiles(null);
    refresh();
  }, [refresh]);

  if (!sessionId) return <NoSession />;

  const shown = (files ?? []).filter((f) => f.toLowerCase().includes(filter.toLowerCase()));

  return (
    <div className="rail-panel">
      <input
        className="rp-input"
        placeholder="按路径过滤…"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      {error && <div className="rail-empty">{error}</div>}
      {!error && files === null && <div className="rail-empty">加载中…</div>}
      {files !== null && shown.length === 0 && <div className="rail-empty">无匹配文件。</div>}
      <div className="rp-list">
        {shown.slice(0, 500).map((f) => (
          <button className="rp-item" key={f} title={f} onClick={() => openFile(f)}>
            {f}
          </button>
        ))}
        {shown.length > 500 && <div className="rail-empty">… 共 {shown.length} 个，继续过滤以缩小范围</div>}
      </div>
    </div>
  );
}

// ── 搜索面板 ────────────────────────────────────────────────────────

function SearchPanel() {
  const sessionId = useSessionId();
  const openFile = useOpenFile();
  const [query, setQuery] = useState('');
  const [matches, setMatches] = useState<SearchMatch[] | null>(null);
  const [searching, setSearching] = useState(false);

  // 输入防抖 300ms
  useEffect(() => {
    if (!sessionId || !query.trim()) {
      setMatches(null);
      return;
    }
    setSearching(true);
    const timer = setTimeout(() => {
      searchFiles(sessionId, query.trim())
        .then((r) => setMatches(r.matches))
        .catch(() => setMatches([]))
        .finally(() => setSearching(false));
    }, 300);
    return () => clearTimeout(timer);
  }, [sessionId, query]);

  if (!sessionId) return <NoSession />;

  return (
    <div className="rail-panel">
      <input
        className="rp-input"
        placeholder="搜索文件内容…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
      />
      {searching && <div className="rail-empty">搜索中…</div>}
      {!searching && matches !== null && matches.length === 0 && query.trim() && (
        <div className="rail-empty">无命中。</div>
      )}
      <div className="rp-list">
        {(matches ?? []).map((m, i) => (
          <button
            className="rp-match"
            key={`${m.path}:${m.line}:${i}`}
            onClick={() => openFile(m.path, m.line)}
          >
            <span className="rp-loc">
              {m.path}:{m.line}
            </span>
            <span className="rp-text">{m.text}</span>
          </button>
        ))}
      </div>
    </div>
  );
}

// ── Git 面板 ────────────────────────────────────────────────────────

const GIT_TAG: Record<string, { tag: string; cls: string }> = {
  modified: { tag: 'M', cls: 'mod' },
  added: { tag: 'A', cls: 'add' },
  deleted: { tag: 'D', cls: 'del' },
  renamed: { tag: 'R', cls: 'mod' },
  untracked: { tag: 'U', cls: 'add' },
};

function GitPanel() {
  const sessionId = useSessionId();
  const openFile = useOpenFile();
  const [status, setStatus] = useState<GitStatus | null>(null);

  const refresh = useCallback(() => {
    if (!sessionId) return;
    gitStatus(sessionId)
      .then(setStatus)
      .catch(() => setStatus({ branch: null, files: [] }));
  }, [sessionId]);

  useEffect(() => {
    setStatus(null);
    refresh();
  }, [refresh]);

  if (!sessionId) return <NoSession />;

  return (
    <div className="rail-panel">
      <div className="rp-bar">
        <span className="rp-branch">{status?.branch ?? '…'}</span>
        <button className="rp-refresh" onClick={refresh} title="刷新">
          ⟳
        </button>
      </div>
      {status === null && <div className="rail-empty">加载中…</div>}
      {status !== null && status.files.length === 0 && (
        <div className="rail-empty">工作区干净。</div>
      )}
      <div className="rp-list">
        {(status?.files ?? []).map((f) => {
          const t = GIT_TAG[f.status] ?? GIT_TAG.modified;
          return (
            <button className="rp-item git" key={f.path} title={f.path} onClick={() => openFile(f.path)}>
              <span className={`git-tag ${t.cls}`}>{t.tag}</span>
              <span className="p">{f.path}</span>
              {(f.added > 0 || f.removed > 0) && (
                <span className="nums">
                  <span className="add">+{f.added}</span>
                  <span className="del">−{f.removed}</span>
                </span>
              )}
            </button>
          );
        })}
      </div>
    </div>
  );
}
