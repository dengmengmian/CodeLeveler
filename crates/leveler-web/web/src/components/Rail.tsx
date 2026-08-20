// Single sidebar: Brand, New Task, Conversations / Files / Search,
// Workspaces (projects → sessions), Settings. Changes and Execution
// live in the workspace tabs, not here.

import {
  ChevronDown,
  ChevronRight,
  Files,
  MessageSquare,
  Plus,
  RefreshCw,
  Search,
  Settings,
  SquarePen,
  type LucideIcon,
} from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { gitStatus, listFiles, searchFiles, type GitStatus, type SearchMatch } from '../lib/api';
import { formatRelative, repoShortName } from '../lib/format';
import { CTRL_ICON, NAV_ICON } from '../lib/icons';
import { sessionStatusCue, sessionsForProject } from '../lib/projectScope';
import { SIDEBAR_NAV } from '../lib/railNav';
import { groupByDay } from '../lib/sessionDay';
import { useBridge } from '../state/bridge';
import { useAppDispatch, useAppState, type RailNav } from '../state/store';
import { AppearancePanel } from './Appearance';
import { BrandMark } from './BrandMark';
import { useOpenFile } from './FileViewer';
import { ProjectPicker } from './ProjectPicker';

const NAV_ICONS: Record<Exclude<RailNav, 'settings'>, LucideIcon> = {
  sessions: MessageSquare,
  files: Files,
  search: Search,
};

export function Sidebar() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const bridge = useBridge();
  const [picking, setPicking] = useState(false);

  const newTask = () => {
    const path = state.selectedProject ?? (state.repository || null);
    if (!path) {
      setPicking(true);
      return;
    }
    bridge.newDraft(path);
    dispatch({ type: 'set_rail_nav', nav: 'sessions' });
  };

  return (
    <aside className={`sidebar${state.railOpen ? '' : ' is-hidden'}`} aria-label="Sidebar">
      {picking && <ProjectPicker onClose={() => setPicking(false)} />}
      <div className="sb-brand" title={`CodeLeveler web · v${__APP_VERSION__}`}>
        <BrandMark />
        <span className="sb-word">CodeLeveler</span>
      </div>
      <button type="button" className="sb-new" onClick={newTask}>
        <SquarePen {...NAV_ICON} aria-hidden="true" />
        New Task
      </button>
      <nav className="sb-nav" aria-label="Primary">
        {SIDEBAR_NAV.map((item) => {
          const Icon = NAV_ICONS[item.id];
          const on = state.railNav === item.id;
          return (
            <button
              key={item.id}
              type="button"
              className={`sb-nav-btn${on ? ' on' : ''}`}
              aria-current={on ? 'page' : undefined}
              onClick={() => dispatch({ type: 'set_rail_nav', nav: item.id })}
            >
              <Icon {...NAV_ICON} aria-hidden="true" />
              {item.label}
            </button>
          );
        })}
      </nav>
      <div className="sb-body">
        {state.railNav === 'sessions' && <WorkspacesPanel onOpenProject={() => setPicking(true)} />}
        {state.railNav === 'files' && (
          <>
            <FilesPanel />
            <GitPanel />
          </>
        )}
        {state.railNav === 'search' && <SearchPanel />}
        {state.railNav === 'settings' && <AppearancePanel />}
      </div>
      <button
        type="button"
        className={`sb-settings${state.railNav === 'settings' ? ' on' : ''}`}
        aria-current={state.railNav === 'settings' ? 'page' : undefined}
        onClick={() => dispatch({ type: 'set_rail_nav', nav: 'settings' })}
      >
        <Settings {...NAV_ICON} aria-hidden="true" />
        Settings
      </button>
    </aside>
  );
}

function WorkspacesPanel({ onOpenProject }: { onOpenProject: () => void }) {
  const state = useAppState();
  const currentPath = state.selectedProject ?? (state.repository || null);
  const groups = useMemo(() => {
    if (state.projects.length > 0) return state.projects;
    if (!currentPath) return [];
    return [{ path: currentPath, name: repoShortName(currentPath), status: 'online' as const }];
  }, [state.projects, currentPath]);

  return (
    <div className="sessions">
      <div className="sb-kicker">WORKSPACES</div>
      {groups.length === 0 && (
        <div className="rail-empty">
          Open a project
          <button type="button" className="rail-new" onClick={onOpenProject}>
            打开项目
          </button>
        </div>
      )}
      {groups.map((p) => (
        <WorkspaceGroup key={p.path} path={p.path} name={p.name} status={p.status} />
      ))}
      {groups.length > 0 && (
        <button type="button" className="sb-open-proj" onClick={onOpenProject}>
          <Plus {...CTRL_ICON} aria-hidden="true" />
          打开项目…
        </button>
      )}
    </div>
  );
}

function WorkspaceGroup({
  path,
  name,
  status,
}: {
  path: string;
  name: string;
  status: string;
}) {
  const state = useAppState();
  const selected = (state.selectedProject ?? state.repository) === path;
  const [open, setOpen] = useState(selected);
  const [renaming, setRenaming] = useState(false);
  const [renamingSession, setRenamingSession] = useState<string | null>(null);
  const bridge = useBridge();
  const proj = state.projects.find((p) => p.path === path) ?? null;
  const sessions = useMemo(() => {
    const rows = sessionsForProject(state.sessions, path);
    return [...rows].sort((a, b) => b.updated_at.localeCompare(a.updated_at));
  }, [state.sessions, path]);

  const submitRename = (raw: string) => {
    setRenaming(false);
    const value = raw.trim();
    if (value === name) return;
    void bridge.renameProject(path, value);
  };

  return (
    <div className={`proj${open ? '' : ' closed'}`}>
      <div className="proj-head-row">
        <button
          type="button"
          className="proj-head"
          title={path}
          onClick={() => setOpen((v) => !v)}
        >
          {open ? (
            <ChevronDown {...CTRL_ICON} aria-hidden="true" />
          ) : (
            <ChevronRight {...CTRL_ICON} aria-hidden="true" />
          )}
          {renaming ? (
            <RenameInput initial={name} onSubmit={submitRename} onCancel={() => setRenaming(false)} />
          ) : (
            <span className="pname">{name}</span>
          )}
          {status === 'offline' && <span className="proj-offline">Offline</span>}
        </button>
        <ProjectMenu
          repo={path}
          name={name}
          canManage={!!proj}
          isPrimary={state.projects[0]?.path === path}
          onRename={() => setRenaming(true)}
        />
      </div>
      {open && (
        <div className="proj-sessions">
          {status === 'offline' && (
            <button type="button" className="rail-new" onClick={() => void bridge.restartProject(path)}>
              Restart
            </button>
          )}
          {sessions.length === 0 && status !== 'offline' && <div className="rail-empty">No sessions yet</div>}
          {groupByDay(sessions).map((day) => (
            <div key={day.bucket} className="sess-day">
              <div className="sess-day-label">{day.label}</div>
              {day.items.map((s) => {
                const liveWait =
                  state.current?.id === s.id &&
                  (state.current.pendingApprovals.length > 0 ||
                    state.current.pendingClarifications.length > 0);
                const cue = liveWait
                  ? { kind: 'waiting' as const, label: 'Waiting' }
                  : sessionStatusCue(s.status);
                const showStatus =
                  cue.kind === 'running' ||
                  cue.kind === 'waiting' ||
                  cue.kind === 'failed' ||
                  cue.kind === 'blocked';
                return (
                  <button
                    key={s.id}
                    type="button"
                    className={`sess${state.current?.id === s.id ? ' active' : ''}`}
                    onClick={() => bridge.selectSession(s.id)}
                    title={
                      cue.label ? `${cue.label} · ${formatRelative(s.updated_at)}` : formatRelative(s.updated_at)
                    }
                  >
                    {showStatus ? <i className={`dot ${cue.kind}`} /> : null}
                    {renamingSession === s.id ? (
                      <RenameInput
                        initial={s.goal}
                        onSubmit={(value) => {
                          setRenamingSession(null);
                          bridge.renameSession(s.id, value);
                        }}
                        onCancel={() => setRenamingSession(null)}
                      />
                    ) : (
                      <span className="t">{s.goal || '未命名会话'}</span>
                    )}
                    <span className="ago">
                      {showStatus ? `${cue.label} · ` : ''}
                      {formatRelative(s.updated_at)}
                    </span>
                    <SessionMenu id={s.id} title={s.goal} onRename={() => setRenamingSession(s.id)} />
                  </button>
                );
              })}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function ProjectMenu({
  repo,
  name,
  canManage,
  isPrimary,
  onRename,
}: {
  repo: string;
  name: string;
  canManage: boolean;
  isPrimary: boolean;
  onRename: () => void;
}) {
  const bridge = useBridge();
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const wrapRef = useRef<HTMLSpanElement>(null);
  const timer = useRef<number | null>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('click', onDocClick);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('click', onDocClick);
      window.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const copyPath = () => {
    void navigator.clipboard.writeText(repo);
    setCopied(true);
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      setCopied(false);
      setOpen(false);
    }, 900);
  };

  const remove = () => {
    setOpen(false);
    if (window.confirm(`移除工作区「${name}」？\n只会从列表中移除，不会删除磁盘上的文件。`)) {
      void bridge.removeProject(repo);
    }
  };

  return (
    <span className="p-menu" ref={wrapRef} onClick={(e) => e.stopPropagation()}>
      <span className="p-add" role="button" title="项目操作" onClick={() => setOpen((v) => !v)}>
        ⋯
      </span>
      {open && (
        <span className="p-pop">
          <span className="p-item" role="button" onClick={copyPath}>
            {copied ? '已复制' : '复制路径'}
          </span>
          {canManage && (
            <span
              className="p-item"
              role="button"
              onClick={() => {
                setOpen(false);
                onRename();
              }}
            >
              重命名
            </span>
          )}
          {canManage && !isPrimary && (
            <span
              className="p-item"
              role="button"
              onClick={() => {
                setOpen(false);
                void bridge.restartProject(repo);
              }}
            >
              重启 Runtime
            </span>
          )}
          {canManage && !isPrimary && (
            <span className="p-item danger" role="button" onClick={remove}>
              移除项目
            </span>
          )}
        </span>
      )}
    </span>
  );
}

function SessionMenu({
  id,
  title,
  onRename,
}: {
  id: string;
  title: string;
  onRename: () => void;
}) {
  const bridge = useBridge();
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const wrapRef = useRef<HTMLSpanElement>(null);
  const timer = useRef<number | null>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('click', onDocClick);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('click', onDocClick);
      window.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const copyId = () => {
    void bridge.copySessionId(id);
    setCopied(true);
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      setCopied(false);
      setOpen(false);
    }, 900);
  };

  const archive = () => {
    setOpen(false);
    if (window.confirm(`归档会话「${title || id}」？\n归档后从列表隐藏,记录仍保留。`)) {
      bridge.archiveSession(id);
    }
  };

  return (
    <span className="p-menu" ref={wrapRef} onClick={(e) => e.stopPropagation()}>
      <span className="p-add" role="button" title="会话操作" onClick={() => setOpen((v) => !v)}>
        ⋯
      </span>
      {open && (
        <span className="p-pop">
          <span className="p-item" role="button" onClick={copyId}>
            {copied ? '已复制' : '复制 Session ID'}
          </span>
          <span
            className="p-item"
            role="button"
            onClick={() => {
              setOpen(false);
              onRename();
            }}
          >
            重命名
          </span>
          <span
            className="p-item"
            role="button"
            onClick={() => {
              setOpen(false);
              bridge.forkSession(id);
            }}
          >
            分叉会话
          </span>
          <span
            className="p-item"
            role="button"
            onClick={() => {
              setOpen(false);
              void bridge.exportSession(id, title);
            }}
          >
            导出会话
          </span>
          <span className="p-item danger" role="button" onClick={archive}>
            归档
          </span>
          <span
            className="p-item danger"
            role="button"
            onClick={() => {
              setOpen(false);
              if (window.confirm(`删除会话「${title || id}」？`)) bridge.deleteSession(id);
            }}
          >
            删除
          </span>
        </span>
      )}
    </span>
  );
}

function RenameInput({
  initial,
  onSubmit,
  onCancel,
}: {
  initial: string;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  const cancelled = useRef(false);
  return (
    <input
      className="p-rename"
      defaultValue={initial}
      autoFocus
      onFocus={(e) => e.currentTarget.select()}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === 'Enter') e.currentTarget.blur();
        if (e.key === 'Escape') {
          cancelled.current = true;
          onCancel();
        }
      }}
      onBlur={(e) => {
        if (!cancelled.current) onSubmit(e.target.value);
      }}
    />
  );
}

function useSessionId(): string | null {
  return useAppState().current?.id ?? null;
}

function NoSession() {
  return <div className="rail-empty">进入会话后可用 —— 面板数据按当前会话定位仓库。</div>;
}

function FilesPanel() {
  const sessionId = useSessionId();
  const openFile = useOpenFile();
  const [files, setFiles] = useState<string[] | null>(null);
  const [filesTruncated, setFilesTruncated] = useState(false);
  const [filter, setFilter] = useState('');
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    if (!sessionId) return;
    listFiles(sessionId)
      .then((r) => {
        setFiles(r.files);
        setFilesTruncated(r.truncated);
        setError(null);
      })
      .catch((err: unknown) => {
        setFilesTruncated(false);
        setError(err instanceof Error ? err.message : String(err));
      });
  }, [sessionId]);

  useEffect(() => {
    setFiles(null);
    setFilesTruncated(false);
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
      {filesTruncated && <div className="rail-empty">仓库较大，仅显示部分文件。</div>}
      {files !== null && shown.length === 0 && <div className="rail-empty">无匹配文件。</div>}
      {files !== null && shown.length > 0 && (
        <FileTree paths={shown} filtering={filter.trim().length > 0} onOpen={openFile} />
      )}
    </div>
  );
}

interface TreeNode {
  name: string;
  path: string;
  dir: boolean;
  children: TreeNode[];
}

function buildTree(paths: string[]): TreeNode[] {
  const root: TreeNode = { name: '', path: '', dir: true, children: [] };
  for (const full of paths) {
    const parts = full.split('/');
    let node = root;
    parts.forEach((part, i) => {
      const isLeaf = i === parts.length - 1;
      const path = parts.slice(0, i + 1).join('/');
      let child = node.children.find((c) => c.name === part && c.dir === !isLeaf);
      if (!child) {
        child = { name: part, path, dir: !isLeaf, children: [] };
        node.children.push(child);
      }
      node = child;
    });
  }
  const sort = (nodes: TreeNode[]) => {
    nodes.sort((a, b) => (a.dir !== b.dir ? (a.dir ? -1 : 1) : a.name.localeCompare(b.name)));
    for (const n of nodes) if (n.dir) sort(n.children);
  };
  sort(root.children);
  return root.children;
}

function FileTree({
  paths,
  filtering,
  onOpen,
}: {
  paths: string[];
  filtering: boolean;
  onOpen: (path: string) => void;
}) {
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const tree = useMemo(() => buildTree(paths.slice(0, 4000)), [paths]);

  const toggle = (path: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });

  const rows: ReactNode[] = [];
  const walk = (nodes: TreeNode[], depth: number) => {
    for (const node of nodes) {
      const pad = { paddingLeft: `${8 + depth * 12}px` };
      if (node.dir) {
        const open = filtering || expanded.has(node.path);
        rows.push(
          <button className="ft-dir" key={`d:${node.path}`} style={pad} onClick={() => toggle(node.path)}>
            <span className="ft-caret">{open ? '▾' : '▸'}</span>
            <span className="ft-name">{node.name}</span>
          </button>,
        );
        if (open) walk(node.children, depth + 1);
      } else {
        rows.push(
          <button
            className="ft-file"
            key={`f:${node.path}`}
            style={pad}
            title={node.path}
            onClick={() => onOpen(node.path)}
          >
            <span className="ft-name">{node.name}</span>
          </button>,
        );
      }
    }
  };
  walk(tree, 0);

  return <div className="ft">{rows}</div>;
}

function SearchPanel() {
  const sessionId = useSessionId();
  const openFile = useOpenFile();
  const [query, setQuery] = useState('');
  const [matches, setMatches] = useState<SearchMatch[] | null>(null);
  const [matchesTruncated, setMatchesTruncated] = useState(false);
  const [searching, setSearching] = useState(false);

  useEffect(() => {
    if (!sessionId || !query.trim()) {
      setMatches(null);
      setMatchesTruncated(false);
      return;
    }
    setSearching(true);
    const timer = setTimeout(() => {
      searchFiles(sessionId, query.trim())
        .then((r) => {
          setMatches(r.matches);
          setMatchesTruncated(r.truncated);
        })
        .catch(() => {
          setMatches([]);
          setMatchesTruncated(false);
        })
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
      {matchesTruncated && <div className="rail-empty">已达到搜索预算，仅显示部分结果。</div>}
      <div className="rp-list">
        {(matches ?? []).map((m, i) => (
          <button className="rp-match" key={`${m.path}:${m.line}:${i}`} onClick={() => openFile(m.path, m.line)}>
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

  if (!sessionId) return null;

  return (
    <div className="rail-panel git-foot">
      <div className="rp-bar">
        <span className="rp-branch">{status?.branch ?? '…'}</span>
        <button type="button" className="rp-refresh" onClick={refresh} title="刷新">
          <RefreshCw {...CTRL_ICON} aria-hidden="true" />
        </button>
      </div>
      {status === null && <div className="rail-empty">加载中…</div>}
      {status !== null && status.files.length === 0 && <div className="rail-empty">工作区干净。</div>}
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
