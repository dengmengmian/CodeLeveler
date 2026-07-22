// 左栏：项目工作台导航。顶部品牌 + 新对话入口；面板切换：
// 会话（按仓库分组的会话列表）/ 文件（仓库文件树，前缀过滤）/
// 搜索（内容搜索）/ Git（工作区改动）。数据走 REST，按当前会话定位仓库；
// 底部 daemon 连接状态。

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { BrandMark } from './BrandMark';
import { ProjectPicker } from './ProjectPicker';
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
      <div className="brand" title="CodeLeveler web · v0.1">
        <BrandMark />
        <div className="name">CodeLeveler</div>
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

// 每个项目组默认只展示最新 N 条会话，超出折叠进「加载更多」。
const SESSION_PREVIEW_COUNT = 5;

function SessionsPanel() {
  const state = useAppState();
  const bridge = useBridge();
  const [closed, setClosed] = useState<ReadonlySet<string>>(new Set());
  // 会话列表展开为全量的项目路径集合（默认都只显示最新 5 条）
  const [expandedSessions, setExpandedSessions] = useState<ReadonlySet<string>>(new Set());
  const [picking, setPicking] = useState(false);
  // 正在 inline 重命名的项目路径（null = 无）
  const [renaming, setRenaming] = useState<string | null>(null);

  // 会话按 repository 分组（组内按 updated_at 倒序），骨架是注册的项目
  // 列表 —— 没有会话或 daemon 离线的项目也显示；不在项目列表里的会话
  // 分组（单项目模式回退）追加在最后。
  const groups = useMemo<ProjectGroup[]>(() => {
    const byRepo = new Map<string, UiSessionSummary[]>();
    for (const s of state.sessions) {
      const repo = s.repository ?? state.repository ?? '';
      const list = byRepo.get(repo);
      if (list) list.push(s);
      else byRepo.set(repo, [s]);
    }
    for (const list of byRepo.values()) {
      list.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
    }
    const out: ProjectGroup[] = [];
    const seen = new Set<string>();
    for (const p of state.projects) {
      seen.add(p.path);
      out.push({ repository: p.path, sessions: byRepo.get(p.path) ?? [] });
    }
    for (const [repo, sessions] of byRepo) {
      if (!seen.has(repo)) out.push({ repository: repo, sessions });
    }
    // 有会话的组按最近活动倒序（沿用旧的首见顺序体验）；无会话的组保持
    // projects 顺序排在最后（sessions 已按 updated_at 倒序，首条即最新）。
    out.sort((a, b) => {
      const ta = a.sessions[0]?.updated_at;
      const tb = b.sessions[0]?.updated_at;
      if (ta && tb) return tb.localeCompare(ta);
      if (ta) return -1;
      if (tb) return 1;
      return 0;
    });
    return out;
  }, [state.sessions, state.projects, state.repository]);

  const toggle = (repo: string) => {
    setClosed((prev) => {
      const next = new Set(prev);
      if (next.has(repo)) next.delete(repo);
      else next.add(repo);
      return next;
    });
  };

  const toggleSessionList = (repo: string) => {
    setExpandedSessions((prev) => {
      const next = new Set(prev);
      if (next.has(repo)) next.delete(repo);
      else next.add(repo);
      return next;
    });
  };

  const projectOf = (repo: string) => state.projects.find((p) => p.path === repo);

  const submitRename = (repo: string, current: string, raw: string) => {
    setRenaming(null);
    const value = raw.trim();
    if (value === current) return;
    // 空串交给后端：清除别名、恢复路径末段的默认名
    void bridge.renameProject(repo, value);
  };

  return (
    <div className="sessions">
      {picking && <ProjectPicker onClose={() => setPicking(false)} />}
      <div className="rail-head">
        <span>项目</span>
        <button className="p-open" title="打开项目（浏览并选择一个仓库目录）" onClick={() => setPicking(true)}>
          ＋ 打开项目
        </button>
      </div>
      {groups.length === 0 && (
        <div className="rail-empty">
          还没有会话。
          <br />
          点击「＋ 新对话」或在下方输入开始。
        </div>
      )}
      {groups.map((g) => {
        const proj = projectOf(g.repository);
        const status = proj?.status ?? null;
        const name = proj?.name ?? repoShortName(g.repository);
        return (
          <div className={`proj${closed.has(g.repository) ? ' closed' : ''}`} key={g.repository || '_'}>
            <button className="proj-head" onClick={() => toggle(g.repository)}>
              <span className="fold">▼</span>
              <span className="pmeta">
                <span className="pname">
                  {status && <i className={`pdot ${status}`} />}
                  {renaming === g.repository ? (
                    <RenameInput
                      initial={name}
                      onSubmit={(value) => submitRename(g.repository, name, value)}
                      onCancel={() => setRenaming(null)}
                    />
                  ) : (
                    name
                  )}
                </span>
                <span className="ppath">{g.repository || '当前仓库'}</span>
              </span>
              <span className="pcount">{g.sessions.length}</span>
              {status === 'offline' ? (
                <span
                  className="p-add restart"
                  role="button"
                  title="重启该项目 daemon"
                  onClick={(e) => {
                    e.stopPropagation();
                    void bridge.restartProject(g.repository);
                  }}
                >
                  ⟳
                </span>
              ) : (
                <span
                  className="p-add"
                  role="button"
                  title="在此项目中新建对话"
                  onClick={(e) => {
                    e.stopPropagation();
                    bridge.newDraft(g.repository || undefined);
                  }}
                >
                  ＋
                </span>
              )}
              <ProjectMenu
                repo={g.repository}
                name={name}
                canManage={!!proj}
                isPrimary={state.projects[0]?.path === g.repository}
                onRename={() => setRenaming(g.repository)}
              />
            </button>
            <div className="proj-sessions">
              {g.sessions.length === 0 && (
                <div className="sess-empty">
                  {status === 'offline' ? 'daemon 离线 —— 点 ⟳ 重启后加载会话' : '暂无会话'}
                </div>
              )}
              {(expandedSessions.has(g.repository)
                ? g.sessions
                : g.sessions.slice(0, SESSION_PREVIEW_COUNT)
              ).map((s) => {
                const dot = statusDot(s.status);
                return (
                  // 单行紧凑式：标题 + 右侧相对时间;状态点只在「运行中/等待
                  // 输入」时出现,完成态不再各占一行刷 COMPLETED 噪声。
                  <button
                    key={s.id}
                    className={`sess${state.current?.id === s.id ? ' active' : ''}`}
                    onClick={() => bridge.selectSession(s.id)}
                    title={`${dot.label} · ${formatRelative(s.updated_at)}`}
                  >
                    {dot.cls !== 'idle' && <i className={`dot ${dot.cls}`} />}
                    <span className="t">{s.goal || '未命名会话'}</span>
                    <span className="ago">{formatRelative(s.updated_at)}</span>
                  </button>
                );
              })}
              {g.sessions.length > SESSION_PREVIEW_COUNT &&
                (expandedSessions.has(g.repository) ? (
                  <button className="sess-more" onClick={() => toggleSessionList(g.repository)}>
                    收起
                  </button>
                ) : (
                  <button className="sess-more" onClick={() => toggleSessionList(g.repository)}>
                    加载更多 {g.sessions.length - SESSION_PREVIEW_COUNT} 个对话
                  </button>
                ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}

// ── 项目「⋯」菜单：复制路径 / 重命名 / 移除工作区 ──────────────────────
// 仿 ThemeMenu 模式：open 状态 + 外部点击 / Escape 关闭。proj-head 本身是
// <button>，内部沿用 .p-add 的 span[role=button] 惯例避免嵌套按钮。

function ProjectMenu({
  repo,
  name,
  canManage,
  isPrimary,
  onRename,
}: {
  repo: string;
  name: string;
  /** 是否在项目列表里（聚合模式）；否则只提供复制路径 */
  canManage: boolean;
  /** primary 项目不可移除 */
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
            <span className="p-item danger" role="button" onClick={remove}>
              移除工作区
            </span>
          )}
        </span>
      )}
    </span>
  );
}

/** inline 重命名输入框：Enter / 失焦提交，Escape 取消。 */
function RenameInput({
  initial,
  onSubmit,
  onCancel,
}: {
  initial: string;
  onSubmit: (value: string) => void;
  onCancel: () => void;
}) {
  // Escape 取消后 input 卸载，失焦回调不得再提交
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
      {files !== null && shown.length > 0 && (
        <FileTree paths={shown} filtering={filter.trim().length > 0} onOpen={openFile} />
      )}
    </div>
  );
}

// ── 文件树 ──────────────────────────────────────────────────────────
// 平铺路径拼成目录树；目录可折叠。过滤时全部展开只显示命中路径的分支。

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
    nodes.sort((a, b) =>
      a.dir !== b.dir ? (a.dir ? -1 : 1) : a.name.localeCompare(b.name),
    );
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
  // 默认全部折叠（大仓库友好）；用户展开的目录记在集合里。过滤时忽略折叠状态。
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
      const pad = { paddingLeft: `${6 + depth * 12}px` };
      if (node.dir) {
        const open = filtering || expanded.has(node.path);
        rows.push(
          <button
            className="ft-dir"
            key={`d:${node.path}`}
            style={pad}
            onClick={() => toggle(node.path)}
          >
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
