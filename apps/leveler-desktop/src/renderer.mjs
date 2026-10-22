import { DesktopTerminals } from './terminal.mjs';
import { renderMarkdown } from './markdown.mjs';
import {
  modelLabel, workspaceLabel, toolLabel, toolTarget, browserActivity, sourceErrorSummary,
  relativeActivity, permissionLabel, recentWorkspaces, renameCommand, messageKind,
  historyQuestions, planStepLabel, planProgressLabel, browserChrome, browserSurfaceBounds, workspaceTaskGroups,
  attachmentErrorPresentation, slashIntent, commandCandidates,
  contractToolStatus, failureReason, displayPreview,
  confirmedDiffOf, diffCounts, messageTime, conversationItems, executionStatusLabel, confirmedChangeFiles,
  mergeExecutionViews,
} from './presentation.mjs';
import {
  projectSnapshot, applySessionMetadata, applyEvent, commandEnvelope, terminalEvents,
  approvalDecisions, clarificationAnswer, uploadDraft, applyUploadEvent, reconcileUploadReply,
  workspaceDraftText, attachmentSubmissionError, permissionCommand, permissionConfirmed,
  beginDiffQuery, expireDiffQuery, retainInterruptedThoughts, mergeRestoredConversation,
} from './state.mjs';
import { brandMark, fileIcon } from './icons.mjs';
import { projectTrajectory, searchTrajectory, buildTimeline, formatDuration, trajectoryWindow } from './trajectory.mjs';
import {
  el, iconNode, iconButton, installTooltips, toast, openMenu, closeMenu, openPopover, closePopover, confirmDialog,
  copyText, unconnected, trapTab,
} from './ui.mjs';

const api = window.desktop;
const $ = id => document.getElementById(id);
const root = document.documentElement;
root.dataset.platform = api.platform === 'darwin' ? 'darwin' : api.platform === 'win32' ? 'win32' : 'linux';
const defaultFonts = {
  text: getComputedStyle(root).getPropertyValue('--font-text').trim(),
  code: getComputedStyle(root).getPropertyValue('--font-code').trim(),
  terminal: getComputedStyle(root).getPropertyValue('--terminal-font').trim(),
};

const LABELS = {
  idle: '就绪', running: '运行中', waiting_user: '等待你处理', answered: '已回答',
  completed: '已完成', failed: '失败', cancelled: '已取消', interrupted: '已中断',
  blocked: '受阻', incomplete: '未完成', unknown: '未知',
};
const TAB_ICON = {
  start: 'compass', files: 'folder', preview: 'file', terminal: 'terminal',
  browser: 'globe', changes: 'diff', plan: 'list', deliverables: 'archive', subagent: 'bot',
  schedule: 'clock',
};
const PERMISSION_MODES = [
  ['full_access', '完全权限'],
  ['assisted', '工作区内修改'],
  ['request_approval', '需批准'],
];

let tasks = [];
let sourceErrors = [];
let current = null;
let currentSource = null;
let selection = null;
let history = [];
let loading = false;
let busy = 0;
let opening = 0;
let uncertain = null;
// Connection is transport truth from Main, never inferred from task status.
let connectionState = { state: 'connecting' };
// Highest durable sequence already applied. A reconnect snapshot is a
// request/response that can be older than events already streamed to this
// page; an older snapshot must never replace newer state.
let lastEventSeq = -1;
let pendingCreation = null;
let runStartedAt = null;
let runClockTimer = null;
let paintPending = false;
let attachmentEarly = null;
let uploadingKey = null;
let titleEditing = false;
let todoDockOpen = false;
let todoDockSessionId = null;
let lastEscapeAt = 0;
let titleComposing = false;
let findMatches = [];
let findSelected = 0;
let sidebarQuery = '';
let contentSearch = { query: '', status: 'idle', results: [], scanned: 0, total: 0 };
let contentSearchSeq = 0;
let contentSearchTimer = null;
const chosenWorkspaces = new Map();
function workspaceKey(path) {
  if (typeof path !== 'string') return path;
  return path.replace(/^\/(tmp|var|etc)\//, '/private/$1/');
}
const clarificationReplies = new Map();
let entryPage = null;
let settingsSection = 'general';
let fontSettingsOpen = false;
let agentsRead = null;
let headerMenuButton = null;
let headerSubagentButton = null;
let headerJobsButton = null;
let wideSidebarCollapsed = false;
let activeView = 'chat';
let liveTrajectory = [];
let liveTurnStartAt = null;
let liveTurnClosed = false;
let trajectoryQuery = '';
let trajectoryActualDuration = false;
let trajectoryCollapseTurns = false;
let trajectoryCollapseCalls = false;
let trajectorySelectedId = null;
let trajectoryDetailTab = 'overview';
let trajectoryDetailWidth = 380;
let trajectoryPinned = true;
let trajectoryNodeLimit = 50;
let trajectoryRange = null;
let trajectoryRangeDrag = null;
let trajectoryPanDrag = null;
let trajectoryPanMoved = false;
let trajectoryViewport = null;
let openInAppState = { apps: null, loading: false, error: null, preferred: null };

const drafts = new Map();
const DRAFTS_KEY = 'codeleveler.drafts.v1';
// Drafts are user input, not Runtime facts: keep the text per session across a
// Desktop restart. Attachments are intentionally not persisted.
function loadDrafts() {
  try {
    const stored = JSON.parse(localStorage.getItem(DRAFTS_KEY) || '{}');
    for (const [key, value] of Object.entries(stored)) if (typeof value === 'string' && value) drafts.set(key, value.slice(0, 200000));
  } catch { /* unreadable storage gives an empty draft set */ }
}
function saveDrafts() {
  try {
    const stored = {};
    for (const [key, value] of drafts) if (value) stored[key] = value.slice(0, 200000);
    localStorage.setItem(DRAFTS_KEY, JSON.stringify(stored));
  } catch { /* a full store must not break typing */ }
}
function clearDraft() { drafts.delete(draftKey()); saveDrafts(); }
loadDrafts();
let queuedMessages = [];
// A submit that the Runtime acknowledged but whose first status event has not
// reached the page yet is still work that a quit would interrupt.
let pendingSubmit = false;
// Transient retry status: the runtime marks these as ephemeral, so they sit in
// the running row rather than creating a transcript item.
let modelRetry = null;
const attachmentDrafts = new Map();
const renderedSources = new WeakMap();
const openExecution = new Set();
const openTools = new Set();
const executionState = new WeakMap();
const foldedGroups = new Set();

/* ------------------------------------------------------------ preferences */

const SETTINGS_KEY = 'codeleveler.settings.v1';
const settings = loadSettings();

function loadSettings() {
  const defaults = {
    theme: 'system', fontSize: 14, codeFontSize: 11, terminalFontSize: 13,
    textFontFamily: '', codeFontFamily: '', terminalFontFamily: '',
    linkOpen: 'side', busySend: 'queue', performance: 'detailed', codingView: true,
    defaultPermission: 'assisted', archivedFilter: 'default', groupMode: 'workspace',
    sortMode: 'updated', manualOrder: { sessions: [], groups: [] },
  };
  try {
    const stored = JSON.parse(localStorage.getItem(SETTINGS_KEY) || '{}');
    const merged = { ...defaults, ...(stored && typeof stored === 'object' ? stored : {}) };
    if (merged.performance === 'off') merged.performance = 'compact';
    if (!['compact', 'detailed'].includes(merged.performance)) merged.performance = 'detailed';
    if (!['queue', 'steer'].includes(merged.busySend)) merged.busySend = 'queue';
    merged.fontSize = Math.max(10, Math.min(22, Number(merged.fontSize) || 14));
    merged.codeFontSize = Math.max(10, Math.min(16, Number(merged.codeFontSize) || 11));
    merged.terminalFontSize = Math.max(10, Math.min(20, Number(merged.terminalFontSize) || 13));
    return merged;
  } catch { return defaults; }
}
function saveSettings() {
  try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings)); } catch { /* optional */ }
}
const OPEN_APP_KEY = 'codeleveler.open-in-app.v1';
function savedOpenApp() {
  try { return localStorage.getItem(OPEN_APP_KEY) || null; } catch { return null; }
}
function rememberOpenApp(appId) {
  try { localStorage.setItem(OPEN_APP_KEY, appId); } catch { /* optional */ }
}
function resolvedTheme() {
  return settings.theme === 'system'
    ? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light')
    : settings.theme;
}
function applyTheme() { root.dataset.theme = resolvedTheme(); }
function applyContentFont() {
  const size = Number(settings.fontSize) || 14;
  root.style.setProperty('--content-font-size', `${size}px`);
  root.style.setProperty('--content-font-delta', `${size - 14}px`);
  root.style.setProperty('--code-font-size', `${Number(settings.codeFontSize) || 11}px`);
  root.style.setProperty('--terminal-font-size', `${Number(settings.terminalFontSize) || 13}px`);
  const family = (value, fallback) => value?.trim() ? `${value.trim()}, ${fallback}` : fallback;
  root.style.setProperty('--font-text', family(settings.textFontFamily, defaultFonts.text));
  root.style.setProperty('--font-code', family(settings.codeFontFamily, defaultFonts.code));
  root.style.setProperty('--terminal-font', family(settings.terminalFontFamily, defaultFonts.terminal));
}
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  if (settings.theme === 'system') applyTheme();
});

/* --------------------------------------------------------------- app state */

function isHome() {
  return !current || (!current.messages.length && !current.tools.length
    && !current.approvals.length && !current.clarifications.length);
}
function hasWorkspace() {
  return typeof current?.session.repository === 'string' && current.session.repository !== '';
}
function attachments() { return attachmentDrafts.get(draftKey()) ?? []; }
function draftKey() { return current?.session.id ?? 'new-session'; }
function uploadPending() {
  return attachments().some(item => ['uploading', 'queued', 'unknown'].includes(item.status));
}
function stashDraft() { drafts.set(draftKey(), $('message').value); saveDrafts(); }

function controls() {
  const uploads = uploadPending();
  const hasContent = $('message').value.trim() !== '' || attachments().some(item => item.status === 'ready');
  const running = ['running', 'waiting_user'].includes(current?.status);
  const offline = connectionState.state !== 'connected';
  const frozen = loading || !!uncertain || uploads || offline;
  $('send').disabled = frozen || !hasContent;
  $('attachment-add').disabled = frozen;
  $('model-picker').disabled = frozen;
  $('permission-picker').disabled = frozen;
  $('workspace-picker').disabled = frozen;
  $('new-session').disabled = loading || uploads || offline;
  $('leading-new').disabled = $('new-session').disabled;
  $('stop').hidden = !running;
  $('stop').disabled = loading;
  $('send').hidden = running && !hasContent;
  for (const button of document.querySelectorAll('#workspace-list .session-row, #workspace-list .group-new, .pane-tab, .add-tab')) {
    button.disabled = loading || !!uncertain || uploads;
  }
  for (const button of document.querySelectorAll('.takeover button')) button.disabled = loading || !!uncertain;
  $('conversation-status').textContent = loading ? '连接 Runtime…'
    : current ? (current.approvals.length || current.clarifications.length
      ? '等待你处理' : current.activity || LABELS[current.status] || current.status) : '';
}

// One visible state, sourced from the bridge's real transport lifecycle.
function applyConnection(frame) {
  connectionState = { ...frame };
  const banner = $('connection-banner');
  if (banner) {
    if (frame.state === 'connected') {
      banner.hidden = true;
      banner.replaceChildren();
      if (frame.resumed) toast('已重新连接 Runtime', { tone: 'success', holdMs: 2200 });
    } else {
      const text = frame.state === 'connecting' ? '正在连接 Runtime…'
        : frame.state === 'restoring' ? '连接已恢复，正在恢复会话…'
        : frame.state === 'restore_failed' ? `会话恢复失败：${frame.message ?? '未知错误'}`
        : frame.state === 'failed' ? (frame.message ?? '无法连接 Runtime')
        : `连接已断开，正在重连${frame.attempt ? `（第 ${frame.attempt} 次）` : ''}…`;
      banner.replaceChildren(el('span', 'connection-text', text));
      if (frame.state === 'failed' || frame.state === 'restore_failed') {
        const retry = el('button', 'button secondary', '重试');
        retry.type = 'button';
        retry.addEventListener('click', () => action(() => api.reconnect()));
        banner.append(retry);
      }
      banner.hidden = false;
    }
  }
  controls();
}

async function action(callback, { onError = null, clearGlobal = true } = {}) {
  if (clearGlobal) showError('');
  busy += 1;
  loading = true;
  controls();
  try {
    return await callback();
  } catch (error) {
    if (onError) onError(error.message);
    else showError(error.message);
  } finally {
    loading = --busy > 0;
    controls();
  }
}

function showError(message) {
  const banner = $('error-banner');
  banner.textContent = message;
  banner.hidden = !message;
}
function composerError(message) {
  const node = $('composer-error');
  node.textContent = message;
  node.hidden = !message;
}

/* --------------------------------------------------------------- sidebar */

function manualSessionIds() {
  return Array.isArray(settings.manualOrder?.sessions) ? settings.manualOrder.sessions : [];
}

function manualGroupPaths() {
  return Array.isArray(settings.manualOrder?.groups) ? settings.manualOrder.groups : [];
}

function sortedTasks() {
  const mode = settings.sortMode ?? 'updated';
  if (mode === 'manual') {
    const order = manualSessionIds();
    const rank = new Map(order.map((id, index) => [id, index]));
    return [...tasks].sort((a, b) => (rank.get(a.id) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.id) ?? Number.MAX_SAFE_INTEGER));
  }
  return [...tasks].sort((a, b) => {
    if (mode === 'name') return String(a.title ?? '').localeCompare(String(b.title ?? ''), 'zh-CN');
    const key = mode === 'created' ? 'created_at' : 'last_activity_at';
    const av = Date.parse(a[key] ?? a.updated_at ?? '') || 0;
    const bv = Date.parse(b[key] ?? b.updated_at ?? '') || 0;
    return bv - av;
  });
}

function visibleTasks() {
  const archived = settings.archivedFilter ?? 'default';
  return sortedTasks().filter(task => {
    const isArchived = task.archived === true || !!task.archived_at;
    if (archived === 'default' && isArchived) return false;
    if (archived === 'only' && !isArchived) return false;
    if (sidebarQuery && !`${task.title ?? ''} ${task.primary_workspace ?? ''}`.toLowerCase().includes(sidebarQuery.toLowerCase())) return false;
    return true;
  });
}

const SIDEBAR_SESSION_LIMIT = 5;
const groupLimits = new Map();
const sessionStatusLabels = {
  running: '运行中', waiting_user: '等待你处理', answered: '已回答', idle: '就绪',
  completed: '已完成', failed: '失败', cancelled: '已取消', unknown: '状态未知',
};

function childIsSettled(child) {
  return child?.done === true || child?.state === 'settled';
}

function treeGroups(tasks) {
  const pathOrder = [];
  const byPath = new Map();
  for (const task of tasks) {
    const path = typeof task.primary_workspace === 'string' && task.primary_workspace ? task.primary_workspace : null;
    if (!path) continue;
    if (!byPath.has(path)) {
      byPath.set(path, { path, name: workspaceLabel(path), depth: 0, tasks: [], children: [], parent: null });
      pathOrder.push(path);
    }
    byPath.get(path).tasks.push(task);
  }
  const nodes = pathOrder.map(path => byPath.get(path));
  for (const node of nodes) {
    let parent = null;
    for (const candidate of nodes) {
      if (candidate.path === node.path) continue;
      if (node.path.startsWith(`${candidate.path}/`) && (!parent || candidate.path.length > parent.path.length)) parent = candidate;
    }
    node.parent = parent;
    if (parent) parent.children.push(node);
  }
  const roots = nodes.filter(node => !node.parent);
  const out = [];
  const walk = (node, depth) => {
    node.depth = depth;
    out.push(node);
    for (const child of node.children) walk(child, depth + 1);
  };
  for (const root of roots) walk(root, 0);
  return out;
}

function groupIsHidden(path) {
  for (const folded of foldedGroups) {
    if (path === folded || path.startsWith(`${folded}/`)) return true;
  }
  return false;
}

function applyManualSessionOrder(ids) {
  settings.manualOrder = { ...(settings.manualOrder ?? {}), sessions: ids };
  settings.sortMode = 'manual';
  saveSettings();
}

function reorderManualSession(draggedId, targetId, groupTasks) {
  const groupIds = groupTasks.map(task => task.id);
  const ids = manualSessionIds().filter(id => groupIds.includes(id));
  for (const id of groupIds) if (!ids.includes(id)) ids.push(id);
  const from = ids.indexOf(draggedId);
  const to = ids.indexOf(targetId);
  if (from < 0 || to < 0 || from === to) return;
  ids.splice(to, 0, ids.splice(from, 1)[0]);
  // Keep tasks from other groups in their existing relative order.
  const others = manualSessionIds().filter(id => !groupIds.includes(id));
  applyManualSessionOrder([...others, ...ids]);
}

function reorderManualGroup(draggedPath, targetPath) {
  const paths = manualGroupPaths().filter(path => path !== draggedPath);
  const target = paths.indexOf(targetPath);
  if (target < 0) return;
  paths.splice(target, 0, draggedPath);
  settings.manualOrder = { ...(settings.manualOrder ?? {}), groups: paths };
  settings.sortMode = 'manual';
  saveSettings();
}

function renderSidebar() {
  const list = $('workspace-list');
  list.replaceChildren();
  hideHoverCard();
  const visible = visibleTasks();
  const mode = settings.groupMode ?? 'workspace';
  if (sidebarQuery) {
    const q = sidebarQuery.trim().toLowerCase();
    const titleMatches = visible.slice(0, 20);
    if (titleMatches.length) {
      list.append(el('p', 'sidebar-search-note', '会话'));
      for (const task of titleMatches) list.append(sessionRow(task, null));
    }
    if (visible.length > 20) list.append(el('p', 'sidebar-search-note', `标题匹配仅显示前 20 条，请缩小搜索范围。`));
    if (contentSearch.query === q) {
      if (contentSearch.status === 'searching') {
        list.append(el('p', 'sidebar-search-note', `正在搜索会话内容…（${contentSearch.scanned}/${contentSearch.total}）`));
      } else if (contentSearch.status === 'done') {
        if (contentSearch.results.length) {
          list.append(el('p', 'sidebar-search-note', '内容'));
          for (const result of contentSearch.results) list.append(contentResultRow(result));
        }
        list.append(el('p', 'sidebar-search-note', '内容搜索仅覆盖当前会话；其他会话内容未读取。'));
      }
    }
    if (!titleMatches.length && contentSearch.query === q && contentSearch.status === 'done' && !contentSearch.results.length) {
      list.append(el('p', 'workspace-empty', '无匹配会话'));
    }
    return;
  }
  let groups = [];
  if (mode === 'flat') {
    groups.push({ path: null, name: '全部会话', tasks: visible, flat: true });
  } else if (mode === 'tree') {
    const loose = visible.filter(task => !task.primary_workspace);
    if (loose.length) groups.push({ path: null, name: '无工作区', tasks: loose });
    groups.push(...treeGroups(visible));
    const manual = manualGroupPaths();
    if (manual.length) {
      const rank = new Map(manual.map((path, index) => [path, index]));
      groups.sort((a, b) => {
        if (!a.path) return -1;
        if (!b.path) return 1;
        return (rank.get(a.path) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.path) ?? Number.MAX_SAFE_INTEGER);
      });
    }
  } else {
    const grouped = workspaceTaskGroups(visible);
    if (grouped.loose.length) groups.push({ path: null, name: '无工作区', tasks: grouped.loose });
    for (const space of grouped.spaces) groups.push({ path: space.path, name: space.name, tasks: space.tasks });
    const manual = manualGroupPaths();
    if (manual.length) {
      const rank = new Map(manual.map((path, index) => [path, index]));
      groups.sort((a, b) => {
        if (!a.path) return -1;
        if (!b.path) return 1;
        return (rank.get(a.path) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.path) ?? Number.MAX_SAFE_INTEGER);
      });
    }
  }
  if (!visible.length) list.append(el('p', 'workspace-empty', '暂无会话'));
  for (const group of groups) {
    if (mode === 'tree' && group.path && [...foldedGroups].some(key => key !== group.path && group.path.startsWith(`${key}/`))) continue;
    const section = el('section', 'workspace-group');
    if (group.path) section.dataset.workspacePath = group.path;
    if (group.depth) section.style.paddingLeft = `${Math.min(group.depth, 6) * 14}px`;
    const key = group.path ?? '(loose)';
    const collapsed = foldedGroups.has(key) || (mode === 'tree' && group.path ? groupIsHidden(group.path) : false);
    const head = el('div', 'workspace-group-head');
    head.draggable = !!group.path && mode !== 'flat';
    const fold = el('button', 'group-fold');
    fold.type = 'button';
    fold.setAttribute('aria-label', `${collapsed ? '展开' : '折叠'} ${group.name}`);
    fold.setAttribute('aria-expanded', String(!collapsed));
    fold.append(iconNode('chevronRight', { size: 12, className: 'group-chevron' }));
    fold.style.display = 'inline-flex';
    fold.style.alignItems = 'center';
    fold.style.width = '16px';
    fold.style.color = 'var(--text-tertiary)';
    fold.style.transform = collapsed ? 'none' : 'rotate(90deg)';
    fold.addEventListener('click', event => { event.stopPropagation(); toggleGroup(key); });
    const label = el('button', 'group-name');
    label.type = 'button';
    label.setAttribute('aria-expanded', String(!collapsed));
    label.style.display = 'flex';
    label.style.alignItems = 'center';
    label.style.gap = '6px';
    label.style.flex = '1';
    label.style.minWidth = '0';
    label.style.overflow = 'hidden';
    label.style.color = 'inherit';
    label.append(iconNode(group.flat ? 'chat' : group.path ? 'folder' : 'chat', { size: 16 }));
    const name = el('span', 'group-name-text', group.name);
    name.style.overflow = 'hidden';
    name.style.textOverflow = 'ellipsis';
    name.style.whiteSpace = 'nowrap';
    label.append(name);
    if (group.path) label.dataset.tooltip = group.path;
    label.setAttribute('aria-label', group.path ? `${group.name} 工作区` : group.name);
    label.addEventListener('click', () => toggleGroup(key));
    head.append(fold, label);
    if (group.path) {
      const create = el('button', 'group-new');
      create.type = 'button';
      create.setAttribute('aria-label', `在 ${group.name} 新建会话`);
      create.dataset.tooltip = '在此工作区新建会话';
      create.append(iconNode('plus', { size: 16 }));
      create.addEventListener('click', event => { event.stopPropagation(); action(() => createTaskInWorkspace(group.path)); });
      const more = el('button', 'group-more');
      more.type = 'button';
      more.setAttribute('aria-label', `工作区操作：${group.name}`);
      more.dataset.tooltip = '工作区操作';
      more.append(iconNode('more', { size: 16 }));
      more.addEventListener('click', event => {
        event.stopPropagation();
        openMenu({
          anchor: more, width: 240, align: 'end',
          items: [
            { label: '复制工作区路径', icon: 'copy', onSelect: () => copyText(api, group.path) },
            { label: '重命名工作区', icon: 'edit', disabled: true, badge: '未接入', title: 'Runtime 未提供工作区重命名命令' },
            { label: '删除工作区', icon: 'trash', disabled: true, badge: '未接入', title: 'Runtime 未提供工作区删除命令' },
          ],
        });
      });
      head.append(more, create);
      head.addEventListener('mouseenter', () => showHoverCard(head, () => ({
        title: group.name,
        lines: [homeAbbreviated(group.path)],
        copy: group.path,
      })));
      head.addEventListener('mouseleave', hideHoverCard);
      head.addEventListener('dragstart', event => {
        if (!head.draggable) return;
        event.dataTransfer?.setData('text/leveler-group', group.path);
        event.dataTransfer.effectAllowed = 'move';
        section.classList.add('dragging');
      });
      head.addEventListener('dragend', () => section.classList.remove('dragging'));
      head.addEventListener('dragover', event => {
        if (!head.draggable) return;
        event.preventDefault();
        head.classList.add('drag-over');
      });
      head.addEventListener('dragleave', () => head.classList.remove('drag-over'));
      head.addEventListener('drop', event => {
        head.classList.remove('drag-over');
        const dragged = event.dataTransfer?.getData('text/leveler-group');
        if (dragged && dragged !== group.path) { event.preventDefault(); reorderManualGroup(dragged, group.path); renderSidebar(); }
      });
    }
    section.append(head);
    if (!collapsed) {
      const sessions = el('div', 'workspace-sessions');
      const limit = groupLimits.get(key) ?? SIDEBAR_SESSION_LIMIT;
      const expanded = limit >= group.tasks.length;
      const shown = group.tasks.slice(0, limit);
      for (const task of shown) sessions.append(sessionRow(task, group));
      if (!group.tasks.length && group.depth === undefined) sessions.append(el('p', 'workspace-empty', '暂无会话'));
      if (group.tasks.length > SIDEBAR_SESSION_LIMIT) {
        const rest = el('button', 'group-overflow');
        rest.type = 'button';
        rest.setAttribute('aria-expanded', String(expanded));
        rest.textContent = expanded ? '收起' : `展开其余 ${group.tasks.length - limit} 个会话`;
        rest.addEventListener('click', () => {
          if (expanded) groupLimits.delete(key);
          else groupLimits.set(key, limit + SIDEBAR_SESSION_LIMIT);
          renderSidebar();
        });
        sessions.append(rest);
      }
      section.append(sessions);
    }
    list.append(section);
  }
}

function contentResultRow(result) {
  const row = el('button', 'sidebar-content-result');
  row.type = 'button';
  row.append(el('span', 'sidebar-content-title', result.title || '当前会话'), el('span', 'session-snippet', result.snippet));
  row.addEventListener('click', () => {
    const node = [...document.querySelectorAll('#conversation [data-message-id]')].find(item => item.dataset.messageId === result.messageId);
    node?.scrollIntoView({ block: 'center', behavior: 'smooth' });
    node?.classList.add('flash');
    setTimeout(() => node?.classList.remove('flash'), 1200);
  });
  return row;
}

function runContentSearch(query) {
  const q = query.trim().toLowerCase();
  contentSearchSeq += 1;
  if (!q) {
    contentSearch = { query: '', status: 'idle', results: [], scanned: 0, total: 0 };
    return;
  }
  const results = [];
  const title = current?.session?.title || '当前会话';
  for (const message of current?.messages ?? []) {
    const text = String(message.text ?? '');
    const at = text.toLowerCase().indexOf(q);
    if (at < 0) continue;
    const start = Math.max(0, at - 40);
    const end = Math.min(text.length, at + q.length + 60);
    results.push({ messageId: message.id, title, snippet: `${start ? '…' : ''}${text.slice(start, end)}${end < text.length ? '…' : ''}` });
    if (results.length >= 20) break;
  }
  contentSearch = { query: q, status: 'done', results, scanned: current ? 1 : 0, total: current ? 1 : 0 };
  renderSidebar();
}

function toggleGroup(key) {
  if (foldedGroups.has(key)) foldedGroups.delete(key); else foldedGroups.add(key);
  renderSidebar();
}

let hoverCardTimer = null;
let hoverCardNode = null;

function hideHoverCard() {
  clearTimeout(hoverCardTimer);
  hoverCardTimer = null;
  if (hoverCardNode) { hoverCardNode.remove(); hoverCardNode = null; }
}

function homeAbbreviated(path) {
  return String(path ?? '').replace(/^\/Users\/[^/]+/, '~');
}

function showHoverCard(anchor, build) {
  hideHoverCard();
  if (document.querySelector('.menu, .header-popover')) return;
  hoverCardTimer = setTimeout(() => {
    const content = build();
    if (!content) return;
    const card = el('div', 'sidebar-hover-card');
    card.setAttribute('role', 'button');
    card.tabIndex = 0;
    if (content.copy) card.setAttribute('aria-label', `复制: ${content.copy}`);
    card.append(el('div', 'hover-card-title', content.title));
    for (const line of content.lines ?? []) card.append(el('div', 'hover-card-line', line));
    if (content.copy) {
      const copy = async () => {
        if (window.getSelection()?.toString()) return;
        await copyText(api, content.copy, { quiet: true });
        card.replaceChildren(el('div', 'hover-card-copied', '已复制'));
      };
      card.addEventListener('click', copy);
      card.addEventListener('keydown', event => {
        if (event.key !== 'Enter' && event.key !== ' ') return;
        event.preventDefault();
        copy();
      });
    }
    document.body.append(card);
    const rect = anchor.getBoundingClientRect();
    card.style.left = `${Math.round(Math.min(innerWidth - card.offsetWidth - 8, rect.left + rect.width + 8))}px`;
    card.style.top = `${Math.round(Math.max(8, Math.min(innerHeight - card.offsetHeight - 8, rect.top)))}px`;
    hoverCardNode = card;
  }, 800);
}

function sessionRow(task, group = null) {
  const row = el('div', 'session-row');
  row.setAttribute('role', 'treeitem');
  row.dataset.sessionId = task.id;
  row.dataset.status = task.status ?? 'unknown';
  row.setAttribute('aria-selected', String(task.id === current?.session.id));
  row.tabIndex = 0;
  const archived = task.archived === true || !!task.archived_at;
  row.draggable = !archived;
  const dot = el('span', 'session-status-dot');
  dot.dataset.status = task.status ?? 'unknown';
  dot.title = sessionStatusLabels[task.status] ?? sessionStatusLabels.unknown;
  const title = el('span', 'session-title', task.title || '未命名会话');
  if (archived) title.style.color = 'var(--text-caption)';
  const meta = el('span', 'session-meta', archived ? '已归档' : relativeActivity(task.last_activity_at ?? task.updated_at));
  const actions = el('span', 'session-row-actions');
  const pin = el('button', 'icon-button session-action');
  pin.type = 'button';
  pin.disabled = true;
  pin.title = '置顶未接入：Runtime 未提供置顶命令';
  pin.setAttribute('aria-label', '置顶（未接入）');
  pin.append(iconNode('pin', { size: 14 }));
  const archive = el('button', 'icon-button session-action');
  archive.type = 'button';
  archive.setAttribute('aria-label', `归档 ${task.title || '未命名会话'}`);
  archive.title = '归档会话';
  archive.append(iconNode('archive', { size: 14 }));
  archive.addEventListener('click', event => { event.stopPropagation(); archiveTask(task); });
  actions.append(archive, pin);
  const more = el('button', 'icon-button session-more');
  more.type = 'button';
  more.setAttribute('aria-label', `会话操作：${task.title || '未命名会话'}`);
  more.append(iconNode('more', { size: 16 }));
  more.addEventListener('click', event => { event.stopPropagation(); openSessionMenu(task, more); });
  const copy = el('span', 'session-copy');
  copy.append(title);
  if (sidebarQuery && task.primary_workspace) copy.append(el('span', 'session-snippet', task.primary_workspace));
  if (sidebarQuery) row.classList.add('search-result');
  row.append(dot, copy, meta, more, actions);
  row.addEventListener('click', () => {
    if (loading || uncertain || uploadPending()) return;
    if (task.id === current?.session.id) { closeNarrowSidebar(); return; }
    action(() => openTask(task));
  });
  row.addEventListener('dblclick', event => {
    event.preventDefault();
    event.stopPropagation();
    // The first click of a double click may be opening this (inactive) task,
    // which sets `loading`; retry after that settles and re-query the live
    // title node because opening re-renders the sidebar.
    const begin = () => {
      if (loading || uncertain) { setTimeout(begin, 50); return; }
      const liveRow = [...document.querySelectorAll('#workspace-list .session-row')]
        .find(node => node.dataset.sessionId === task.id);
      const liveTitle = liveRow?.querySelector('.session-title');
      if (liveTitle) beginSidebarRename(task, liveTitle);
    };
    begin();
  });
  row.addEventListener('keydown', event => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    row.click();
  });
  row.addEventListener('contextmenu', event => { event.preventDefault(); openSessionMenu(task, more); });
  row.addEventListener('mouseenter', () => showHoverCard(row, () => ({
    title: task.title || '未命名会话',
    lines: [
      relativeActivity(task.last_activity_at ?? task.updated_at),
      task.status === 'running' ? '进行中' : task.status === 'waiting_user' ? '等待你处理' : '',
      archived ? '已归档' : '',
    ].filter(Boolean),
    copy: task.title || '',
  })));
  row.addEventListener('mouseleave', hideHoverCard);
  row.addEventListener('dragstart', event => {
    if (!row.draggable) return;
    event.dataTransfer?.setData('text/leveler-session', task.id);
    event.dataTransfer.effectAllowed = 'move';
    row.classList.add('dragging');
  });
  row.addEventListener('dragend', () => row.classList.remove('dragging'));
  row.addEventListener('dragover', event => {
    if (!row.draggable) return;
    event.preventDefault();
    row.classList.add('drag-over');
  });
  row.addEventListener('dragleave', () => row.classList.remove('drag-over'));
  row.addEventListener('drop', event => {
    row.classList.remove('drag-over');
    const dragged = event.dataTransfer?.getData('text/leveler-session');
    if (dragged && dragged !== task.id && group) {
      event.preventDefault();
      reorderManualSession(dragged, task.id, group.tasks);
      renderSidebar();
    }
  });
  return row;
}

function beginSidebarRename(task, titleNode) {
  if (document.querySelector('.sidebar-rename-input')) return;
  const input = el('input', 'sidebar-rename-input');
  input.value = task.title || '';
  input.maxLength = 256;
  input.setAttribute('aria-label', '会话标题');
  titleNode.replaceWith(input);
  input.focus();
  input.select();
  let finished = false;
  const finish = async commit => {
    if (finished) return;
    finished = true;
    const name = input.value.trim();
    input.replaceWith(titleNode);
    if (!commit || !name || name === task.title) { renderSidebar(); return; }
    try {
      await action(async () => {
        const snapshot = await api.snapshot(task.id);
        const command = renameCommand(task.id, name);
        await api.deliver(commandEnvelope(task.id, command, snapshot.last_sequence ?? 0));
        await refreshTasks();
        if (current?.session.id === task.id) await refreshMetadata();
        // The task index can trail the confirmed rename by one refresh. The
        // Runtime accepted this exact title; show it now rather than leaving
        // the old title until an unrelated refresh.
        const renamed = tasks.find(item => item.id === task.id);
        if (renamed && renamed.title !== name) renamed.title = name;
        renderSidebar();
        toast('标题已更新', { tone: 'success', holdMs: 1600 });
      });
    } catch (error) {
      toast(error.message, { tone: 'error' });
      renderSidebar();
    }
  };
  input.addEventListener('keydown', event => {
    if (event.isComposing) return;
    if (event.key === 'Enter') { event.preventDefault(); finish(true); }
    else if (event.key === 'Escape') { event.preventDefault(); finish(false); }
  });
  input.addEventListener('blur', () => { if (!finished) finish(false); });
}

function openSessionMenu(task, anchor) {
  const archived = task.archived === true || !!task.archived_at;
  openMenu({
    anchor,
    width: 220,
    items: [
      { label: '置顶', icon: 'pin', disabled: true, badge: '未接入' },
      // The title editor refuses to start while an action owns the loading
      // guard, so open the task first and enter editing only after it clears.
      { label: '重命名', icon: 'edit', onSelect: () => action(async () => { await openTask(task); }).then(() => { if (current?.session.id === task.id) beginTitleEdit(); }) },
      { label: '分叉会话', icon: 'fork', onSelect: () => action(() => forkTask(task)) },
      archived
        ? { label: '取消归档', icon: 'archive', disabled: true, badge: '未接入' }
        : { label: '归档', icon: 'archive', onSelect: () => archiveTask(task) },
      { label: '复制会话 ID', icon: 'copy', onSelect: () => copyText(api, task.id) },
    ],
  });
}

async function forkTask(task) {
  const known = new Set(tasks.map(item => item.id));
  // The runtime addresses commands through the selected session; a sidebar
  // fork of a non-current task must open it rather than fail silently.
  if (task.id !== current?.session.id) await openTask(task);
  await deliver({
    type: 'fork_session',
    session_id: task.id,
  }, commandEnvelope(task.id, { type: 'fork_session', session_id: task.id }, null));
  await refreshTasks();
  const created = tasks.find(item => !known.has(item.id));
  if (!created) {
    toast('分叉已提交，但任务列表尚未返回副本；请刷新后查看', { tone: 'warning', holdMs: 4200 });
    return;
  }
  toast('已创建分叉会话', { tone: 'success', holdMs: 1800 });
  await openTask(created);
}

async function archiveTask(task) {
  const ok = await confirmDialog({
    title: '归档会话？',
    body: '归档后将从默认会话列表隐藏，完整对话记录保留。当前桌面没有恢复归档的入口。',
    confirm: '归档',
    danger: true,
  });
  if (!ok) return;
  await action(async () => {
    if (task.id !== current?.session.id) await openTask(task);
    await deliver({ type: 'archive_session', session_id: task.id });
    await refreshTasks();
    if ((settings.archivedFilter ?? 'default') === 'default' && tasks.some(item => item.id === task.id)) {
      throw new Error('Runtime 尚未确认会话归档，请刷新列表');
    }
    if (current?.session.id === task.id) {
      current = null;
      history = [];
      $('conversation').replaceChildren();
      render();
    }
    toast('会话已归档', { tone: 'info', holdMs: 2200 });
  });
}

function archiveCurrent() {
  if (!current) return;
  const task = tasks.find(item => item.id === current.session.id)
    ?? { id: current.session.id, title: current.session.goal, source_id: currentSource };
  archiveTask(task);
}

function openAccountMenu(anchor) {
  openMenu({
    anchor, width: 220, side: 'top',
    items: [
      { label: '设置', icon: 'settings', shortcut: '⌘ ,', onSelect: () => openSettings('general') },
      { label: '意见反馈', icon: 'feedback', onSelect: openFeedbackDialog },
      { label: '登录', icon: 'user', disabled: true, badge: '未接入', title: '当前版本未接入登录' },
    ],
  });
}

function openViewOptions(anchor) {
  const archived = settings.archivedFilter ?? 'default';
  const group = settings.groupMode ?? 'workspace';
  const sort = settings.sortMode ?? 'updated';
  openMenu({
    anchor, width: 236, align: 'end',
    items: [
      { heading: '分组' },
      { label: '按工作区', checked: group === 'workspace', onSelect: () => { settings.groupMode = 'workspace'; saveSettings(); renderSidebar(); } },
      { label: '按工作区树', checked: group === 'tree', onSelect: () => { settings.groupMode = 'tree'; saveSettings(); renderSidebar(); } },
      { label: '平铺', checked: group === 'flat', onSelect: () => { settings.groupMode = 'flat'; saveSettings(); renderSidebar(); } },
      { heading: '排序' },
      { label: '最近更新', checked: sort === 'updated', onSelect: () => { settings.sortMode = 'updated'; saveSettings(); renderSidebar(); } },
      { label: '创建时间', checked: sort === 'created', onSelect: () => { settings.sortMode = 'created'; saveSettings(); renderSidebar(); } },
      { label: '名称', checked: sort === 'name', onSelect: () => { settings.sortMode = 'name'; saveSettings(); renderSidebar(); } },
      { label: '手动排序', checked: sort === 'manual', onSelect: () => {
        settings.sortMode = 'manual';
        if (!manualSessionIds().length) settings.manualOrder = { ...(settings.manualOrder ?? {}), sessions: [...tasks].map(task => task.id) };
        const paths = workspaceTaskGroups(tasks).spaces.map(space => space.path);
        if (!manualGroupPaths().length) settings.manualOrder = { ...(settings.manualOrder ?? {}), groups: paths };
        saveSettings(); renderSidebar();
      } },
      { heading: '归档' },
      { label: '默认（不含归档）', checked: archived === 'default', onSelect: () => setArchivedFilter('default') },
      { label: '包含归档', checked: archived === 'all', onSelect: () => setArchivedFilter('all') },
      { label: '仅归档', checked: archived === 'only', onSelect: () => setArchivedFilter('only') },
    ],
  });
}

function setArchivedFilter(value) {
  settings.archivedFilter = value;
  saveSettings();
  action(refreshTasks);
}

function openSidebarSearch() {
  if (document.querySelector('.sidebar-search-input')) return;
  const head = document.querySelector('.workspace-section-head');
  const label = head.querySelector('.section-label');
  const input = el('input', 'sidebar-search-input');
  input.type = 'search';
  input.placeholder = '搜索会话名称';
  input.setAttribute('aria-label', '搜索会话名称');
  label.replaceWith(input);
  input.focus();
  input.addEventListener('input', () => {
    sidebarQuery = input.value;
    renderSidebar();
    clearTimeout(contentSearchTimer);
    contentSearchTimer = setTimeout(() => runContentSearch(sidebarQuery), 250);
  });
  const close = () => {
    sidebarQuery = '';
    contentSearchSeq += 1;
    contentSearch = { query: '', status: 'idle', results: [], scanned: 0, total: 0 };
    input.replaceWith(label);
    renderSidebar();
  };
  input.addEventListener('keydown', event => { if (event.key === 'Escape') { event.preventDefault(); close(); } });
  input.addEventListener('blur', () => { if (!input.value) close(); });
}

/* ---------------------------------------------------------------- header */

function headerCountButton() {
  const button = el('button', 'header-count-action');
  button.type = 'button';
  button.setAttribute('aria-haspopup', 'dialog');
  button.setAttribute('aria-expanded', 'false');
  button.append(
    el('span', 'header-state-dot'),
    el('span', 'header-count-label'),
    iconNode('chevronDown', { size: 12, className: 'header-count-chevron' }),
  );
  return button;
}

function openSubagentPopover(anchor) {
  openPopover({
    anchor, width: 336, align: 'end', className: 'subagent-popover',
    render: node => {
      node.append(el('div', 'header-popover-title', '子智能体'));
      const children = current?.children ?? [];
      if (!children.length) node.append(el('div', 'header-popover-empty', '暂无子智能体'));
      for (const child of children) {
        const row = el('button', 'header-agent-row');
        row.type = 'button';
        const dot = el('span', 'header-state-dot');
        dot.dataset.state = childIsSettled(child) ? (child.ok === false ? 'failed' : 'done') : 'running';
        const copy = el('span', 'header-agent-copy');
        copy.append(el('span', 'header-agent-name', child.nickname || child.title || child.id));
        const secondary = [child.title && child.title !== child.nickname ? child.title : child.purpose, childIsSettled(child) ? (child.ok === false ? '失败' : '已完成') : (child.phase || '正在运行')].filter(Boolean).join(' · ');
        if (secondary) copy.append(el('span', 'header-agent-meta', secondary));
        const metrics = el('span', 'header-agent-metrics');
        if (child.input_tokens || child.output_tokens) metrics.append(el('span', undefined, `${(child.input_tokens ?? 0) + (child.output_tokens ?? 0)} tok`));
        row.append(dot, copy, metrics);
        row.addEventListener('click', () => { closePopover(); openRightbarTab('subagent'); });
        node.append(row);
      }
      node.append(el('div', 'header-popover-note', '打开子会话尚未接入当前 Runtime；此处展示真实运行状态。'));
    },
  });
}

function formatJobDuration(ms) {
  const seconds = Math.max(0, Math.floor((ms ?? 0) / 1000));
  if (seconds < 60) return `${seconds}秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}分${seconds % 60}秒`;
  return `${Math.floor(minutes / 60)}小时${minutes % 60}分`;
}

let jobsPopoverState = null;
const expandedJobs = new Set();
const clearedJobs = new Set();
/** One armed stop at a time: a second press within the window confirms. */
let jobsArmed = null;
let jobsArmTimer = null;
let jobsClockTimer = null;
let settledOpen = true;
const pendingJobStops = new Set();

function jobCommand(job) {
  return [job.program, ...(Array.isArray(job.args) ? job.args : [])].filter(Boolean).join(' ') || job.task_id;
}

function jobDuration(job, live) {
  // The live clock derives from the observed start; snapshot-provided rows carry
  // only Runtime-observed age, and event-provided rows carry started_at.
  if (live) return formatJobDuration(job.started_at ? Date.now() - job.started_at : (job.elapsed_ms ?? 0));
  return formatJobDuration(job.duration_ms ?? ((job.finished_at ?? 0) - (job.started_at ?? 0)));
}

function jobSettledState(job) {
  // A stopped task is not a failure however its exit code reads (the exit
  // event's own rule); check the stop fact before the ok flag.
  if (job.stopped) return { label: '已停止', dot: 'warning' };
  if (job.status === 'unknown') return { label: '结果未知', dot: 'failed' };
  if (job.status === 'failed') return { label: '已失败', dot: 'failed' };
  return { label: '已完成', dot: 'done' };
}

function disarmJob() {
  if (jobsArmTimer) { clearTimeout(jobsArmTimer); jobsArmTimer = null; }
  jobsArmed = null;
}

function stopJob(job) {
  if (pendingJobStops.has(job.task_id)) return;
  if (jobsArmed !== job.task_id) {
    disarmJob();
    jobsArmed = job.task_id;
    jobsArmTimer = setTimeout(() => {
      jobsArmTimer = null;
      jobsArmed = null;
      if (jobsPopoverState?.node.isConnected) renderJobsInto(jobsPopoverState.node);
    }, 3000);
    if (jobsPopoverState?.node.isConnected) renderJobsInto(jobsPopoverState.node);
    return;
  }
  disarmJob();
  pendingJobStops.add(job.task_id);
  if (jobsPopoverState?.node.isConnected) renderJobsInto(jobsPopoverState.node);
  deliver({ type: 'cancel_background_task', session_id: current.session.id, task_id: job.task_id }).then(result => {
    if (result?.ok === false) throw new Error(result.error?.message ?? '停止请求未被 Runtime 接受');
    toast('已请求停止后台任务', { tone: 'info', holdMs: 2200 });
  }).catch(error => {
    pendingJobStops.delete(job.task_id);
    toast(error?.message ?? '停止后台任务失败', { tone: 'error', holdMs: 4000 });
  }).finally(() => {
    if (jobsPopoverState?.node.isConnected) renderJobsInto(jobsPopoverState.node);
  });
}

function jobChevron(expanded) {
  const box = el('span', 'job-chevron-box');
  box.append(iconNode('chevronDown', { size: 12, className: expanded ? 'job-chevron open' : 'job-chevron' }));
  return box;
}

function jobPanel(job) {
  const panel = el('div', 'job-panel');
  panel.dataset.task = job.task_id;
  const output = el('pre', 'job-output', job.output || '尚无输出');
  if (!job.output) output.dataset.empty = 'true';
  panel.append(output);
  return panel;
}

/** A live chunk updates the open panel in place, keeping its scroll position. */
function updateJobsOutput(taskId) {
  const node = jobsPopoverState?.node;
  if (!node?.isConnected) return;
  const panel = [...node.querySelectorAll('.job-panel')].find(candidate => candidate.dataset.task === taskId);
  if (!panel) return;
  const job = (current?.backgroundTasks ?? []).find(candidate => candidate.task_id === taskId);
  if (!job) return;
  const output = panel.querySelector('.job-output');
  if (!output) return;
  const stick = output.scrollHeight - output.scrollTop - output.clientHeight < 40;
  output.textContent = job.output || '尚无输出';
  output.dataset.empty = job.output ? 'false' : 'true';
  if (stick) output.scrollTop = output.scrollHeight;
}

function renderJobsInto(node) {
  // Rebuilding on a live output chunk must not steal keyboard focus from the
  // control the reader is using.
  const active = document.activeElement;
  const focusKey = node.contains(active) ? active?.dataset?.jobFocus : null;
  node.replaceChildren();
  const jobs = current?.backgroundTasks ?? [];
  const settled = (current?.settledBackgroundTasks ?? []).filter(job => !clearedJobs.has(job.task_id));
  node.append(el('div', 'header-popover-title', '后台任务'));
  if (!jobs.length && !settled.length) node.append(el('div', 'header-popover-empty', '暂无后台任务'));
  if (jobs.length) node.append(el('div', 'job-section-label', '进行中'));
  for (const job of jobs) {
    const expanded = expandedJobs.has(job.task_id);
    const card = el('div', 'job-live-card');
    const row = el('button', 'job-row');
    row.type = 'button';
    row.dataset.jobFocus = `row:${job.task_id}`;
    row.setAttribute('aria-expanded', String(expanded));
    row.setAttribute('aria-label', `${expanded ? '收起' : '展开'}任务输出：${jobCommand(job)}`);
    const dot = el('span', 'header-state-dot');
    dot.dataset.state = 'running';
    const main = el('span', 'job-main');
    const primary = el('span', 'job-primary');
    primary.append(el('span', 'job-label', jobCommand(job)));
    const secondary = el('span', 'job-secondary');
    secondary.append(el('span', 'job-kind', '命令'));
    secondary.append(el('span', 'job-status', pendingJobStops.has(job.task_id) ? '正在停止' : '运行中'));
    const duration = el('span', 'job-duration', jobDuration(job, true));
    duration.dataset.live = job.task_id;
    secondary.append(duration);
    main.append(primary, secondary);
    row.append(dot, main, jobChevron(expanded));
    row.addEventListener('click', () => {
      if (expanded) expandedJobs.delete(job.task_id); else expandedJobs.add(job.task_id);
      renderJobsInto(node);
    });
    const stop = el('button', 'job-stop');
    stop.type = 'button';
    stop.dataset.jobFocus = `stop:${job.task_id}`;
    stop.disabled = pendingJobStops.has(job.task_id);
    const armed = jobsArmed === job.task_id;
    stop.classList.toggle('job-stop-armed', armed);
    stop.setAttribute('aria-label', armed ? `确认停止 ${jobCommand(job)}` : `停止 ${jobCommand(job)}`);
    stop.title = armed ? '再次按下确认停止' : '停止后台任务';
    stop.append(el('span', 'job-stop-glyph'));
    if (armed) stop.append(el('span', 'job-stop-label', '确认停止'));
    stop.addEventListener('click', () => stopJob(job));
    card.append(row, stop);
    node.append(card);
    if (expanded) node.append(jobPanel(job));
  }
  if (settled.length) {
    const header = el('div', 'job-section-label job-settled-head');
    const toggle = el('button', 'job-section-toggle');
    toggle.type = 'button';
    toggle.dataset.jobFocus = 'settled-toggle';
    toggle.setAttribute('aria-expanded', String(settledOpen));
    toggle.append(iconNode('chevronDown', { size: 12, className: settledOpen ? 'job-chevron open' : 'job-chevron' }));
    toggle.append(el('span', undefined, `已结束 ${settled.length}`));
    toggle.addEventListener('click', () => { settledOpen = !settledOpen; renderJobsInto(node); });
    const clear = el('button', 'job-section-clear', '清除');
    clear.type = 'button';
    clear.dataset.jobFocus = 'settled-clear';
    clear.addEventListener('click', () => {
      for (const job of settled) clearedJobs.add(job.task_id);
      renderJobsInto(node);
    });
    header.append(toggle, clear);
    node.append(header);
    if (settledOpen) {
      for (const job of [...settled].reverse()) {
        const state = jobSettledState(job);
        const expanded = expandedJobs.has(job.task_id);
        // A settled row with retained output is a disclosure; one without is
        // read-only, like the reference settled tail.
        const observable = Boolean(job.output);
        const card = el('div', 'job-settled-card');
        const row = el(observable ? 'button' : 'span', observable ? 'job-row job-row-settled' : 'job-row job-row-settled job-row-static');
        if (observable) {
          row.type = 'button';
          row.dataset.jobFocus = `row:${job.task_id}`;
          row.setAttribute('aria-expanded', String(expanded));
          row.setAttribute('aria-label', `${expanded ? '收起' : '展开'}任务输出：${jobCommand(job)}`);
        }
        const dot = el('span', 'header-state-dot');
        dot.dataset.state = state.dot;
        row.append(dot, el('span', 'job-kind', '命令'), el('span', 'job-label job-label-settled', jobCommand(job)),
          el('span', 'job-status', state.label), el('span', 'job-duration', jobDuration(job, false)));
        if (observable) {
          row.append(jobChevron(expanded));
          row.addEventListener('click', () => {
            if (expanded) expandedJobs.delete(job.task_id); else expandedJobs.add(job.task_id);
            renderJobsInto(node);
          });
        }
        card.append(row);
        node.append(card);
        if (expanded) node.append(jobPanel(job));
      }
    }
  }
  if (focusKey) {
    const target = [...node.querySelectorAll('[data-job-focus]')].find(candidate => candidate.dataset.jobFocus === focusKey);
    target?.focus({ preventScroll: true });
  }
  startJobsClock();
}

function startJobsClock() {
  const node = jobsPopoverState?.node;
  if (!node || !node.isConnected) { if (jobsClockTimer) { clearInterval(jobsClockTimer); jobsClockTimer = null; } return; }
  const live = current?.backgroundTasks ?? [];
  if (!live.length) { if (jobsClockTimer) { clearInterval(jobsClockTimer); jobsClockTimer = null; } return; }
  if (jobsClockTimer) return;
  jobsClockTimer = setInterval(() => {
    const host = jobsPopoverState?.node;
    if (!host?.isConnected) { clearInterval(jobsClockTimer); jobsClockTimer = null; return; }
    for (const job of current?.backgroundTasks ?? []) {
      const cell = host.querySelector(`[data-live="${CSS.escape(job.task_id)}"]`);
      if (cell) cell.textContent = jobDuration(job, true);
    }
  }, 1000);
}

function openJobsPopover(anchor) {
  disarmJob();
  openPopover({
    anchor, width: 500, align: 'end', className: 'jobs-popover',
    render: node => {
      jobsPopoverState = { node, anchor };
      node.addEventListener('keydown', event => trapTab(node, event));
      renderJobsInto(node);
    },
  });
}

function openSchedulePopover(anchor) {
  openPopover({
    anchor, width: 336, align: 'end', className: 'schedule-popover',
    render: node => {
      node.append(el('div', 'header-popover-title', '提醒'));
      node.append(el('div', 'header-popover-empty', '提醒目录尚未接入当前桌面桥接。'));
      node.append(el('div', 'header-popover-note', 'Runtime 暴露了 schedule 工具与事件，但桌面没有列表、创建或取消接口。'));
      const open = el('button', 'header-popover-action', '打开任务详情');
      open.type = 'button';
      open.addEventListener('click', () => { closePopover(); openRightbarTab('schedule'); });
      node.append(open);
    },
  });
}

function ensureHeaderActions() {
  if (headerMenuButton) return;
  headerMenuButton = $('header-more');
  headerMenuButton.append(iconNode('more', { size: 16 }));
  headerMenuButton.addEventListener('click', () => {
    if (!current) return;
    openMenu({
      anchor: headerMenuButton, align: 'end', width: 240,
      items: [
        { label: '下载 Session 日志', icon: 'download', disabled: true, badge: '未接入', title: 'Runtime 未提供 Session 打包导出接口' },
        { label: '反馈', icon: 'feedback', onSelect: openFeedbackDialog },
      ],
    });
  });
  const preset = $('header-preset');
  if (preset && !preset.querySelector('svg')) preset.replaceChildren(iconNode('shield', { size: 14 }), el('span', undefined, '标准模式'));
  const sessionActions = $('header-session-actions');
  headerSubagentButton = headerCountButton();
  headerJobsButton = headerCountButton();
  sessionActions.append(headerSubagentButton, headerJobsButton);
  headerSubagentButton.addEventListener('click', () => openSubagentPopover(headerSubagentButton));
  headerJobsButton.addEventListener('click', () => openJobsPopover(headerJobsButton));
  const schedule = $('header-schedule');
  schedule.append(iconNode('clock', { size: 16 }));
  schedule.addEventListener('click', () => openSchedulePopover(schedule));
  installViewTabs();
}

/** One delegated handler owns both tabs. It survives any header re-render and
 * gives arrow/Home/End navigation the same activation path as a click. */
function installViewTabs() {
  const host = $('view-tabs');
  if (!host || host.dataset.bound === 'true') return;
  host.dataset.bound = 'true';
  const activate = view => {
    const next = view === 'trajectory' ? 'trajectory' : 'chat';
    if (!current || isHome()) return;
    const scroller = $('conversation-scroll');
    const previousScroll = scroller.scrollTop;
    if (next === activeView) {
      host.querySelector(`[data-view="${next}"]`)?.focus({ preventScroll: true });
      return;
    }
    activeView = next;
    renderHeader();
    renderConversation();
    renderTrajectory();
    if (next === 'chat') scroller.scrollTop = previousScroll;
    controls();
    host.querySelector(`[data-view="${next}"]`)?.focus({ preventScroll: true });
  };
  host.addEventListener('click', event => {
    const tab = event.target.closest('[data-view]');
    if (!tab) return;
    event.preventDefault();
    activate(tab.dataset.view);
  });
  host.addEventListener('keydown', event => {
    const tabs = [...host.querySelectorAll('[data-view]')];
    const at = tabs.indexOf(document.activeElement);
    if (at < 0) return;
    let next = null;
    if (event.key === 'ArrowRight') next = (at + 1) % tabs.length;
    else if (event.key === 'ArrowLeft') next = (at - 1 + tabs.length) % tabs.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = tabs.length - 1;
    else if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); activate(tabs[at].dataset.view); return; }
    else return;
    event.preventDefault();
    const tab = tabs[next];
    tab.focus();
    activate(tab.dataset.view);
  });
}

function feedbackCategories() {
  return ['任务结果', '指令理解与遵循', '产品功能与交互', '稳定性和速度', '资源使用与费用', '安全隐私与权限', '其他'];
}

function openFeedbackDialog(options = {}) {
  const presetCategory = typeof options?.category === 'string' ? options.category : null;
  const presetDetail = typeof options?.detail === 'string' ? options.detail : '';
  if (document.querySelector('.feedback-modal')) return;
  const overlay = el('div', 'feedback-overlay');
  overlay.setAttribute('role', 'presentation');
  const dialog = el('div', 'feedback-modal');
  dialog.setAttribute('role', 'dialog');
  dialog.setAttribute('aria-modal', 'true');
  dialog.setAttribute('aria-label', '提交反馈');
  const head = el('div', 'feedback-head');
  head.append(el('h2', undefined, '提交反馈'));
  const close = iconButton('close', '关闭反馈', { size: 16, tooltip: '关闭' });
  head.append(close);
  const chips = el('div', 'feedback-chips');
  const selected = new Set(presetCategory ? [presetCategory] : []);
  for (const category of feedbackCategories()) {
    const chip = el('button', 'feedback-chip', category);
    chip.type = 'button';
    chip.setAttribute('aria-pressed', String(selected.has(category)));
    chip.addEventListener('click', () => {
      if (selected.has(category)) selected.delete(category); else selected.add(category);
      chip.setAttribute('aria-pressed', String(selected.has(category)));
    });
    chips.append(chip);
  }
  const detail = el('textarea', 'feedback-detail');
  detail.rows = 5;
  detail.value = presetDetail;
  detail.placeholder = '填写详情以帮助我们改进体验，提交内容会包括当前对话的日志';
  detail.setAttribute('aria-label', '反馈详情');
  const submit = el('button', 'button primary feedback-submit', '提交');
  submit.type = 'button';
  submit.addEventListener('click', () => {
    // No feedback channel is connected to this Runtime. Never report a
    // successful submission for a message nothing received.
    toast('反馈通道尚未接入当前 Runtime', { tone: 'info', holdMs: 3200 });
  });
  dialog.append(head, chips, detail, submit);
  overlay.append(dialog);
  overlay.addEventListener('mousedown', event => { if (event.target === overlay) overlay.remove(); });
  close.addEventListener('click', () => overlay.remove());
  const onKey = event => {
    if (event.key === 'Escape') { overlay.remove(); document.removeEventListener('keydown', onKey, true); }
  };
  document.addEventListener('keydown', onKey, true);
  document.body.append(overlay);
  detail.focus();
}

function openJobsMenu(anchor) {
  const jobs = current?.backgroundTasks ?? [];
  openMenu({
    anchor, width: 320, align: 'end',
    items: jobs.length
      ? jobs.map(job => ({
        label: `${job.program ?? '进程'} ${(job.args ?? []).join(' ')}`.slice(0, 60),
        icon: 'terminal',
        badge: job.pid ? `PID ${job.pid}` : undefined,
        disabled: true,
        title: '后台任务取消接口尚未接入桌面',
      }))
      : [{ label: '暂无后台任务', disabled: true }],
  });
}

function beginTitleEdit() {
  if (!current || loading || uncertain || titleEditing) return;
  titleEditing = true;
  const title = $('conversation-title');
  const input = el('input', 'title-input');
  input.value = current.session.goal ?? '';
  input.maxLength = 256;
  input.setAttribute('aria-label', '会话标题');
  title.replaceWith(input);
  input.focus();
  input.select();
  let finished = false;
  const finish = async commit => {
    if (finished) return;
    if (commit && !titleComposing) {
      try {
        const command = renameCommand(current?.session.id, input.value);
        await action(async () => {
          await deliver(command);
          await refreshMetadata();
          if (current.session.goal !== command.name) throw new Error('Runtime 尚未返回更新后的标题');
          await refreshTasks();
          toast('标题已更新', { tone: 'success', holdMs: 1600 });
        });
      } catch (error) { toast(error.message, { tone: 'error' }); }
    }
    finished = true;
    titleEditing = false;
    input.replaceWith(title);
    renderHeader();
  };
  input.addEventListener('compositionstart', () => { titleComposing = true; });
  input.addEventListener('compositionend', () => { titleComposing = false; });
  input.addEventListener('keydown', event => {
    if (event.isComposing) return;
    if (event.key === 'Enter') { event.preventDefault(); finish(true); }
    else if (event.key === 'Escape') { event.preventDefault(); finish(false); }
  });
  input.addEventListener('blur', () => { if (titleEditing) finish(false); });
}

function renderHeaderCrumbs(text) {
  const nav = $('header-crumbs');
  if (!nav) return;
  for (const node of [...nav.querySelectorAll('.header-crumb:not(#conversation-title)')]) node.remove();
  const title = $('conversation-title');
  title.textContent = text;
  title.className = 'header-crumb header-crumb-current';
  title.setAttribute('aria-current', 'page');
}

function renderHeader() {
  const home = isHome();
  const pageOpen = !$('entry-page').hidden;
  document.body.classList.toggle('home', home && !pageOpen);
  document.body.classList.toggle('blank-session', home);
  $('conversation-header').hidden = home || pageOpen;
  $('floating-right-toggle').hidden = !home || pageOpen || rightbarOpen;
  const tabs = $('view-tabs');
  if (tabs) {
    tabs.hidden = home || pageOpen || !current;
    for (const tab of tabs.querySelectorAll('[data-view]')) {
      const selected = tab.dataset.view === activeView;
      tab.setAttribute('aria-selected', String(selected));
      tab.tabIndex = selected ? 0 : -1;
    }
  }
  if (!current) {
    if (!titleEditing) renderHeaderCrumbs('新建会话');
    $('conversation-status').textContent = '';
    renderOpenInAppControls();
    return;
  }
  if (!titleEditing) renderHeaderCrumbs(current.session.goal || '新建会话');
  // The reference header carries no status sentence; runtime state is shown in
  // the transcript and the composer. Keep the element for assistive tech only.
  $('conversation-status').textContent = current.approvals.length || current.clarifications.length
    ? '等待你处理' : current.activity || LABELS[current.status] || current.status;
  $('conversation-status').dataset.tone = current.status === 'failed' ? 'error'
    : current.status === 'running' ? 'active' : current.approvals.length || current.clarifications.length ? 'waiting' : '';
  $('conversation-status').hidden = true;
  const children = current.children ?? [];
  const jobs = current.backgroundTasks ?? [];
  const settledJobs = current.settledBackgroundTasks ?? [];
  if (headerSubagentButton) {
    headerSubagentButton.hidden = !children.length;
    const running = children.some(child => !childIsSettled(child));
    headerSubagentButton.querySelector('.header-count-label').textContent = `${children.length} 个子智能体${running ? '，正在运行' : ''}`;
    headerSubagentButton.querySelector('.header-state-dot').dataset.state = running ? 'running' : 'done';
    headerSubagentButton.setAttribute('aria-label', `${children.length} 个子智能体${running ? '，正在运行' : ''}`);
  }
  if (headerJobsButton) {
    const totalJobs = jobs.length + settledJobs.length;
    headerJobsButton.hidden = !totalJobs;
    headerJobsButton.querySelector('.header-count-label').textContent = jobs.length ? `${jobs.length} 个后台任务运行中` : `${settledJobs.length} 个后台任务`;
    headerJobsButton.querySelector('.header-state-dot').dataset.state = jobs.length ? 'running' : 'done';
    headerJobsButton.setAttribute('aria-label', jobs.length ? `${jobs.length} 个后台任务运行中` : `${settledJobs.length} 个后台任务`);
  }
  const schedule = $('header-schedule');
  if (schedule) {
    schedule.hidden = !current;
    schedule.dataset.tooltip = '提醒（目录未接入）';
    schedule.setAttribute('aria-label', '提醒（目录未接入）');
  }
  renderOpenInAppControls();
}

/* ---------------------------------------------------------- open in app */

function ensureOpenInApps() {
  if (openInAppState.loading || openInAppState.apps) return;
  openInAppState.loading = true;
  openInAppState.preferred ??= savedOpenApp();
  api.openInAppApps().then(apps => {
    openInAppState = { ...openInAppState, apps: Array.isArray(apps) ? apps : [], loading: false, error: null };
  }, error => {
    openInAppState = { ...openInAppState, apps: [], loading: false, error: error?.message ?? '无法获取应用列表' };
  }).finally(() => {
    renderOpenInAppControls();
    renderConversation();
    if (rightbarOpen) renderRightbar();
  });
}

function appLeading(app) {
  if (app?.icon) {
    const image = el('img', 'open-app-icon');
    image.src = app.icon;
    image.alt = '';
    image.width = 16;
    image.height = 16;
    return image;
  }
  return iconNode('external', { size: 14 });
}

async function openInApp(path, appId, { action = 'open', remember = false } = {}) {
  if (!path || (!appId && action !== 'default') || openInAppState.busy) return;
  openInAppState.busy = true;
  renderOpenInAppControls();
  renderConversation();
  if (rightbarOpen) renderRightbar();
  try {
    await api.openInApp({ path, appId, action });
    if (action === 'open' && remember) {
      openInAppState.preferred = appId;
      rememberOpenApp(appId);
    }
    renderOpenInAppControls();
    if (rightbarOpen) renderRightbar();
  } catch (error) {
    toast(action === 'reveal' ? '无法显示文件位置，请重试' : '打开失败，请重试', { tone: 'warning', holdMs: 3200 });
  } finally {
    openInAppState.busy = false;
    renderOpenInAppControls();
    renderConversation();
    if (rightbarOpen) renderRightbar();
  }
}

function openInAppMenu(anchor, path, kind) {
  ensureOpenInApps();
  const apps = openInAppState.apps ?? [];
  const preferred = apps.find(app => app.id === openInAppState.preferred)?.id ?? apps[0]?.id ?? null;
  const items = [];
  if (openInAppState.loading) items.push({ label: '正在读取应用…', disabled: true });
  else if (openInAppState.error) items.push({ label: '无法获取应用列表', disabled: true, title: openInAppState.error });
  else {
    for (const app of apps) {
      items.push({
        label: app.id === preferred ? `${app.name}（默认）` : app.name,
        image: app.icon ?? undefined,
        checked: app.id === preferred,
        onSelect: () => openInApp(path, app.id, { remember: kind === 'directory' }),
      });
    }
    if (kind === 'file') {
      items.push({ divider: true });
      items.push({ label: '使用系统默认应用', icon: 'external', onSelect: () => openInApp(path, null, { action: 'default' }) });
      items.push({ label: '显示文件位置', icon: 'folder', onSelect: () => openInApp(path, apps[0]?.id ?? 'finder', { action: 'reveal' }) });
    }
  }
  openMenu({ anchor, items, align: 'end', width: 220 });
}

function absoluteOpenPath(value) {
  if (!value) return null;
  if (value.startsWith('/') || /^[A-Za-z]:[\\/]/.test(value)) return value;
  const root = current?.session.repository;
  if (!root) return null;
  const separator = root.includes('\\') ? '\\' : '/';
  return `${root.replace(/[\\/]+$/, '')}${separator}${value.replace(/^[\\/]+/, '')}`;
}

function renderOpenInAppInto(host, value, kind) {
  if (!host) return;
  const path = absoluteOpenPath(value);
  if (!path) { host.hidden = true; host.replaceChildren(); return; }
  ensureOpenInApps();
  host.hidden = false;
  host.replaceChildren();
  const apps = openInAppState.apps;
  if (!openInAppState.loading && apps && apps.length === 0) { host.hidden = true; return; }
  const preferred = apps?.find(app => app.id === openInAppState.preferred) ?? apps?.[0] ?? null;
  const primary = el('button', 'open-app-primary');
  primary.type = 'button';
  primary.disabled = openInAppState.loading || !!openInAppState.busy || !preferred;
  primary.setAttribute('aria-label', preferred ? `用 ${preferred.name} 打开` : '正在读取应用');
  primary.dataset.tooltip = preferred ? `用 ${preferred.name} 打开` : '正在读取应用';
  if (openInAppState.busy) primary.append(el('span', 'open-app-spinner'));
  else if (openInAppState.loading) primary.classList.add('loading');
  else primary.append(preferred ? appLeading(preferred) : iconNode('external', { size: 14 }));
  primary.addEventListener('click', () => {
    if (preferred) openInApp(path, preferred.id, { remember: kind === 'directory' });
  });
  host.append(primary);
  if ((apps?.length ?? 0) > 1 || kind === 'file') {
    const more = el('button', 'open-app-more');
    more.type = 'button';
    more.setAttribute('aria-label', '更多打开方式');
    more.setAttribute('aria-haspopup', 'menu');
    more.dataset.tooltip = '更多打开方式';
    more.append(iconNode('chevronDown', { size: 10 }));
    more.disabled = openInAppState.loading || !!openInAppState.busy;
    more.addEventListener('click', () => openInAppMenu(more, path, kind));
    host.append(more);
  }
}

function renderOpenInAppControls() {
  const path = current?.session.repository || null;
  renderOpenInAppInto($('open-in-app'), path, 'directory');
}

/* --------------------------------------------------------------- composer */

function renderComposerDocks() {
  const host = $('composer-docks');
  if (!host) return;
  host.replaceChildren();
  // A plan is real progress even before its first work message commits; the
  // composer dock must not disappear just because the session still looks home.
  const hasPlan = (current?.plan?.steps ?? []).length > 0;
  if (!current || (isHome() && !hasPlan)) { host.hidden = true; queuedMessages = queuedMessages.filter(item => item.sessionId === current?.session.id); return; }
  queuedMessages = queuedMessages.filter(item => item.sessionId === current.session.id);
  host.hidden = false;
  if (queuedMessages.length) {
    const dock = el('div', 'composer-dock queue-dock');
    const head = el('div', 'composer-dock-head');
    head.append(el('span', undefined, `待发送 ${queuedMessages.length}`));
    dock.append(head);
    for (const item of queuedMessages) {
      const row = el('div', 'composer-dock-row');
      row.append(el('span', 'composer-dock-text', item.content || `附件 × ${item.attachments.length}`));
      const remove = el('button', 'composer-dock-action', '移除');
      remove.type = 'button';
      remove.setAttribute('aria-label', '移除排队消息');
      remove.addEventListener('click', () => {
        queuedMessages = queuedMessages.filter(message => message.id !== item.id);
        renderComposerDocks();
      });
      row.append(remove);
      dock.append(row);
    }
    host.append(dock);
  }
  // Reference task panel: the list stays readable in every plan state, and the
  // header names per-status counts instead of a bare ratio.
  const steps = current.plan?.steps ?? [];
  if (steps.length) {
    if (todoDockSessionId !== current.session.id) { todoDockSessionId = current.session.id; todoDockOpen = false; }
    const dock = el('details', 'composer-dock todo-dock');
    const summary = el('summary', 'composer-dock-head todo-head');
    const chevron = el('span', 'todo-chevron');
    chevron.append(iconNode(todoDockOpen ? 'chevronDown' : 'chevronUp', { size: 14 }));
    summary.append(iconNode('list', { size: 16 }), el('span', 'todo-title', '任务'),
      el('span', 'todo-progress', planProgressLabel(steps)), chevron);
    dock.append(summary);
    for (const step of steps) {
      const row = el('div', 'composer-dock-row todo-row');
      const dot = el('span', 'composer-step-dot');
      dot.dataset.state = step.status === 'done' ? 'done'
        : step.status === 'running' ? 'ongoing'
          : step.status === 'failed' ? 'error' : 'idle';
      row.append(dot, el('span', 'composer-dock-text', step.description));
      dock.append(row);
    }
    dock.open = todoDockOpen;
    dock.addEventListener('toggle', () => {
      todoDockOpen = dock.open;
      chevron.replaceChildren(iconNode(todoDockOpen ? 'chevronDown' : 'chevronUp', { size: 14 }));
    });
    host.append(dock);
  }
}

async function flushQueuedMessage(attempt = 0) {
  if (!current || !queuedMessages.length) return;
  if (loading || ['running', 'waiting_user'].includes(current.status)) {
    if (attempt < 12) setTimeout(() => flushQueuedMessage(attempt + 1), 400);
    return;
  }
  const item = queuedMessages[0];
  if (item.sessionId !== current.session.id) return;
  try {
    await deliver({ type: 'submit_message', session_id: item.sessionId, content: item.content, attachments: item.attachments });
    queuedMessages.shift();
    renderComposerDocks();
  } catch (error) {
    toast(`排队消息未发送：${error.message}`, { tone: 'error', holdMs: 4000 });
  }
}

function renderComposerMeta() {
  const workspacePath = current?.session.repository || selection?.path || '';
  const workspace = $('workspace-picker');
  workspace.replaceChildren(
    iconNode('folder', { size: 16 }),
    el('span', 'trigger-label', workspacePath ? workspaceLabel(workspacePath) : '选择工作区'),
    iconNode('chevronDown', { size: 12, className: 'trigger-chevron' }),
  );
  workspace.dataset.tooltip = workspacePath || '选择或打开工作区';
  const preset = $('preset-picker');
  preset.replaceChildren(
    iconNode('bot', { size: 14 }),
    el('span', 'trigger-label', '标准模式'),
    iconNode('chevronDown', { size: 12, className: 'trigger-chevron' }),
  );
  const mode = current?.session.mode;
  const permission = $('permission-picker');
  permission.replaceChildren(
    iconNode(mode === 'full_access' ? 'shieldCheck' : 'shield', { size: 14 }),
    el('span', 'trigger-label', mode ? permissionLabel(mode) : '工作区内修改'),
    iconNode('chevronDown', { size: 12, className: 'trigger-chevron' }),
  );
  permission.dataset.tooltip = '访问模式';
  permission.setAttribute('aria-label', `访问模式，当前：${mode ? permissionLabel(mode) : '工作区内修改'}`);
  const model = current?.session.model;
  $('model-picker').replaceChildren(
    el('span', 'trigger-label', modelLabel(model) || '请选择模型'),
    iconNode('chevronDown', { size: 12, className: 'trigger-chevron' }),
  );
  $('model-picker').dataset.tooltip = model ? `模型提供商：${model.provider}` : '选择模型';
  $('send').replaceChildren(iconNode('arrowUp', { size: 16, stroke: 1.8 }));
  // The running button shows a solid stop block, not an outlined icon glyph.
  $('stop').replaceChildren(el('span', 'stop-glyph'));
  const running = ['running', 'waiting_user'].includes(current?.status);
  const action = running ? (settings.busySend === 'steer' ? '插入当前回合' : '排队发送') : '发送消息';
  $('send').dataset.tooltip = action;
  $('send').setAttribute('aria-label', action);
}

function formatTokenCount(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value.toLocaleString('en-US') : '未提供';
}

function contextMeterPanel() {
  const accounting = current?.contextUsage;
  if (!accounting) return null;
  const button = el('button', 'context-pill');
  button.type = 'button';
  const used = formatTokenCount(accounting.used_tokens);
  const capacity = formatTokenCount(accounting.input_capacity_tokens);
  button.append(iconNode('layers', { size: 14 }));
  button.append(el('span', undefined, accounting.input_capacity_tokens == null ? `上下文 ${used}` : `上下文 ${used} / ${capacity}`));
  button.dataset.tooltip = '上下文统计';
  button.addEventListener('click', () => openMenu({
    anchor: button, width: 340, side: 'top', align: 'start',
    items: [
      { heading: '上下文统计' },
      { label: `已用输入：${ formatTokenCount(accounting.used_tokens) }`, disabled: true },
      { label: `有效输入容量（压缩口径）：${ formatTokenCount(accounting.input_capacity_tokens) }`, disabled: true },
      { label: `模型窗口（仅展示口径）：${ formatTokenCount(accounting.context_window_tokens) }`, disabled: true },
      { label: `软压缩阈值：${ formatTokenCount(accounting.compact_at_tokens) }`, disabled: true },
      { label: `输出预留：${ formatTokenCount(accounting.output_reservation_tokens) }`, disabled: true },
      { label: `安全余量：${ formatTokenCount(accounting.headroom_tokens) }`, disabled: true },
      { label: `压缩状态：${ ({ none: '正常', soft: '软压缩', hard_required: '已达硬容量' })[accounting.fold_state] ?? accounting.fold_state ?? '未提供' }`, disabled: true },
      { label: `统计方式：${ ({ exact: '精确分词', estimated: '估计', mixed: '模型报告总量，分类为估计' })[accounting.token_count_kind] ?? accounting.token_count_kind ?? '未提供' }`, disabled: true },
    ],
  }));
  return button;
}

function renderContextMeter() {
  const host = $('context-stats');
  if (!host) return;
  const pills = [contextMeterPanel(), usagePill()].filter(Boolean);
  host.hidden = pills.length === 0;
  host.replaceChildren(...pills);
}

function refreshContextUsage() {
  if (!current) return;
  const sessionId = current.session.id;
  const queryId = crypto.randomUUID();
  api.deliver(commandEnvelope(sessionId, { type: 'query_context', session_id: sessionId, query_id: queryId }, current.session.last_sequence ?? 0))
    .then(result => { if (!result.ok) throw new Error(result.error.message); })
    .catch(() => { /* no accounting yet is not a visible failure */ });
}

function render() {
  renderSidebar();
  renderHeader();
  renderComposerMeta();
  renderComposerDocks();
  renderContextMeter();
  renderConversation();
  renderTrajectory();
  renderInteractions();
  renderRightbar();
  controls();
  layout();
}

/* ---------------------------------------------------------- conversation */

function userMessageNode(existing, message) {
  const key = `message:${message.id}`;
  let row = existing.get(key);
  if (!row) {
    row = el('article', 'message user');
    row.dataset.key = key;
    row.dataset.messageId = message.id;
    row.append(el('div', 'bubble'), el('div', 'message-actions'));
  }
  const bubble = row.querySelector('.bubble');
  if (renderedSources.get(bubble) !== message.text) {
    renderedSources.set(bubble, message.text);
    bubble.textContent = message.text;
  }
  decorateMessageActions(row, message);
  const images = Number(message.images ?? 0);
  let imageRow = row.querySelector('.message-images');
  if (images > 0 && !imageRow) {
    imageRow = el('div', 'message-images');
    row.querySelector('.bubble').after(imageRow);
  }
  if (imageRow && imageRow.dataset.count !== String(images)) {
    imageRow.dataset.count = String(images);
    imageRow.replaceChildren();
    imageRow.classList.toggle('single', images === 1);
    for (let index = 0; index < Math.min(images, 4); index += 1) {
      const tile = el('button', 'message-image-tile');
      tile.type = 'button';
      tile.setAttribute('aria-label', `查看大图 ${index + 1}`);
      tile.append(iconNode('image', { size: images === 1 ? 28 : 18 }));
      tile.addEventListener('click', () => openImagePreview(images));
      imageRow.append(tile);
    }
    if (images > 4) imageRow.append(el('span', 'message-image-more', `+${images - 4}`));
  }
  if (images <= 0 && imageRow) imageRow.remove();
  renderUserExtras(row, message);
  return row;
}

/** One assistant message: interim narration and the committed answer share the
 * same node so a completed turn changes its weight without re-creating a row.
 * The classification itself comes from `conversationItems`, never from prose. */
function assistantTextNode(existing, item) {
  const message = item.message;
  const key = `message:${message.id}`;
  let row = existing.get(key);
  if (!row) {
    row = el('article', 'message assistant');
    row.dataset.key = key;
    row.dataset.messageId = message.id;
    row.append(el('div', 'markdown'), el('div', 'message-actions'));
  }
  row.classList.toggle('narration', item.kind === 'narration');
  row.classList.toggle('final', item.kind === 'final');
  row.classList.toggle('streaming', !!item.streaming);
  const body = row.querySelector('.markdown');
  const filePaths = new Set(item.filePaths ?? []);
  const source = JSON.stringify([item.text, [...filePaths]]);
  if (renderedSources.get(body) !== source) {
    renderedSources.set(body, source);
    renderMarkdown(body, item.text, { onLink: openMessageLink, filePaths });
    for (const reference of body.querySelectorAll('.markdown-file-link')) reference.prepend(fileIcon(reference.title.split('#')[0]));
    addCodeActions(body);
  }
  decorateMessageActions(row, message);
  return row;
}

/** Runtime-authored rows sit on the transcript but never look like the person
 * speaking, and a compaction summary is never the model's answer. */
function runtimeNoticeNode(existing, item) {
  const key = `message:${item.message.id}`;
  let row = existing.get(key);
  if (!row) {
    row = el('section', 'message notice');
    row.dataset.key = key;
    row.dataset.messageId = item.message.id;
    row.append(el('span', 'notice-pill'), el('span', 'notice-text'));
  }
  row.classList.toggle('summary-notice', item.kind === 'summary');
  row.querySelector('.notice-pill').textContent = item.kind === 'summary' ? '上下文摘要' : '运行提示';
  row.querySelector('.notice-text').textContent = item.text;
  return row;
}

function renderUserExtras(row, message) {
  const replyQuestion = clarificationReplies.get(String(message.text ?? '')) ?? null;
  const files = Array.isArray(message.files) ? message.files : [];
  const references = Array.isArray(message.references) ? message.references : [];
  const signature = JSON.stringify([files, references, replyQuestion]);
  let extras = row.querySelector('.user-extras');
  if (!extras) {
    extras = el('div', 'user-extras');
    row.append(extras);
  }
  if (extras.dataset.signature === signature) return;
  extras.dataset.signature = signature;
  extras.replaceChildren();
  if (replyQuestion) {
    const reply = el('div', 'question-reply');
    const toggle = el('button', 'question-reply-toggle', '回答先前等待中的问题');
    toggle.type = 'button';
    toggle.setAttribute('aria-expanded', 'false');
    const detail = el('div', 'question-reply-detail', String(replyQuestion));
    detail.hidden = true;
    toggle.addEventListener('click', () => {
      detail.hidden = !detail.hidden;
      toggle.setAttribute('aria-expanded', String(!detail.hidden));
    });
    reply.append(toggle, detail);
    extras.append(reply);
  }
  for (const file of files) {
    const card = el('div', 'message-file-card');
    const name = String(file?.name ?? file?.path ?? '附件');
    const extension = name.includes('.') ? name.split('.').pop().slice(0, 8).toUpperCase() : '';
    const size = Number(file?.size_bytes ?? 0);
    card.title = name;
    card.append(
      iconNode('fileText', { size: 22 }),
      el('span', 'message-file-body', `${name}`),
      el('span', 'message-file-meta', [extension, size ? `${Math.max(1, Math.round(size / 1024))} KB` : ''].filter(Boolean).join(' · ')),
    );
    extras.append(card);
  }
  if (references.length) {
    const chips = el('div', 'message-refs');
    for (const reference of references) {
      const label = String(reference?.label ?? reference?.path ?? reference?.name ?? '引用');
      chips.append(el('span', 'message-ref-chip', label));
    }
    extras.append(chips);
  }
}

function openImagePreview(count) {
  const dialog = el('dialog', 'modal image-preview-modal');
  const card = el('div', 'image-preview-card');
  card.setAttribute('aria-label', '图片预览');
  const head = el('div', 'image-preview-head');
  head.append(el('h2', undefined, '图片预览'));
  const close = iconButton('close', '关闭图片预览', { size: 16, tooltip: '关闭图片预览' });
  head.append(close);
  const body = el('div', 'image-preview-body');
  body.append(iconNode('image', { size: 48 }), el('p', undefined, `运行时只提供图片数量（${count} 张），未提供图片内容，无法预览。`));
  card.append(head, body);
  dialog.append(card);
  close.addEventListener('click', () => dialog.close());
  dialog.addEventListener('close', () => { dialog.remove(); window.dispatchEvent(new Event('ui-overlay-change')); }, { once: true });
  document.body.append(dialog);
  dialog.showModal();
  window.dispatchEvent(new Event('ui-overlay-change'));
  close.focus();
}

function decorateMessageActions(row, message) {
  let actions = row.querySelector('.message-actions');
  if (!actions) { actions = el('div', 'message-actions'); row.append(actions); }
  const time = messageTime(message);
  const signature = JSON.stringify([message.id, message.text, time]);
  if (actions.dataset.for === signature) return;
  actions.dataset.for = signature;
  const copy = el('button', 'message-copy');
  copy.type = 'button';
  copy.setAttribute('aria-label', '复制消息');
  copy.dataset.tooltip = '复制';
  copy.append(iconNode('copy', { size: 15 }));
  copy.addEventListener('click', async () => {
    const ok = await copyText(api, message.text, { quiet: true, showFailure: true });
    copy.replaceChildren(iconNode(ok ? 'check' : 'copy', { size: 15 }));
    setTimeout(() => copy.replaceChildren(iconNode('copy', { size: 15 })), 1000);
  });
  const branch = el('button', 'message-copy branch-action');
  branch.type = 'button';
  branch.setAttribute('aria-label', '从当前会话分叉');
  branch.dataset.tooltip = '从当前会话分叉（Runtime 为会话级）';
  branch.append(iconNode('fork', { size: 15 }));
  branch.addEventListener('click', () => {
    const task = tasks.find(item => item.id === current?.session.id);
    if (task) action(() => forkTask(task));
    else toast('当前会话尚未进入任务列表', { tone: 'info', holdMs: 2200 });
  });
  actions.replaceChildren(copy, branch);
  if (messageKind(message) === 'assistant') {
    const good = el('button', 'message-copy');
    good.type = 'button';
    good.setAttribute('aria-label', '好的回答');
    good.dataset.tooltip = '好的回答';
    good.append(iconNode('thumbUp', { size: 15 }));
    good.addEventListener('click', () => openFeedbackDialog({ category: '任务结果', detail: '好的回答：' }));
    const bad = el('button', 'message-copy');
    bad.type = 'button';
    bad.setAttribute('aria-label', '有问题的回答');
    bad.dataset.tooltip = '有问题的回答';
    bad.append(iconNode('thumbDown', { size: 15 }));
    bad.addEventListener('click', () => openFeedbackDialog({ category: '任务结果', detail: '有问题的回答：' }));
    actions.replaceChildren(copy, good, bad, branch);
  }
  if (time) {
    const stamp = el('time', 'message-time', new Date(time).toLocaleString('zh-CN', { year: 'numeric', month: 'long', day: 'numeric', hour: '2-digit', minute: '2-digit' }));
    stamp.dateTime = time;
    if (messageKind(message) === 'user') actions.prepend(stamp);
    else actions.append(stamp);
  }
}

function addCodeActions(body) {
  for (const pre of body.querySelectorAll('pre')) {
    if (pre.querySelector('.code-banner')) continue;
    const code = pre.querySelector('code');
    if (!code) continue;
    const banner = el('div', 'code-banner');
    banner.dataset.uiAction = 'true';
    banner.append(el('span', undefined, '代码'));
    const wrap = el('button', 'code-copy');
    wrap.type = 'button';
    wrap.dataset.uiAction = 'true';
    wrap.setAttribute('aria-label', '自动换行');
    wrap.setAttribute('aria-pressed', 'false');
    wrap.append(iconNode('wrap', { size: 14 }));
    wrap.addEventListener('click', () => {
      const on = !pre.classList.contains('wrap');
      pre.classList.toggle('wrap', on);
      wrap.setAttribute('aria-pressed', String(on));
    });
    banner.append(wrap);
    const button = el('button', 'code-copy');
    button.type = 'button';
    button.dataset.uiAction = 'true';
    button.setAttribute('aria-label', '复制代码');
    button.append(iconNode('copy', { size: 14 }));
    button.addEventListener('click', async () => {
      await copyText(api, code.textContent, { quiet: true, showFailure: true });
      button.replaceChildren(iconNode('check', { size: 14 }));
      setTimeout(() => button.replaceChildren(iconNode('copy', { size: 14 })), 1000);
    });
    banner.append(button);
    pre.prepend(banner);
  }
}

function liveThoughtNode(existing, text) {
  let node = existing.get('thought:live');
  if (!node) {
    node = el('div', 'thought-row thought-live');
    node.dataset.key = 'thought:live';
    const head = el('div', 'thought-summary');
    head.append(iconNode('sparkle', { size: 14 }), el('span', 'thought-label', '正在分析请求'), el('span', 'thought-preview'));
    node.append(head);
  }
  // The live row is one line of activity, not the analysis transcript: the
  // latest reasoning line is the signal, and the settled Thought keeps the body.
  const latest = text.split('\n').map(line => line.trim()).filter(Boolean).at(-1) ?? '';
  node.querySelector('.thought-preview').textContent = latest;
  return node;
}

function toolKind(name) {
  const value = String(name ?? '').toLowerCase();
  if (/bash|shell|command|run_command|exec|terminal/.test(value)) return 'command';
  if (/read|cat|view/.test(value)) return 'read';
  if (/write|edit|patch/.test(value)) return 'edit';
  if (/web_(search|fetch)|fetch_web|search_web/.test(value)) return 'web';
  if (/grep|search|find|glob|list/.test(value)) return 'search';
  if (/subagent|delegate/.test(value)) return 'subagent';
  if (/^job_|background_task/.test(value)) return 'job';
  if (/goal/.test(value)) return 'goal';
  if (/todo|plan/.test(value)) return 'todo';
  if (/present|deliver/.test(value)) return 'deliver';
  return 'generic';
}

function toolArgs(tool) {
  if (tool?.arguments && typeof tool.arguments === 'object') return tool.arguments;
  if (typeof tool?.arguments !== 'string' || !tool.arguments.trim()) return null;
  try { return JSON.parse(tool.arguments); } catch { return null; }
}

function toolCopyButton(text) {
  const button = el('button', 'tool-copy');
  button.type = 'button';
  button.setAttribute('aria-label', '复制');
  button.dataset.tooltip = '复制';
  button.append(iconNode('copy', { size: 14 }));
  button.addEventListener('click', async () => {
    const ok = await copyText(api, text, { quiet: true, showFailure: true });
    button.replaceChildren(iconNode(ok ? 'check' : 'copy', { size: 14 }));
    setTimeout(() => button.replaceChildren(iconNode('copy', { size: 14 })), 1000);
  });
  return button;
}

function toolIconName(tool) {
  const name = String(tool.name ?? '').toLowerCase();
  if (/bash|shell|command|run/.test(name)) return 'terminal';
  if (/read|cat|open|view/.test(name)) return 'eye';
  if (/write|edit|patch/.test(name)) return 'edit';
  if (/grep|search|find|glob|list/.test(name)) return 'search';
  if (/web/.test(name)) return 'globe';
  if (/todo/.test(name)) return 'list';
  if (/subagent|agent/.test(name)) return 'bot';
  if (/browser/.test(name)) return 'globe';
  return 'wrench';
}

function toolNode(existing, tool) {
  const key = `tool:${tool.id}`;
  let row = existing.get(key);
  if (!row) {
    row = el('details', 'tool-row');
    row.dataset.key = key;
    row.dataset.toolId = tool.id;
    const summary = el('summary', 'tool-summary');
    const lead = el('span', 'tool-status');
    summary.append(lead, el('span', 'tool-label'), el('span', 'tool-target'), el('span', 'tool-failure'), el('span', 'tool-state'), el('span', 'tool-duration'));
    row.append(summary);
    row.addEventListener('toggle', () => {
      if (row.open) openTools.add(tool.id); else openTools.delete(tool.id);
      if (row.open) renderToolDetails(row, tool.id);
    });
  }
  const status = contractToolStatus(tool.status);
  row.dataset.status = status;
  row.dataset.kind = toolKind(tool.name);
  const waiting = current?.approvals.some(item => item.call_id === tool.id);
  const lead = row.querySelector('.tool-status');
  lead.replaceChildren(iconNode(toolIconName(tool), { size: 14 }));
  row.querySelector('.tool-label').textContent = waiting ? '等待审批' : toolLabel(tool);
  const target = String(toolTarget(tool) ?? '')
    || (toolLabel(tool) === '工具调用' ? String(tool.name ?? '') : '');
  const failureText = failureReason(tool.preview ?? '') ?? '';
  row.querySelector('.tool-target').textContent = waiting ? '等待你的批准'
    : target || (status === 'failed' ? failureText : '');
  row.querySelector('.tool-failure').textContent = !waiting && status === 'failed' && target ? failureText : '';
  row.querySelector('.tool-state').textContent = waiting ? ''
    : status === 'cancelled' ? '已停止'
      : status === 'unknown' ? '结果未知' : '';
  const summary = row.querySelector('.tool-summary');
  let extras = summary.querySelector('.tool-extras');
  if (!extras) {
    extras = el('span', 'tool-extras');
    summary.querySelector('.tool-duration').before(extras);
  }
  extras.replaceChildren();
  if (row.dataset.kind === 'command' && tool.exit_code !== undefined && tool.exit_code !== null && tool.exit_code !== 0) {
    const badge = el('span', 'tool-exit');
    badge.append(el('span', 'tool-exit-dot'), el('span', undefined, `退出码 ${tool.exit_code}`));
    extras.append(badge);
  }
  if (row.dataset.kind === 'edit') {
    const patch = confirmedDiffOf(tool);
    if (patch) {
      const [added, removed] = diffCounts(patch);
      extras.append(el('span', 'tool-diffstat add', `+${added}`), el('span', 'tool-diffstat del', `−${removed}`));
    }
  }
  row.querySelector('.tool-duration').textContent = tool.durationMs
    ? `${(tool.durationMs / 1000).toFixed(tool.durationMs < 10000 ? 1 : 0)}s` : '';
  if (browserActivity(tool)) {
    if (!row.querySelector('.browser-activity-link')) {
      const link = el('button', 'browser-activity-link', '查看活动');
      link.type = 'button';
      link.addEventListener('click', event => { event.preventDefault(); event.stopPropagation(); openRightbarTab('browser'); });
      row.querySelector('.tool-summary').append(link);
    }
  }
  row.open = openTools.has(tool.id);
  if (row.open) renderToolDetails(row, tool.id);
  return row;
}

function toolFilePath(tool) {
  const args = toolArgs(tool) ?? {};
  for (const key of ['path', 'file', 'file_path']) if (typeof args[key] === 'string' && args[key]) return args[key];
  const target = String(toolTarget(tool) ?? '');
  if (/\.[A-Za-z0-9]{1,8}$/.test(target)) return target;
  const located = /^([^\n:]+\.[A-Za-z0-9]{1,8}):\d+/.exec(String(displayPreview(tool) ?? ''));
  return located ? located[1] : null;
}

function toolFileButton(tool, label = '打开文件') {
  const path = toolFilePath(tool);
  if (!path || path.startsWith('/') || /^https?:/i.test(path)) return null;
  const button = el('button', 'tool-open-file', label);
  button.type = 'button';
  button.title = `在 Preview 中打开 ${path}`;
  button.append(iconNode('file', { size: 12 }), el('span', undefined, label));
  button.addEventListener('click', event => {
    event.preventDefault();
    event.stopPropagation();
    openPreviewForPath(path);
  });
  return button;
}

function renderToolDetails(row, id) {
  const tool = current?.tools.find(item => item.id === id);
  if (!tool) return;
  const kind = toolKind(tool.name);
  const args = toolArgs(tool);
  const argsRaw = typeof tool.arguments === 'string' ? tool.arguments : JSON.stringify(tool.arguments ?? {}, null, 2);
  const output = displayPreview(tool) || '';
  const patch = confirmedDiffOf(tool);
  let detail = row.querySelector('.tool-details');
  if (!detail) {
    detail = el('div', 'tool-details');
    row.append(detail);
  }
  const signature = [kind, tool.status, tool.exit_code, patch, argsRaw, output].join('\u0000');
  if (detail.dataset.signature === signature) return;
  detail.dataset.signature = signature;
  detail.dataset.kind = kind;
  detail.replaceChildren();
  let card;
  if (String(tool.name ?? '').startsWith('workflow')) {
    card = workflowRunPanel(tool, args, output);
  } else if (tool.name === 'schedule_create') {
    card = scheduleTurnCard(tool, args, output);
  } else if (kind === 'command') {
    card = el('div', 'tool-terminal');
    card.dataset.status = row.dataset.status;
    const banner = el('div', 'tool-terminal-banner');
    const run = el('span', 'tool-run-dot');
    run.dataset.status = row.dataset.status;
    const cwd = typeof args?.cwd === 'string' ? args.cwd : current?.session.repository ?? '';
    const command = typeof args?.command === 'string' ? args.command
      : typeof args?.program === 'string' ? [args.program, ...(Array.isArray(args.args) ? args.args : [])].join(' ') : String(toolTarget(tool) ?? '');
    banner.append(run, el('span', 'tool-terminal-cwd', cwd || '工作区'), el('span', 'tool-terminal-command', command), toolCopyButton(output));
    card.append(banner, el('pre', 'tool-terminal-output', output || '（无输出）'));
  } else if (kind === 'read') {
    card = el('div', 'tool-read-card');
    const head = el('div', 'tool-read-head');
    head.append(iconNode('eye', { size: 14 }), el('span', undefined, `显示 ${output ? output.split('\n').length : 0} 行`));
    const openFile = toolFileButton(tool);
    if (openFile) head.append(openFile);
    const pre = el('pre', 'tool-read-pre');
    const content = output.split('\n');
    for (const [index, line] of content.entries()) {
      const node = el('span', 'tool-read-line');
      node.append(el('span', 'tool-read-number', String(index + 1)), el('span', 'tool-read-text', line));
      pre.append(node);
    }
    card.append(head, pre);
  } else if (kind === 'edit') {
    card = el('div', 'tool-edit-card');
    if (patch) {
      const [added, removed] = diffCounts(patch);
      const receipt = el('div', 'tool-edit-receipt');
      receipt.append(
        el('span', 'tool-edit-title', '已确认改动'),
        el('span', 'tool-diffstat add', `+${added}`),
        el('span', 'tool-diffstat del', `−${removed}`),
      );
      const openChanges = el('button', 'button secondary small', '查看完整变更');
      openChanges.type = 'button';
      openChanges.addEventListener('click', event => {
        event.preventDefault();
        event.stopPropagation();
        openConfirmedChanges([tool.id], null);
      });
      receipt.append(openChanges);
      card.append(receipt);
    } else {
      card.append(el('pre', 'tool-output', output || '（无输出）'));
    }
    const openFile = toolFileButton(tool);
    if (openFile) card.append(openFile);
  } else if (kind === 'search') {
    card = el('div', 'tool-search-card');
    const digest = searchDigest(output);
    const head = el('div', 'tool-search-head');
    head.append(iconNode(tool.name === 'glob' || tool.name === 'find_files' ? 'search' : 'search', { size: 14 }));
    head.append(el('span', 'tool-search-title', toolLabel(tool)));
    head.append(el('span', 'tool-search-summary', digest ? `${digest.matches} 处匹配 · ${digest.files} 个文件` : output ? '搜索结果' : '无结果'));
    const openFile = toolFileButton(tool);
    if (openFile) head.append(openFile);
    head.append(toolCopyButton(output));
    card.append(head, el('pre', 'tool-search-output', output || '无结果'));
  } else if (kind === 'web') {
    card = el('div', 'tool-web-card');
    const data = parseToolJson(output);
    const head = el('div', 'tool-web-head');
    const title = tool.name === 'web_fetch' ? '网页获取' : '网页搜索';
    head.append(iconNode('globe', { size: 14 }), el('span', 'tool-web-title', title));
    const target = typeof args?.url === 'string' ? args.url : typeof args?.query === 'string' ? args.query : '';
    if (target) head.append(el('span', 'tool-web-target', target));
    card.append(head);
    const sources = Array.isArray(data?.sources) ? data.sources : [];
    if (sources.length) {
      const list = el('div', 'tool-web-sources');
      for (const source of sources.slice(0, 8)) {
        const row = el('button', 'tool-web-source');
        row.type = 'button';
        row.append(el('span', 'tool-web-source-title', String(source.title ?? source.url ?? '来源')), el('span', 'tool-web-source-url', String(source.url ?? '')));
        if (source.snippet) row.append(el('span', 'tool-web-source-snippet', String(source.snippet)));
        if (typeof source.url === 'string' && /^https?:/i.test(source.url)) row.addEventListener('click', () => openMessageLink(source.url));
        list.append(row);
      }
      card.append(list);
    } else {
      card.append(el('pre', 'tool-web-output', output || '（无内容）'));
    }
  } else if (kind === 'goal') {
    card = el('div', 'tool-details-card');
    const data = parseToolJson(output);
    const title = { create_goal: '创建目标', get_goal: '查看目标', update_goal: '更新目标' }[tool.name] ?? '目标';
    const head = el('div', 'tool-details-head');
    head.append(iconNode('compass', { size: 14 }), el('span', undefined, title));
    card.append(head);
    if (data && typeof data === 'object') {
      const objective = data.objective ?? data.goal?.objective ?? args?.objective;
      if (objective) card.append(el('div', 'tool-details-primary', String(objective)));
      const rows = [];
      const status = data.status ?? data.goal?.status;
      if (status) rows.push(['状态', String(status)]);
      const rounds = data.rounds_started ?? data.roundsStarted ?? data.goal?.rounds_started;
      const max = data.max_goal_rounds ?? data.maxGoalRounds ?? data.goal?.max_goal_rounds;
      if (rounds !== undefined || max !== undefined) rows.push(['执行轮次', `${rounds ?? '?'} / ${max ?? '?'}`]);
      const blocked = data.blocked_reason?.message ?? data.blockedReason?.message ?? data.goal?.blocked_reason?.message;
      if (blocked) rows.push(['受阻原因', String(blocked)]);
      for (const [label, value] of rows) {
        const row = el('div', 'tool-details-row');
        row.append(el('span', 'tool-details-label', label), el('span', 'tool-details-value', value));
        card.append(row);
      }
      if (!objective && !rows.length) card.append(el('pre', 'tool-output', output || '没有目标'));
    } else card.append(el('pre', 'tool-output', output || '没有目标'));
  } else if (kind === 'subagent') {
    card = el('div', 'tool-details-card');
    const head = el('div', 'tool-details-head');
    head.append(iconNode('bot', { size: 14 }), el('span', undefined, '创建子智能体'));
    card.append(head);
    const started = /started (background )?subagent(?: job)? (\S+)/i.exec(output);
    if (started) {
      const receipt = el('div', 'tool-receipt');
      receipt.append(el('span', 'tool-receipt-mark', '已启动'), el('span', 'tool-details-label', started[1] ? '任务 ID' : '智能体 ID'), el('span', 'tool-details-value', started[2]));
      card.append(receipt);
    } else {
      card.append(el('div', 'tool-details-primary', String(args?.prompt ?? args?.task ?? '子智能体')));
      card.append(el('pre', 'tool-output', output || '（无回复）'));
    }
  } else if (kind === 'job') {
    card = el('div', 'tool-details-card');
    const titles = { job_list: '查看后台任务', job_output: '读取任务输出', job_kill: '取消后台任务' };
    const head = el('div', 'tool-details-head');
    head.append(iconNode('clock', { size: 14 }), el('span', undefined, titles[tool.name] ?? '后台任务'));
    card.append(head);
    const status = /\[status:\s*([a-z_]+)\]/i.exec(output);
    if (status) card.append(el('span', 'tool-job-status', `状态：${status[1]}`));
    if (tool.name === 'job_kill') {
      const receipt = el('div', 'tool-receipt');
      receipt.append(el('span', 'tool-receipt-mark', tool.status === 'success' ? '已取消' : '未确认'));
      card.append(receipt);
    }
    card.append(el('pre', 'tool-output', output || '（无输出）'));
  } else if (kind === 'deliver') {
    card = el('div', 'tool-deliver-card');
    const head = el('div', 'tool-details-head');
    head.append(iconNode('upload', { size: 14 }), el('span', undefined, '交付文件'));
    head.append(el('span', 'tool-deliver-status', { running: '正在交付', success: '已交付', failed: '交付失败' }[tool.status] ?? '准备交付'));
    card.append(head);
    const files = (Array.isArray(args?.files) ? args.files : []).map(file => typeof file === 'string' ? file : file?.path).filter(Boolean);
    if (typeof args?.path === 'string') files.unshift(args.path);
    const listed = [...new Set(files)];
    for (const file of listed.slice(0, 4)) card.append(el('div', 'tool-deliver-file', String(file)));
    if (listed.length > 4) card.append(el('div', 'tool-deliver-more', `其余 ${listed.length - 4} 个文件`));
    if (output) card.append(el('pre', 'tool-output', output));
  } else if (kind === 'todo' && (Array.isArray(args?.todos) || Array.isArray(args?.items) || Array.isArray(args?.steps))) {
    card = el('div', 'tool-todo-card');
    const items = args.todos ?? args.items ?? args.steps;
    for (const item of items) {
      const text = typeof item === 'string' ? item : item?.content ?? item?.text ?? item?.description ?? '';
      const state = typeof item === 'object' ? item?.status ?? '' : '';
      const line = el('div', 'tool-todo-line');
      line.dataset.status = state;
      line.append(el('span', 'tool-todo-mark', { completed: '✓', done: '✓', in_progress: '●', running: '●' }[state] ?? '○'), el('span', undefined, String(text)));
      card.append(line);
    }
  } else {
    card = el('div', 'tool-io-card');
    for (const [label, text, error] of [['IN', argsRaw ?? '', false], ['OUT', output || '（无输出）', row.dataset.status === 'failed']]) {
      const line = el('div', 'tool-io-row');
      line.append(el('span', 'tool-io-label', label), el('pre', `tool-io-text${error ? ' error' : ''}`, text));
      card.append(line);
    }
  }
  detail.append(card);
  const raw = el('details', 'tool-raw');
  raw.append(el('summary', undefined, '技术详情'), el('pre', undefined, `工具：${tool.name}\n调用：${tool.id}\n参数：${argsRaw ?? ''}${tool.exit_code !== undefined ? `\n退出码：${tool.exit_code}` : ''}`));
  detail.append(raw);
}

function parseToolJson(output) {
  try { return JSON.parse(String(output ?? '')); } catch { return null; }
}

function searchDigest(output) {
  const lines = String(output ?? '').split('\n').filter(line => line.trim() !== '');
  const files = new Set();
  let matches = 0;
  for (const line of lines) {
    const located = /^([^:\n]+):(\d+)(?::|\s)/.exec(line);
    if (located) { files.add(located[1]); matches += 1; continue; }
    if (/^[^:\n]+\.[A-Za-z0-9]+$/.test(line.trim())) files.add(line.trim());
  }
  if (!matches && !files.size) return null;
  return { matches: matches || lines.length, files: files.size };
}

const workflowStatusLabels = {
  running: '运行中', completed: '已完成', failed: '失败', cancelled: '已取消', interrupted: '已中断',
};

function workflowRunPanel(tool, args, output) {
  const data = parseToolJson(output) ?? (typeof args === 'object' ? args : null);
  const run = data && Array.isArray(data.phases) ? data : null;
  const root = el('section', 'workflow-run-panel');
  root.dataset.status = run?.status ?? (tool.status === 'failed' ? 'failed' : 'running');
  const header = el('div', 'workflow-run-header');
  const chevron = el('button', 'workflow-run-toggle');
  chevron.type = 'button';
  chevron.setAttribute('aria-expanded', 'true');
  chevron.setAttribute('aria-label', '折叠工作流');
  chevron.append(iconNode('chevronUp', { size: 12 }));
  const members = run ? run.phases.reduce((sum, phase) => sum + (phase.members?.length ?? 0), 0) : 0;
  header.append(
    chevron,
    el('span', 'workflow-run-title', run?.name ?? String(toolLabel(tool) ?? '工作流')),
    el('span', 'workflow-run-separator'),
    el('span', 'workflow-run-summary', run ? `${members} 个成员` : '运行时未返回工作流结构'),
  );
  const status = el('span', 'workflow-run-status');
  status.dataset.status = run?.status ?? 'running';
  status.append(el('span', 'workflow-state-dot'), el('span', undefined, workflowStatusLabels[run?.status] ?? '运行中'));
  header.append(status);
  const body = el('div', 'workflow-run-phases');
  if (!run) body.append(el('pre', 'workflow-run-fallback', String(output ?? '（无输出）')));
  else if (!run.phases.length) body.append(el('span', 'workflow-run-empty', '没有启动成员'));
  else {
    for (const phase of run.phases) {
      const section = el('div', 'workflow-phase');
      const head = el('div', 'workflow-phase-header');
      head.append(
        iconNode('chevronDown', { size: 12 }),
        el('span', 'workflow-phase-title', phase.phase ?? '未分阶段'),
        el('span', 'workflow-run-separator'),
        el('span', 'workflow-phase-count', `${phase.members?.length ?? 0} 个成员`),
      );
      section.append(head);
      const list = el('div', 'workflow-members');
      for (const member of phase.members ?? []) {
        const row = el('div', 'workflow-member');
        const dot = el('span', 'workflow-state-dot');
        dot.dataset.status = member.status;
        row.append(dot, el('span', 'workflow-member-label', member.label || '空成员名'), el('span', 'workflow-member-status', workflowStatusLabels[member.status] ?? '未知'));
        list.append(row);
      }
      section.append(list);
      body.append(section);
    }
  }
  root.append(header, body);
  chevron.addEventListener('click', () => {
    const open = body.hidden;
    body.hidden = !open;
    chevron.setAttribute('aria-expanded', String(open));
    chevron.setAttribute('aria-label', open ? '折叠工作流' : '展开工作流');
    chevron.replaceChildren(iconNode(open ? 'chevronUp' : 'chevronDown', { size: 12 }));
  });
  return root;
}

function scheduleTurnCard(tool, args, output) {
  const data = parseToolJson(output) ?? (typeof args === 'object' ? args : null);
  const card = el('div', 'schedule-turn-card');
  card.dataset.tool = 'schedule_create';
  const leading = el('span', 'schedule-turn-leading');
  leading.append(iconNode('clock', { size: 18 }));
  const body = el('span', 'schedule-turn-body');
  const title = typeof data?.title === 'string' && data.title ? data.title
    : typeof args?.title === 'string' && args.title ? args.title : 'schedule_create';
  body.append(el('span', 'schedule-turn-title', title));
  const rule = data?.rule ?? data?.frequency ?? data?.cron ?? args?.rule ?? args?.frequency ?? args?.cron;
  const frequency = typeof rule === 'string' ? rule
    : rule && typeof rule === 'object' ? Object.entries(rule).map(([key, value]) => `${key} ${value}`).join(' · ') : '';
  body.append(el('span', 'schedule-turn-frequency', frequency || '规则未提供'));
  const open = el('button', 'button secondary small', '打开');
  open.type = 'button';
  open.title = '打开任务详情';
  open.addEventListener('click', () => openRightbarTab('schedule'));
  card.append(leading, body, open);
  return card;
}

function syncChildren(host, children) {
  for (const child of [...host.children]) if (!children.includes(child)) child.remove();
  for (const child of children) if (child.parentNode !== host) host.append(child);
}

function executionFilesNode(view) {
  const host = el('div', 'execution-files');
  host.append(el('div', 'execution-files-head', '已确认改动'));
  if (!view.files?.length) {
    host.append(el('div', 'execution-files-empty', '运行时没有提供该操作的 Diff。'));
    return host;
  }
  for (const file of view.files) {
    const row = el('button', 'execution-file-row');
    row.type = 'button';
    row.title = `${file.path} · +${file.added} −${file.removed}`;
    const glyph = el('span', 'execution-file-icon');
    glyph.append(fileIcon(file.path));
    row.append(
      glyph,
      el('span', 'execution-file-path', file.path),
      el('span', 'execution-file-stat add', `+${file.added}`),
      el('span', 'execution-file-stat del', `−${file.removed}`),
    );
    row.addEventListener('click', event => {
      event.preventDefault();
      event.stopPropagation();
      openConfirmedChanges(view.toolIds, file.path);
    });
    host.append(row);
  }
  return host;
}

function fillExecutionBody(node, view, existing) {
  const body = node.querySelector('.execution-body');
  const children = [];
  if (view.variant === 'edit') {
    const signature = JSON.stringify(view.files?.map(file => [file.path, file.added, file.removed]) ?? []);
    let files = node.querySelector('.execution-files');
    if (!files || files.dataset.signature !== signature) {
      const fresh = executionFilesNode(view);
      fresh.dataset.signature = signature;
      if (files) files.replaceWith(fresh);
      files = fresh;
    }
    children.push(files);
  }
  for (const id of view.tools) {
    const tool = current?.tools.find(item => item.id === id);
    if (tool) children.push(toolNode(existing, tool));
  }
  syncChildren(body, children);
}

/** One execution group: a sibling of narration, collapsed by default even while
 * it runs. Opening it reveals the real tool rows; an edit's summary opens the
 * Workbench instead, because the full Diff is not conversation text. */
function executionNode(existing, item) {
  const view = item.execution;
  const key = `exec:${view.id}:${view.variant}`;
  let node = existing.get(key);
  if (!node) {
    node = el('details', `execution-group execution-${view.variant}`);
    node.dataset.key = key;
    node.dataset.groupId = view.id;
    node.dataset.variant = view.variant;
    const summary = el('summary', 'execution-summary');
    summary.append(
      el('span', 'execution-glyph'),
      el('span', 'execution-label'),
      el('span', 'execution-action'),
      el('span', 'execution-status'),
      el('span', 'execution-failure'),
      el('span', 'execution-hint'),
    );
    node.append(summary, el('div', 'execution-body'));
    executionState.set(node, { view, existing });
    node.addEventListener('toggle', () => {
      const state = /** @type {{view:any,existing:Map<string,HTMLElement>}} */ (executionState.get(node));
      if (node.open) openExecution.add(state.view.id); else openExecution.delete(state.view.id);
      if (node.open) fillExecutionBody(node, state.view, state.existing);
      else node.querySelector('.execution-body').replaceChildren();
    });
    if (view.variant !== 'edit') {
      // Disclosure affordance: the row's leading mark names the activity, so
      // the fold arrow trails the summary instead of competing with it.
      const chevron = el('span', 'execution-chevron-wrap');
      chevron.append(iconNode('chevronDown', { size: 12 }));
      summary.append(chevron);
    }
    if (view.variant === 'edit') {
      const toggle = el('button', 'execution-toggle');
      toggle.type = 'button';
      toggle.setAttribute('aria-label', '展开修改详情');
      toggle.dataset.tooltip = '展开修改详情';
      toggle.append(iconNode('chevronRight', { size: 12, className: 'execution-chevron' }));
      summary.prepend(toggle);
      summary.addEventListener('click', event => {
        if (event.target.closest('.execution-toggle')) {
          event.preventDefault();
          node.open = !node.open;
          return;
        }
        event.preventDefault();
        const state = /** @type {{view:any,existing:Map<string,HTMLElement>}} */ (executionState.get(node));
        openConfirmedChanges(state.view.toolIds, null);
      });
    }
  }
  executionState.set(node, { view, existing });
  node.dataset.status = view.waiting ? 'waiting' : view.status;
  node.dataset.tone = view.failed ? 'error' : view.cancelled || view.unknown ? 'warning' : view.running ? 'active' : '';
  const summary = node.querySelector('.execution-summary');
  // The leading mark names the work (reference activity icon); the row itself
  // is the disclosure control, so a second caret is noise.
  const member = current?.tools.find(tool => tool.id === view.tools[0]);
  summary.querySelector('.execution-glyph').replaceChildren(iconNode(member ? toolIconName(member) : 'wrench', { size: 14 }));
  summary.querySelector('.execution-label').textContent = view.label;
  summary.querySelector('.execution-action').textContent = view.waiting
    ? '等待你的批准'
    : view.running ? view.liveAction : '';
  const status = executionStatusLabel(view);
  summary.querySelector('.execution-status').textContent = status ? `· ${status}` : '';
  summary.querySelector('.execution-failure').textContent = view.running ? '' : view.failure;
  summary.querySelector('.execution-hint').textContent = view.variant === 'edit' ? '查看变更' : '';
  node.open = openExecution.has(view.id);
  // Collapsed means nothing inside is painted: the real tool rows still exist
  // in the model's ledger and are built the moment the reader opens the group.
  if (node.open) fillExecutionBody(node, view, existing);
  else node.querySelector('.execution-body').replaceChildren();
  return node;
}

function turnReceiptNode(existing, item) {
  const terminal = item.terminal;
  const key = `receipt:${item.seq}`;
  let row = existing.get(key);
  if (!row) {
    row = el('div', 'turn-receipt');
    row.dataset.key = key;
    row.append(
      el('span', 'turn-receipt-mark'),
      el('span', 'turn-receipt-label'),
      el('span', 'turn-receipt-stats'),
      el('div', 'turn-receipt-detail'),
    );
  }
  row.dataset.status = terminal.status;
  row.dataset.tone = terminal.tone;
  row.querySelector('.turn-receipt-mark').textContent = terminal.glyph;
  row.querySelector('.turn-receipt-label').textContent = terminal.label;
  row.querySelector('.turn-receipt-stats').textContent = terminal.stats.join(' · ');
  const detail = row.querySelector('.turn-receipt-detail');
  detail.textContent = terminal.detail ?? '';
  detail.hidden = !terminal.detail;
  return row;
}

function openConfirmedChanges(toolIds, selected = null) {
  openRightbarTab('changes', { source: 'confirmed', toolIds, selected, title: '本次修改' });
}


function planCard() {
  const steps = current?.plan?.steps ?? [];
  if (!steps.length) return null;
  const done = steps.filter(step => step.status === 'done').length;
  const card = el('div', 'plan-card-row');
  card.dataset.key = 'plan-card';
  const main = el('button', 'plan-card-main');
  main.type = 'button';
  const icon = el('span', 'plan-card-icon');
  icon.append(iconNode('list', { size: 20 }));
  const copy = el('span', 'plan-card-copy');
  const planStateLabel = current?.plan?.state === 'completed' ? '（已完成）' : current?.plan?.state === 'interrupted' ? '（已中断）' : '';
  copy.append(el('span', 'plan-card-title', `执行计划${planStateLabel}`), el('span', 'plan-card-desc', `${done} / ${steps.length} 步骤已完成`));
  main.append(icon, copy, el('span', 'plan-card-open', '打开'));
  main.addEventListener('click', () => openRightbarTab('plan'));
  const review = el('button', 'plan-review-open', '审阅');
  review.type = 'button';
  review.title = '只读审阅；运行时未提供计划审批';
  review.addEventListener('click', () => openPlanReviewPanel());
  card.append(main, review);
  return card;
}

function openPlanReviewPanel() {
  const steps = current?.plan?.steps ?? [];
  if (!steps.length) return;
  const dialog = el('dialog', 'modal plan-review-modal');
  const card = el('div', 'plan-review-card');
  card.setAttribute('aria-label', '计划待审');
  const strip = el('div', 'plan-review-strip');
  strip.append(el('span', 'plan-review-dot'), el('span', undefined, '计划待审'));
  const summary = el('div', 'plan-review-summary');
  summary.append(el('h3', 'plan-review-title', steps[0]?.description ?? '执行计划'));
  if (steps.length > 1) summary.append(el('p', 'plan-review-description', steps.slice(1).map(step => step.description).join(' · ')));
  const note = el('p', 'plan-review-note', '当前 Runtime 没有计划审批与接管语义；此面板只读展示计划内容。');
  const footer = el('div', 'plan-review-footer');
  const feedback = el('div', 'plan-review-feedback', '');
  const actions = el('div', 'plan-review-actions');
  const revise = el('button', 'button secondary', '要求修改');
  const approve = el('button', 'button primary', '同意执行');
  for (const [button, reason] of [[revise, '运行时未提供计划修改请求'], [approve, '运行时未提供计划审批']]) {
    button.type = 'button';
    button.disabled = true;
    button.title = reason;
  }
  const close = el('button', 'button secondary', '关闭');
  close.type = 'button';
  close.addEventListener('click', () => dialog.close());
  actions.append(revise, approve, close);
  footer.append(feedback, actions);
  card.append(strip, summary, note, footer);
  dialog.append(card);
  dialog.addEventListener('close', () => { dialog.remove(); window.dispatchEvent(new Event('ui-overlay-change')); }, { once: true });
  document.body.append(dialog);
  dialog.showModal();
  window.dispatchEvent(new Event('ui-overlay-change'));
  close.focus();
}

function presentedFiles() {
  const out = [];
  const seen = new Set();
  for (const tool of current?.tools ?? []) {
    if (toolKind(tool.name) !== 'deliver') continue;
    const args = toolArgs(tool) ?? {};
    const files = Array.isArray(args.files) ? [...args.files] : [];
    if (typeof args.path === 'string') files.unshift(args.path);
    for (const file of files) {
      const path = typeof file === 'string' ? file : file?.path;
      if (typeof path !== 'string' || !path || seen.has(path)) continue;
      seen.add(path);
      out.push({ path, status: tool.status });
    }
  }
  return out;
}

function presentedFilesCard() {
  const files = presentedFiles();
  if (!files.length) return null;
  const wrap = el('div', 'presented-files-card');
  wrap.dataset.key = 'presented-files';
  for (const file of files.slice(0, 4)) {
    const row = el('button', 'presented-file');
    row.type = 'button';
    const icon = el('span', 'presented-file-icon');
    icon.append(iconNode('fileText', { size: 18 }));
    const body = el('span', 'presented-file-body');
    body.append(
      el('span', 'presented-file-name', file.path.split(/[\\/]/).filter(Boolean).at(-1) ?? file.path),
      el('span', 'presented-file-status', { running: '正在交付', success: '在侧边栏预览', failed: '交付失败' }[file.status] ?? '已请求打开'),
    );
    row.append(icon, body);
    row.addEventListener('click', () => openRightbarTab('deliverables'));
    wrap.append(row);
  }
  if (files.length > 4) wrap.append(el('div', 'presented-file-more', `其余 ${files.length - 4} 个文件`));
  return wrap;
}

function deliveredCard() {
  const report = current?.session?.completion_report ?? current?.completionReport;
  if (!report) return null;
  const card = el('button', 'plan-card-row');
  card.type = 'button';
  card.dataset.key = 'delivered-card';
  const icon = el('span', 'plan-card-icon');
  icon.append(iconNode('archive', { size: 20 }));
  const copy = el('span', 'plan-card-copy');
  copy.append(el('span', 'plan-card-title', '本轮交付'), el('span', 'plan-card-desc', `${report.files_changed ?? 0} 个文件变更 · +${report.added ?? 0} −${report.removed ?? 0}`));
  card.append(icon, copy, unconnected(), el('span', 'plan-card-open', '查看'));
  card.addEventListener('click', () => openRightbarTab('deliverables'));
  return card;
}

function renderConversation() {
  const list = $('conversation');
  const scroller = $('conversation-scroll');
  const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 120;
  if (!current) {
    list.replaceChildren();
    // No session owns a live clock; leaving the old start behind makes the
    // next turn's elapsed time inherit the previous one.
    if (runClockTimer) { clearInterval(runClockTimer); runClockTimer = null; }
    runStartedAt = null;
    document.body.classList.toggle('home', true);
    document.body.classList.toggle('blank-session', true);
    return;
  }
  const home = isHome();
  document.body.classList.toggle('home', home);
  document.body.classList.toggle('blank-session', home);
  const existing = new Map([...list.querySelectorAll('[data-key]')].map(node => [node.dataset.key, node]));
  const nodes = [];
  // The renderer paints the presentation projection; it does not decide what
  // a message is, where a group begins, or whether a turn answered.
  const items = conversationItems({ ...current, approvals: current.approvals ?? [] });
  const trailing = [];
  while (items.at(-1)?.kind === 'live_thought') trailing.unshift(items.pop());
  const approvalIds = new Set((current.approvals ?? []).map(item => String(item.call_id ?? '')));
  for (let index = 0; index < items.length; index += 1) {
    const item = items[index];
    if (item.kind === 'thought') continue;
    if (item.kind === 'user') {
      const node = userMessageNode(existing, item.message);
      node.classList.add('flow-turn');
      nodes.push(node);
    } else if (item.kind === 'narration' || item.kind === 'final') {
      const node = assistantTextNode(existing, item);
      node.classList.add('flow-response');
      nodes.push(node);
    } else if (item.kind === 'notice' || item.kind === 'summary') {
      nodes.push(runtimeNoticeNode(existing, item));
    } else if (item.kind === 'execution') {
      // Model steps do not create reader-visible headers: consecutive blocks of
      // one run paint as a single process row, like the reference transcript.
      // A call waiting for approval keeps its own visible row, so it is never
      // folded into a neighbour.
      const run = [item.execution];
      while (!run.at(-1).waiting && items[index + 1]?.kind === 'execution' && !items[index + 1].execution.waiting) {
        run.push(items[++index].execution);
      }
      const view = run.length > 1 ? mergeExecutionViews(run, current.tools, approvalIds) : item.execution;
      if (view.variant === 'tool') {
        const tool = current.tools.find(candidate => candidate.id === view.tools[0]);
        if (tool) nodes.push(toolNode(existing, tool));
      } else {
        nodes.push(executionNode(existing, { ...item, execution: view }));
      }
    } else if (item.kind === 'turn_receipt') {
      nodes.push(turnReceiptNode(existing, item));
    } else if (item.kind === 'turn_error') {
      nodes.push(turnErrorNode(item.error));
    }
  }
  const maxTokens = maxTokensNode(); if (maxTokens) nodes.push(maxTokens);
  const running = current.status === 'running';
  let runningRow = null;
  if (running) {
    runStartedAt ??= Date.now();
    let row = existing.get('running');
    if (!row) {
      row = el('div', 'running-row');
      row.dataset.key = 'running';
      const content = el('span', 'running-content');
      content.append(el('span', 'running-dot'), el('span', 'running-text'));
      row.append(el('span', 'running-divider'), content);
    }
    row.querySelector('.running-text').textContent = runningLabel();
    runningRow = row;
  }
  if (running && !runClockTimer) {
    runClockTimer = setInterval(() => {
      const text = $('conversation').querySelector('.running-row .running-text');
      if (text) text.textContent = runningLabel();
    }, 1000);
  }
  if (!running && runClockTimer) { clearInterval(runClockTimer); runClockTimer = null; runStartedAt = null; }
  for (const shell of current.session.user_shells ?? []) nodes.push(userShellNode(shell));
  const plan = planCard(); if (plan) nodes.push(plan);
  const delivered = deliveredCard(); if (delivered) nodes.push(delivered);
  const presented = presentedFilesCard(); if (presented) nodes.push(presented);
  // The live analysis is the turn's latest activity: it sits above the running
  // line, never above the answer it belongs to.
  for (const item of trailing) nodes.push(liveThoughtNode(existing, item.text));
  if (runningRow) nodes.push(runningRow);
  const keep = new Set(nodes);
  for (const child of [...list.children]) if (!keep.has(child)) child.remove();
  for (let i = 0; i < nodes.length; i += 1) if (list.children[i] !== nodes[i]) list.insertBefore(nodes[i], list.children[i] ?? null);
  if (nearBottom) scroller.scrollTop = scroller.scrollHeight;
  if (!$('find-bar').hidden) updateFind();
  renderTurnRail();
  updateScrollBottom();
}

function maxTokensNode() {
  if (current?.lastTerminal !== 'truncated') return null;
  const row = el('div', 'turn-max-tokens-row');
  row.dataset.key = 'max-tokens';
  const copy = el('div', 'turn-max-tokens-copy');
  copy.append(
    el('span', 'turn-max-tokens-title', '已达到输出 token 上限'),
    el('span', 'turn-max-tokens-text', '回答被截断，已有输出保留在对话中。发送“继续”可让模型接着输出。'),
  );
  row.append(el('span', 'turn-max-tokens-dot'), copy);
  return row;
}

function turnErrorNode(error) {
  const row = el('div', 'turn-error-row');
  row.dataset.key = `turn-error:${error.id}`;
  row.append(el('span', 'turn-error-dot'), el('span', 'turn-error-title', '本轮失败'), el('span', 'turn-error-text', error.text));
  return row;
}

function runningLabel() {
  const elapsed = runStartedAt ? Math.floor((Date.now() - runStartedAt) / 1000) : 0;
  const clock = elapsed >= 1 ? `，用时 ${elapsed}s ···` : ' ···';
  // Commands and other runtime activity own the line while they run; bare
  // "深度求索中" is only the fallback between activities (TUI precedence).
  const activity = String(current?.activity ?? '').trim();
  const base = activity ? `${activity}${clock}` : `深度求索中${clock}`;
  return modelRetry ? `${base} 正在重试模型请求（${modelRetry.attempt}/${modelRetry.max}）` : base;
}

function usagePill() {
  const usage = current?.usage;
  if (!usage) return null;
  if (!usage.input_tokens && !usage.output_tokens) return null;
  const parts = [`输入 ${usage.input_tokens ?? 0}`, `输出 ${usage.output_tokens ?? 0}`];
  if (usage.cached_input_tokens) parts.push(`缓存 ${usage.cached_input_tokens}`);
  if (settings.performance === 'detailed' && usage.reasoning_tokens) parts.push(`推理 ${usage.reasoning_tokens}`);
  const button = el('button', 'context-pill usage-pill');
  button.type = 'button';
  button.append(el('span', undefined, parts.join(' · ')));
  button.dataset.tooltip = 'Token 用量';
  button.setAttribute('aria-label', '查看 Token 用量');
  button.addEventListener('click', () => openUsageDialog(usage));
  return button;
}

function openUsageDialog(usage) {
  const dialog = el('dialog', 'modal usage-dialog');
  const card = el('div', 'modal-card');
  const head = el('div', 'modal-head');
  head.append(el('h2', undefined, 'Token 用量'));
  const close = iconButton('close', '关闭用量', { size: 16, tooltip: '关闭' });
  head.append(close);
  const body = el('div', 'usage-rows');
  const add = (label, value) => { const row = el('div', 'usage-row'); row.append(el('span', undefined, label), el('span', undefined, String(value))); body.append(row); };
  add('输入', usage.input_tokens ?? '未知');
  add('输出', usage.output_tokens ?? '未知');
  add('缓存读', usage.cached_input_tokens ?? 0);
  if (usage.reasoning_tokens !== undefined) add('推理', usage.reasoning_tokens);
  card.append(head, body);
  dialog.append(card);
  document.body.append(dialog);
  dialog.addEventListener('close', () => dialog.remove(), { once: true });
  close.addEventListener('click', () => dialog.close());
  dialog.showModal();
}

function userShellNode(shell) {
  const row = el('div', 'user-shell-row');
  row.dataset.key = `shell:${shell.id}`;
  const head = el('div', 'user-shell-head');
  head.append(iconNode('terminal', { size: 14 }), el('code', 'user-shell-command', shell.command || '（空命令）'), el('span', 'user-shell-status', shell.status ?? 'unknown'));
  if (shell.exit_code !== null && shell.exit_code !== undefined) head.append(el('span', 'user-shell-exit', `exit ${shell.exit_code}`));
  row.append(head);
  if (shell.output_tail) {
    const details = el('details', 'user-shell-output');
    details.append(el('summary', undefined, '输出'), el('pre', undefined, shell.output_tail));
    if (shell.output_truncated) details.append(el('p', 'user-shell-truncated', '较早输出已丢弃'));
    row.append(details);
  }
  return row;
}

function renderTurnRail() {
  const rail = $('turn-rail');
  if (!rail) return;
  const active = !!current && !isHome() && $('entry-page').hidden && activeView === 'chat';
  if (!active) { rail.hidden = true; return; }
  const turns = [...$('conversation').querySelectorAll('.message.user[data-message-id]')];
  if (turns.length < 2) { rail.hidden = true; return; }
  rail.replaceChildren();
  turns.forEach((node, index) => {
    const button = el('button', 'turn-rail-dot');
    button.type = 'button';
    button.setAttribute('aria-label', `跳转到第 ${index + 1} 轮`);
    button.dataset.tooltip = `第 ${index + 1} 轮`;
    button.addEventListener('click', () => node.scrollIntoView({ block: 'start', behavior: 'smooth' }));
    rail.append(button);
  });
  rail.hidden = false;
}

function updateScrollBottom() {
  const button = $('scroll-bottom');
  if (!button) return;
  const scroller = $('conversation-scroll');
  const active = !!current && !isHome() && $('entry-page').hidden && activeView === 'chat';
  const distance = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
  button.hidden = !active || distance < 160;
}

/* ------------------------------------------------------------- trajectory */

const TRAJECTORY_LIVE_ONLY = new Set([
  'token_usage', 'context_usage', 'context_loaded', 'agent_activity', 'command_progress',
  'notification', 'session_opened', 'session_updated', 'session_history_loaded',
  'agents_loaded', 'agent_loaded', 'agent_mutated', 'global_tasks_loaded',
  'observability_loaded', 'session_list', 'runtime_ready', 'model_retrying',
  'turn_finalizing', 'plan_updated', 'diff_updated', 'diff_failed', 'attachment_added',
  'attachment_processing_failed', 'prompt_suggestion', 'away_summary', 'project_rules_loaded',
  'context_updated', 'background_tasks_reconciled', 'memory_list', 'memory_recalled',
  'memory_changed', 'goal_recap_created', 'unfinished_goals_loaded', 'child_contribution_loaded',
]);
const TRAJECTORY_TERMINALS = new Set([
  'turn_completed', 'turn_completed_with_warnings', 'turn_answered', 'turn_cancelled',
  'task_cancelled', 'turn_truncated', 'turn_incomplete', 'turn_failed', 'session_completed',
]);

/**
 * A frame without `last_sequence` is UNKNOWN, not older: the client protocol
 * says the anchor is absent when unknown. Discarding such a frame silently
 * drops the runtime's settled status. Only a KNOWN, older anchor is stale.
 * @param {unknown} sequence
 */
function staleSequence(sequence) {
  return typeof sequence === 'number' && sequence < lastEventSeq;
}

function recordLiveTrajectory(event) {
  if (!current || !event) return;
  if (event.type === 'session_history_loaded') { liveTrajectory = []; liveTurnStartAt = null; liveTurnClosed = false; return; }
  if (TRAJECTORY_LIVE_ONLY.has(event.type)) return;
  const startsTurn = event.type === 'user_message_added' && (liveTurnClosed || liveTrajectory.length === 0);
  if (event.type === 'user_message_added' && liveTurnClosed) {
    liveTrajectory = [];
    liveTurnStartAt = null;
    liveTurnClosed = false;
  }
  if (liveTurnStartAt === null) liveTurnStartAt = Date.now();
  liveTrajectory.push({
    turn_elapsed_ms: Math.max(0, Date.now() - liveTurnStartAt),
    turn_start: startsTurn,
    event,
  });
  if (TRAJECTORY_TERMINALS.has(event.type)) liveTurnClosed = true;
  if (activeView === 'trajectory') scheduleTrajectory();
}

let trajectoryPaintPending = false;
function scheduleTrajectory() {
  if (trajectoryPaintPending) return;
  trajectoryPaintPending = true;
  requestAnimationFrame(() => {
    trajectoryPaintPending = false;
    if (activeView === 'trajectory') renderTrajectory();
  });
}

function trajectoryProjection() {
  return projectTrajectory([...history, ...liveTrajectory], { usage: current?.usage });
}

function trajectorySummary(record) {
  if (record.kind === 'tool') {
    const args = String(record.arguments ?? '').replace(/\s+/g, ' ').trim();
    const result = String(record.preview ?? '').replace(/\s+/g, ' ').trim();
    return `${record.toolName || '工具'} ${args}`.trim() + (result ? `  →  ${result}` : '');
  }
  if (record.kind === 'assistant') return record.text;
  return record.text;
}

function ensureTrajectoryDom() {
  const root = $('trajectory-root');
  if (root.dataset.ready === '1') return root;
  root.replaceChildren();
  const toolbar = el('div', 'trajectory-toolbar');
  toolbar.setAttribute('role', 'toolbar');
  toolbar.setAttribute('aria-label', '轨迹工具栏');
  const actions = el('div', 'trajectory-actions');
  const duration = el('button', 'trajectory-toggle');
  duration.type = 'button';
  duration.dataset.trajectoryAction = 'duration';
  duration.setAttribute('aria-label', '使用实际时长');
  duration.append(iconNode('clock', { size: 16 }), el('span', undefined, '时长'));
  duration.addEventListener('click', () => {
    trajectoryActualDuration = !trajectoryActualDuration;
    renderTrajectory();
  });
  const turns = el('button', 'trajectory-action');
  turns.type = 'button';
  turns.dataset.trajectoryAction = 'turns';
  turns.addEventListener('click', () => {
    trajectoryCollapseTurns = !trajectoryCollapseTurns;
    renderTrajectory();
  });
  const calls = el('button', 'trajectory-action');
  calls.type = 'button';
  calls.dataset.trajectoryAction = 'calls';
  calls.addEventListener('click', () => {
    trajectoryCollapseCalls = !trajectoryCollapseCalls;
    renderTrajectory();
  });
  actions.append(duration, turns, calls);
  const search = el('label', 'trajectory-search');
  search.append(iconNode('search', { size: 12 }));
  const input = el('input');
  input.type = 'search';
  input.id = 'trajectory-search';
  input.placeholder = '搜索';
  input.setAttribute('aria-label', '搜索轨迹');
  input.addEventListener('input', () => { trajectoryQuery = input.value; renderTrajectory(); });
  search.append(input);
  toolbar.append(actions, search);
  const timeline = el('div', 'trajectory-timeline');
  timeline.setAttribute('aria-label', '时间线概览；水平拖动可聚焦事件');
  const range = el('div', 'trajectory-range');
  range.hidden = true;
  const timeCursor = el('span', 'trajectory-time-cursor');
  timeCursor.hidden = true;
  timeline.append(range, timeCursor);
  const fractionAt = clientX => {
    const track = timeline.querySelector('.trajectory-lane-track');
    if (!track) return 0;
    const rect = track.getBoundingClientRect();
    return Math.max(0, Math.min(1, (clientX - rect.left) / Math.max(1, rect.width)));
  };
  const updateRange = (startFraction, endFraction) => {
    const domain = timelineDomain(timeline);
    const low = Math.min(startFraction, endFraction);
    const high = Math.max(startFraction, endFraction);
    trajectoryRange = high - low < 0.002 ? null : { start: domain.start + low * domain.duration, end: domain.start + high * domain.duration };
    range.hidden = trajectoryRange === null;
    if (trajectoryRange) {
      const track = timeline.querySelector('.trajectory-lane-track');
      const trackRect = track.getBoundingClientRect();
      const hostRect = timeline.getBoundingClientRect();
      range.style.left = `${trackRect.left - hostRect.left + low * trackRect.width}px`;
      range.style.width = `${Math.max(2, (high - low) * trackRect.width)}px`;
    }
    applyTrajectoryRange();
  };
  timeline.addEventListener('pointerdown', event => {
    if (event.button !== 0) return;
    if (event.target.closest('.trajectory-range')) return;
    event.preventDefault();
    trajectoryRangeDrag = fractionAt(event.clientX);
    timeline.setPointerCapture(event.pointerId);
    updateRange(trajectoryRangeDrag, trajectoryRangeDrag);
  });
  timeline.addEventListener('pointermove', event => {
    if (trajectoryRangeDrag === null) return;
    updateRange(trajectoryRangeDrag, fractionAt(event.clientX));
  });
  const endDrag = event => {
    if (trajectoryRangeDrag === null) return;
    updateRange(trajectoryRangeDrag, fractionAt(event.clientX));
    trajectoryRangeDrag = null;
  };
  timeline.addEventListener('pointerup', endDrag);
  timeline.addEventListener('pointercancel', endDrag);
  timeline.addEventListener('contextmenu', event => {
    event.preventDefault();
    if (trajectoryPanMoved) { trajectoryPanMoved = false; return; }
    if (trajectoryViewport) return;
    trajectoryRange = null;
    range.hidden = true;
    applyTrajectoryRange();
  });
  timeline.addEventListener('pointerdown', event => {
    if (event.button !== 2 || !trajectoryViewport || !trajectoryActualDuration) return;
    event.preventDefault();
    trajectoryPanDrag = { pointerId: event.pointerId, anchorX: event.clientX, anchorStart: timelineDomain(timeline).start };
    trajectoryPanMoved = false;
    timeline.setPointerCapture(event.pointerId);
    timeline.classList.add('panning');
  });
  timeline.addEventListener('pointermove', event => {
    if (trajectoryActualDuration) {
      const domain = timelineDomain(timeline);
      const track = timeline.querySelector('.trajectory-lane-track');
      if (track) {
        const trackRect = track.getBoundingClientRect();
        const fraction = Math.max(0, Math.min(1, (event.clientX - trackRect.left) / Math.max(1, trackRect.width)));
        timeCursor.textContent = formatDuration(domain.start + fraction * domain.duration) ?? '';
        timeCursor.style.left = `${trackRect.left - timeline.getBoundingClientRect().left + fraction * trackRect.width}px`;
        timeCursor.hidden = false;
      }
    }
    if (!trajectoryPanDrag) return;
    trajectoryPanMoved = true;
    const track = timeline.querySelector('.trajectory-lane-track');
    if (!track) return;
    const domain = timelineDomain(timeline);
    const delta = (event.clientX - trajectoryPanDrag.anchorX) / Math.max(1, track.getBoundingClientRect().width);
    const fullStart = domain.fullStart;
    const maxStart = domain.fullStart + domain.fullDuration - domain.duration;
    const start = Math.min(Math.max(trajectoryPanDrag.anchorStart - delta * domain.duration, fullStart), maxStart);
    trajectoryViewport = { start, end: start + domain.duration };
    renderTrajectoryTimeline(timeline, timeline._records ?? []);
    applyTrajectoryRange();
  });
  const endPan = event => {
    if (!trajectoryPanDrag || trajectoryPanDrag.pointerId !== event.pointerId) return;
    trajectoryPanDrag = null;
    timeline.classList.remove('panning');
  };
  timeline.addEventListener('pointerup', endPan);
  timeline.addEventListener('pointercancel', endPan);
  timeline.addEventListener('pointerleave', () => { timeCursor.hidden = true; });
  timeline.addEventListener('wheel', event => {
    if (!trajectoryActualDuration || !timeline._records?.length) return;
    event.preventDefault();
    const domain = timelineDomain(timeline);
    const track = timeline.querySelector('.trajectory-lane-track');
    if (!track) return;
    const trackRect = track.getBoundingClientRect();
    const fraction = Math.max(0, Math.min(1, (event.clientX - trackRect.left) / Math.max(1, trackRect.width)));
    const nextDuration = Math.min(domain.fullDuration, Math.max(Math.min(20, domain.fullDuration), domain.duration * Math.exp(event.deltaY * 0.0015)));
    if (nextDuration >= domain.fullDuration * 0.999) {
      trajectoryViewport = null;
      renderTrajectoryTimeline(timeline, timeline._records);
      applyTrajectoryRange();
      return;
    }
    const anchorTime = domain.start + fraction * domain.duration;
    const start = Math.min(Math.max(anchorTime - fraction * nextDuration, domain.fullStart), domain.fullStart + domain.fullDuration - nextDuration);
    trajectoryViewport = { start, end: start + nextDuration };
    renderTrajectoryTimeline(timeline, timeline._records);
    applyTrajectoryRange();
  }, { passive: false });
  const body = el('div', 'trajectory-body');
  const ledger = el('div', 'trajectory-ledger');
  ledger.tabIndex = 0;
  ledger.addEventListener('scroll', () => {
    trajectoryPinned = ledger.scrollHeight - ledger.scrollTop - ledger.clientHeight < 80;
  });
  ledger.addEventListener('keydown', event => {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter'].includes(event.key)) return;
    const rows = [...ledger.querySelectorAll('.trajectory-row[data-id]')];
    if (!rows.length) return;
    if (event.key === 'Enter') {
      if (trajectorySelectedId) { event.preventDefault(); renderTrajectory(); }
      return;
    }
    event.preventDefault();
    const currentIndex = rows.findIndex(row => row.dataset.id === trajectorySelectedId);
    const index = event.key === 'Home' ? 0
      : event.key === 'End' ? rows.length - 1
      : Math.max(0, Math.min(rows.length - 1, (currentIndex < 0 ? 0 : currentIndex) + (event.key === 'ArrowDown' ? 1 : -1)));
    trajectorySelectedId = rows[index].dataset.id;
    trajectoryDetailTab = 'overview';
    renderTrajectory();
    requestAnimationFrame(() => $('trajectory-root')?.querySelector(`.trajectory-row[data-id="${trajectorySelectedId}"]`)?.focus());
  });
  const separator = el('div', 'trajectory-detail-resize');
  separator.setAttribute('role', 'separator');
  separator.setAttribute('aria-label', '调整事件详情宽度');
  separator.setAttribute('aria-orientation', 'vertical');
  separator.tabIndex = 0;
  separator.hidden = true;
  const detail = el('div', 'trajectory-detail');
  detail.hidden = true;
  body.append(ledger, separator, detail);
  root.append(toolbar, timeline, body);
  separator.addEventListener('pointerdown', event => {
    if (detail.hidden) return;
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = trajectoryDetailWidth;
    separator.setPointerCapture(event.pointerId);
    const move = moveEvent => {
      trajectoryDetailWidth = Math.max(240, Math.min(Math.round(root.clientWidth * 0.6), startWidth + startX - moveEvent.clientX));
      detail.style.width = `${trajectoryDetailWidth}px`;
    };
    const up = () => {
      separator.removeEventListener('pointermove', move);
      separator.removeEventListener('pointerup', up);
    };
    separator.addEventListener('pointermove', move);
    separator.addEventListener('pointerup', up);
  });
  root.dataset.ready = '1';
  return root;
}

function renderTrajectoryTimeline(host, records) {
  for (const child of [...host.querySelectorAll('.trajectory-lane')]) child.remove();
  const timeline = buildTimeline(records, trajectoryActualDuration);
  host._records = records;
  let fullStart = Infinity;
  let fullEnd = 0;
  for (const item of timeline.items) {
    fullStart = Math.min(fullStart, item.start);
    fullEnd = Math.max(fullEnd, item.end);
  }
  if (!timeline.items.length || !Number.isFinite(fullStart)) { fullStart = 0; fullEnd = timeline.total; }
  const fullDuration = Math.max(1, fullEnd - fullStart);
  host.dataset.fullStart = String(fullStart);
  host.dataset.fullDuration = String(fullDuration);
  host.dataset.total = String(timeline.total);
  if (trajectoryActualDuration && trajectoryViewport) {
    const start = Math.max(fullStart, Math.min(trajectoryViewport.start, fullEnd - 1));
    const end = Math.min(fullEnd, Math.max(start + 1, trajectoryViewport.end));
    if (end - start >= fullDuration * 0.999) trajectoryViewport = null;
    else trajectoryViewport = { start, end };
  }
  const domainStart = trajectoryActualDuration && trajectoryViewport ? trajectoryViewport.start : fullStart;
  const domainDuration = trajectoryActualDuration && trajectoryViewport ? trajectoryViewport.end - trajectoryViewport.start : fullDuration;
  host.dataset.domainStart = String(domainStart);
  host.dataset.domainDuration = String(domainDuration);
  const lanes = [['input', '输入'], ['model', '模型'], ['tool', '工具']];
  for (const [laneId, label] of lanes) {
    const row = el('div', 'trajectory-lane');
    row.append(el('span', 'trajectory-lane-label', label));
    const track = el('div', 'trajectory-lane-track');
    for (const item of timeline.items.filter(entry => entry.lane === laneId)) {
      if (item.end < domainStart || item.start > domainStart + domainDuration) continue;
      const bar = el('span', 'trajectory-bar');
      bar.dataset.kind = item.kind;
      bar.dataset.start = String(item.start);
      bar.dataset.end = String(item.end);
      bar.style.left = `${((item.start - domainStart) / domainDuration) * 100}%`;
      bar.style.width = `max(2px, calc(${((item.end - item.start) / domainDuration) * 100}% - 2px))`;
      const record = records.find(entry => entry.id === item.id);
      bar.title = record ? `${record.label} · ${formatDuration(record.startedMs) ?? '计时不可用'}` : '';
      track.append(bar);
    }
    row.append(track);
    host.append(row);
  }
}

function timelineDomain(timeline) {
  return {
    start: Number(timeline?.dataset.domainStart) || 0,
    duration: Math.max(1, Number(timeline?.dataset.domainDuration) || 1),
    fullStart: Number(timeline?.dataset.fullStart) || 0,
    fullDuration: Math.max(1, Number(timeline?.dataset.fullDuration) || 1),
  };
}

function applyTrajectoryRange() {
  const root = $('trajectory-root');
  if (!root) return;
  const timelineEl = root.querySelector('.trajectory-timeline');
  const domain = timelineDomain(timelineEl);
  const overlay = root.querySelector('.trajectory-range');
  if (overlay) {
    overlay.hidden = trajectoryRange === null;
    if (trajectoryRange) {
      const timeline = timelineEl;
      const track = timeline?.querySelector('.trajectory-lane-track');
      if (timeline && track) {
        const trackRect = track.getBoundingClientRect();
        const hostRect = timeline.getBoundingClientRect();
        const low = Math.max(0, Math.min((trajectoryRange.start - domain.start) / domain.duration, (trajectoryRange.end - domain.start) / domain.duration));
        const high = Math.min(1, Math.max((trajectoryRange.start - domain.start) / domain.duration, (trajectoryRange.end - domain.start) / domain.duration));
        overlay.style.left = `${trackRect.left - hostRect.left + low * trackRect.width}px`;
        overlay.style.width = `${Math.max(2, (high - low) * trackRect.width)}px`;
      }
    }
  }
  for (const row of root.querySelectorAll('.trajectory-row[data-start]')) {
    const start = Number(row.dataset.start);
    const end = Number(row.dataset.end);
    row.toggleAttribute('data-outside', !!trajectoryRange && (end < trajectoryRange.start || start > trajectoryRange.end));
  }
  for (const bar of root.querySelectorAll('.trajectory-bar')) {
    const start = Number(bar.dataset.start);
    const end = Number(bar.dataset.end);
    bar.toggleAttribute('data-outside', !!trajectoryRange && (end < trajectoryRange.start || start > trajectoryRange.end));
  }
}

function trajectoryDetailTabs(record) {
  if (record.kind === 'tool') {
    const tabs = [['overview', '概述'], ['payload', '参数'], ['result', '结果']];
    if (record.appliedDiff) tabs.push(['diff', '差异']);
    tabs.push(['schema', 'Schema'], ['timing', '计时']);
    return tabs;
  }
  if (record.kind === 'assistant') {
    const tabs = [['overview', '概述'], ['preview', '预览']];
    if (record.usage) tabs.push(['usage', '用量']);
    tabs.push(['raw', '原始内容']);
    return tabs;
  }
  return [['overview', '概述'], ['raw', '原始内容']];
}

function detailCodeBlock(text, { empty = '不可用' } = {}) {
  if (!text) return el('div', 'trajectory-detail-empty', empty);
  const pre = el('pre', 'trajectory-code');
  pre.textContent = text;
  return pre;
}

function detailRow(label, value, { tone = '' } = {}) {
  const row = el('div', 'trajectory-detail-row');
  row.append(el('span', 'trajectory-detail-label', label));
  const cell = el('span', 'trajectory-detail-value', value);
  if (tone) cell.dataset.tone = tone;
  row.append(cell);
  return row;
}

function renderTrajectoryDetail(host, record, projection) {
  if (!record) { host.hidden = true; return; }
  const root = $('trajectory-root');
  host.hidden = false;
  host.style.width = `${trajectoryDetailWidth}px`;
  const separator = root.querySelector('.trajectory-detail-resize');
  if (separator) separator.hidden = false;
  const head = el('div', 'trajectory-detail-head');
  const title = el('div', 'trajectory-detail-title');
  title.append(el('span', 'trajectory-detail-kind', record.label));
  title.append(el('span', 'trajectory-detail-context', `第 ${record.turn} 轮${record.request ? ` · 第 ${record.request} 步` : ''}`));
  const close = iconButton('close', '关闭详情', { size: 15, tooltip: '关闭详情' });
  close.addEventListener('click', () => {
    trajectorySelectedId = null;
    renderTrajectory();
  });
  head.append(title, close);
  const tabs = el('div', 'trajectory-detail-tabs');
  const tabsSpec = trajectoryDetailTabs(record);
  if (!tabsSpec.some(([id]) => id === trajectoryDetailTab)) trajectoryDetailTab = 'overview';
  for (const [id, label] of tabsSpec) {
    const button = el('button', 'trajectory-detail-tab', label);
    button.type = 'button';
    button.setAttribute('role', 'tab');
    button.setAttribute('aria-selected', String(trajectoryDetailTab === id));
    button.addEventListener('click', () => { trajectoryDetailTab = id; renderTrajectory(); });
    tabs.append(button);
  }
  const body = el('div', 'trajectory-detail-body');
  const usage = record.usage;
  if (trajectoryDetailTab === 'overview') {
    const layer = record.kind === 'user' ? '用户消息' : record.kind === 'tool' || record.kind === 'assistant' ? '助手消息' : record.kind === 'context' ? '上下文' : record.label;
    body.append(detailRow('层级', layer));
    body.append(detailRow('状态', trajectoryStatusLabel(record.status)));
    if (record.kind === 'tool') {
      body.append(trajectorySection('参数', detailCodeBlock(record.arguments, { empty: '参数不可用' })));
      body.append(trajectorySection('结果', detailCodeBlock(record.preview, { empty: '结果不可用' })));
      body.append(trajectorySection('Schema', el('div', 'trajectory-detail-empty', 'Schema 不可用：Runtime 未记录工具 schema')));
    } else {
      if (record.kind === 'user') body.append(detailRow('图片', String(record.images ?? 0)));
      body.append(detailRow('Token 用量', usage ? `输入 ${usage.input ?? '?'} · 输出 ${usage.output ?? '?'}${usage.cached ? ` · 缓存 ${usage.cached}` : ''}` : '不可用（Runtime 未持久化用量）'));
      body.append(detailRow('时长', formatDuration(record.durationMs) ?? '不可用'));
      const markdown = el('div', 'markdown trajectory-detail-markdown');
      renderMarkdown(markdown, record.text || '（无文本）', { onLink: openMessageLink });
      body.append(trajectorySection('预览', markdown));
    }
    const timing = el('div', 'trajectory-detail-timing');
    timing.append(
      detailRow('轮内偏移', formatDuration(record.startedMs) ?? '不可用'),
      detailRow('总时长', formatDuration(record.durationMs) ?? '不可用'),
      detailRow('首 token 延迟', record.kind === 'assistant' ? (formatDuration(record.ttftMs) ?? '不可用') : '不适用于此记录'),
      detailRow('生成', record.kind === 'assistant' ? (formatDuration(record.durationMs) ?? '不可用') : '不适用于此记录'),
      detailRow('吞吐量', usage && record.durationMs ? '不可用（Runtime 未记录生成 token 时间）' : '不可用'),
    );
    body.append(trajectorySection('请求计时', timing));
  } else if (trajectoryDetailTab === 'payload') {
    body.append(detailCodeBlock(record.arguments, { empty: '参数不可用' }));
  } else if (trajectoryDetailTab === 'result') {
    body.append(detailCodeBlock(record.preview, { empty: '结果不可用' }));
  } else if (trajectoryDetailTab === 'diff') {
    body.append(detailCodeBlock(record.appliedDiff, { empty: '差异不可用' }));
  } else if (trajectoryDetailTab === 'usage') {
    const value = record.usage;
    if (!value) body.append(el('div', 'trajectory-detail-empty', 'Token 用量不可用：Runtime 未持久化该请求的用量'));
    else {
      body.append(
        detailRow('输入', value.input === null ? '未知' : String(value.input)),
        detailRow('输出', value.output === null ? '未知' : String(value.output)),
        detailRow('缓存读', String(value.cached ?? 0)),
        ...(value.reasoning === undefined ? [] : [detailRow('推理', String(value.reasoning))]),
      );
    }
  } else if (trajectoryDetailTab === 'schema') {
    body.append(el('div', 'trajectory-detail-empty', 'Schema 不可用：Runtime 未记录工具 schema'));
  } else if (trajectoryDetailTab === 'timing') {
    body.append(
      detailRow('开始时间', `${formatDuration(record.startedMs) ?? '不可用'}（轮内偏移）`),
      detailRow('总时长', formatDuration(record.durationMs) ?? '不可用'),
      detailRow('首 token 延迟', formatDuration(record.ttftMs) ?? '不可用'),
      detailRow('生成', record.kind === 'assistant' ? (formatDuration(record.durationMs) ?? '不可用') : '不适用'),
    );
  } else if (trajectoryDetailTab === 'preview') {
    const markdown = el('div', 'markdown trajectory-detail-markdown');
    renderMarkdown(markdown, record.text || '（无文本）', { onLink: openMessageLink });
    body.append(markdown);
  } else if (trajectoryDetailTab === 'raw') {
    body.append(detailCodeBlock(JSON.stringify({
      kind: record.kind, subtype: record.subtype, status: record.status,
      text: record.text, tool: record.toolName, arguments: record.arguments,
      result: record.preview, usage: record.usage, timing: {
        startedMs: record.startedMs, durationMs: record.durationMs, ttftMs: record.ttftMs,
      },
    }, null, 2), { empty: '原始内容不可用' }));
  }
  host.replaceChildren(head, tabs, body);
}

function trajectoryStatusLabel(status) {
  return {
    success: '已完成', completed: '已完成', failed: '失败', running: '进行中',
    stopped: '已停止', interrupted: '已中断',
  }[status] ?? status ?? '未知';
}

function trajectorySection(title, content) {
  const section = el('section', 'trajectory-detail-section');
  const heading = el('h3', 'trajectory-detail-heading');
  heading.append(el('span', undefined, title), iconNode('chevronRight', { size: 12 }));
  section.append(heading, content);
  return section;
}

function renderTrajectory() {
  const root = ensureTrajectoryDom();
  const active = !!current && !isHome() && $('entry-page').hidden && activeView === 'trajectory';
  root.hidden = !active;
  $('conversation-scroll').hidden = active;
  if (!active) {
    const separator = root.querySelector('.trajectory-detail-resize');
    if (separator) separator.hidden = true;
    return;
  }
  const projection = trajectoryProjection();
  const records = projection.records;
  const windowed = trajectoryWindow(records, trajectoryNodeLimit);
  const windowIds = new Set(windowed.records.map(record => record.id));
  const duration = root.querySelector('[data-trajectory-action="duration"]');
  duration.setAttribute('aria-pressed', String(trajectoryActualDuration));
  duration.title = trajectoryActualDuration ? '使用等宽时长' : '使用实际时长';
  const turns = root.querySelector('[data-trajectory-action="turns"]');
  turns.setAttribute('aria-label', trajectoryCollapseTurns ? '展开所有轮次' : '收起所有轮次');
  turns.setAttribute('aria-pressed', String(trajectoryCollapseTurns));
  turns.replaceChildren(iconNode(trajectoryCollapseTurns ? 'plus' : 'minus', { size: 12 }), el('span', undefined, '轮次'));
  const calls = root.querySelector('[data-trajectory-action="calls"]');
  calls.setAttribute('aria-label', trajectoryCollapseCalls ? '展开所有调用' : '收起所有调用');
  calls.setAttribute('aria-pressed', String(trajectoryCollapseCalls));
  calls.replaceChildren(iconNode(trajectoryCollapseCalls ? 'plus' : 'minus', { size: 12 }), el('span', undefined, '调用'));
  const search = root.querySelector('#trajectory-search');
  if (search.value !== trajectoryQuery) search.value = trajectoryQuery;
  renderTrajectoryTimeline(root.querySelector('.trajectory-timeline'), records);
  const ledger = root.querySelector('.trajectory-ledger');
  const matches = searchTrajectory(records, trajectoryQuery);
  const filtering = trajectoryQuery.trim() !== '';
  const table = el('table', 'trajectory-table');
  table.setAttribute('role', 'grid');
  const colgroup = el('colgroup');
  const eventCol = el('col', 'trajectory-event-column');
  const contentCol = el('col');
  colgroup.append(eventCol, contentCol);
  table.append(colgroup);
  const tbody = el('tbody');
  const selected = records.find(record => record.id === trajectorySelectedId) ?? null;
  if (!selected && trajectorySelectedId) trajectorySelectedId = null;
  let rendered = 0;
  if (!filtering && windowed.hasOlder) {
    const row = el('tr', 'trajectory-history-row');
    const cell = el('td');
    cell.colSpan = 2;
    const button = el('button', 'trajectory-history-button');
    button.type = 'button';
    button.append(iconNode('chevronUp', { size: 12 }), el('span', undefined, `加载更早（已显示最近 ${windowed.records.length} / ${windowed.total} 条）`));
    button.addEventListener('click', () => {
      trajectoryPinned = false;
      trajectoryNodeLimit += 50;
      renderTrajectory();
    });
    cell.append(button);
    row.append(cell);
    tbody.append(row);
  }
  const systemVisible = !filtering;
  if (systemVisible) {
    const row = el('tr', 'trajectory-row trajectory-system-row');
    row.dataset.kind = 'system';
    const typeCell = el('td', 'trajectory-type-cell');
    const chip = el('span', 'trajectory-kind', '系统');
    chip.dataset.kind = 'system';
    typeCell.append(chip);
    const contentCell = el('td', 'trajectory-content-cell');
    contentCell.append(el('span', 'trajectory-summary', '初始系统提示词 · 不可用：Runtime 未记录系统提示词'));
    row.append(typeCell, contentCell);
    tbody.append(row);
  }
  for (const turn of projection.turns) {
    const visible = turn.records.filter(record => filtering ? matches.has(record.id) : windowIds.has(record.id));
    if (filtering && visible.length === 0) continue;
    if (trajectoryCollapseTurns) {
      // Collapsed turns keep one summary row; the expanded ledger carries the
      // turn label inside the first row's gutter like the reference.
      const group = el('tr', 'trajectory-group-row');
      const groupCell = el('td');
      groupCell.colSpan = 2;
      const groupButton = el('button', 'trajectory-group-toggle');
      groupButton.type = 'button';
      groupButton.setAttribute('aria-expanded', 'false');
      groupButton.append(el('span', 'trajectory-turn-label', `第 ${turn.index} 轮`));
      groupButton.append(el('span', 'trajectory-turn-summary', `${visible.length} 条记录`));
      groupButton.addEventListener('click', () => { trajectoryCollapseTurns = false; renderTrajectory(); });
      groupCell.append(groupButton);
      group.append(groupCell);
      tbody.append(group);
      rendered += 1;
      continue;
    }
    let firstShown = true;
    let priorRequest = null;
    for (const record of visible) {
      if (trajectoryCollapseCalls && record.kind === 'tool') continue;
      const row = el('tr', 'trajectory-row');
      row.tabIndex = record.id === trajectorySelectedId ? 0 : -1;
      row.setAttribute('role', 'row');
      row.dataset.id = record.id;
      row.dataset.kind = record.kind;
      row.dataset.start = String(record.startedMs ?? 0);
      row.dataset.end = String(record.endedMs ?? record.startedMs ?? 0);
      if (record.id === trajectorySelectedId) row.setAttribute('aria-selected', 'true');
      const typeCell = el('td', 'trajectory-type-cell');
      if (firstShown) {
        typeCell.append(el('span', 'trajectory-turn-label', `第 ${turn.index} 轮`));
      } else if (record.request && record.request !== priorRequest) {
        const requestButton = el('button', 'trajectory-request-button', `#${record.request}`);
        requestButton.type = 'button';
        requestButton.setAttribute('aria-label', `请求 #${record.request}`);
        requestButton.title = `请求 #${record.request}`;
        requestButton.addEventListener('click', event => {
          event.stopPropagation();
          trajectorySelectedId = record.id;
          trajectoryDetailTab = 'overview';
          renderTrajectory();
        });
        typeCell.append(requestButton);
      }
      priorRequest = record.request || priorRequest;
      const chip = el('span', 'trajectory-kind', record.label);
      chip.dataset.kind = record.kind;
      typeCell.append(chip);
      const contentCell = el('td', 'trajectory-content-cell');
      contentCell.append(el('span', 'trajectory-summary', trajectorySummary(record) || '（空）'));
      if (record.kind === 'user' && record.images > 0) contentCell.append(el('span', 'trajectory-status', `图片 × ${record.images}`));
      if (record.kind === 'tool' && record.status !== 'success') contentCell.append(el('span', 'trajectory-status', record.status === 'running' ? '运行中' : record.status === 'failed' ? '失败' : record.status === 'stopped' ? '已停止' : '未知'));
      row.append(typeCell, contentCell);
      row.addEventListener('click', () => {
        trajectorySelectedId = record.id;
        trajectoryDetailTab = 'overview';
        renderTrajectory();
      });
      tbody.append(row);
      firstShown = false;
      rendered += 1;
    }
  }
  if (filtering && rendered === 0) {
    const row = el('tr', 'trajectory-empty-row');
    const cell = el('td');
    cell.colSpan = 2;
    cell.textContent = '无匹配记录';
    row.append(cell);
    tbody.append(row);
  }
  if (!records.length) {
    const row = el('tr', 'trajectory-empty-row');
    const cell = el('td');
    cell.colSpan = 2;
    cell.textContent = '暂无轨迹记录。Runtime 还没有为这个会话记录事件。';
    row.append(cell);
    tbody.append(row);
  }
  table.append(tbody);
  const scrollTop = ledger.scrollTop;
  ledger.replaceChildren(table);
  applyTrajectoryRange();
  if (trajectoryPinned) ledger.scrollTop = ledger.scrollHeight;
  else ledger.scrollTop = scrollTop;
  const detailHost = root.querySelector('.trajectory-detail');
  renderTrajectoryDetail(detailHost, selected, projection);
  if (!selected) {
    detailHost.hidden = true;
    const separator = root.querySelector('.trajectory-detail-resize');
    if (separator) separator.hidden = true;
  }
}

/* ----------------------------------------------------------- interactions */

function renderInteractions() {
  const host = $('takeover');
  const cards = [];
  if (uncertain) {
    const card = el('div', 'interaction-card retry-card');
    card.append(el('h3', undefined, '发送结果未知'));
    card.append(el('p', undefined, 'Runtime 可能已经接收此操作。重试将复用同一 command_id，避免重复执行。'));
    const actions = el('div', 'interaction-actions');
    const retry = el('button', 'button primary', '重试原操作');
    retry.disabled = loading;
    retry.addEventListener('click', () => action(async () => { await deliver(uncertain.command, uncertain); await refreshSnapshot(); }));
    actions.append(retry);
    card.append(actions);
    cards.push(card);
  }
  for (const request of current?.approvals ?? []) {
    const card = el('div', 'interaction-card approval-card');
    card.append(el('h3', undefined, '等待审批'));
    if (request.tool) card.append(el('div', 'approval-tool', request.tool));
    card.append(el('p', undefined, request.summary ?? ''));
    if (request.command) card.append(el('div', 'interaction-command', request.command));
    if (request.risks?.length) {
      const risks = el('ul', 'interaction-risks');
      for (const risk of request.risks) risks.append(el('li', undefined, risk));
      card.append(risks);
    }
    if (request.grant) {
      const details = el('details', 'interaction-details');
      details.append(
        el('summary', undefined, '技术详情'),
        el('pre', undefined, JSON.stringify({ tool: request.tool, grant: request.grant }, null, 2)),
      );
      card.append(details);
    }
    const actions = el('div', 'interaction-actions');
    for (const decision of approvalDecisions(request)) {
      const label = { approve_once: '允许一次', approve_session: '本会话允许', approve_always: '始终允许', deny: '拒绝' }[decision];
      const button = el('button', `button ${decision === 'deny' ? 'secondary' : 'primary'}`, label);
      button.disabled = loading;
      button.addEventListener('click', () => action(async () => {
        for (const item of actions.querySelectorAll('button')) item.disabled = true;
        await deliver({ type: 'approval_decision', request_id: request.id, decision });
        await refreshMetadata();
      }));
      actions.append(button);
    }
    card.append(actions);
    cards.push(card);
  }
  for (const request of current?.clarifications ?? []) {
    const card = el('div', 'interaction-card');
    card.append(el('h3', undefined, '需要你的回答'));
    const questions = request.questions?.length ? request.questions : [{ question: request.question, options: request.options, kind: 'single', allow_other: true }];
    // A single question is already the fieldset legend; repeating the same
    // headline above it reads as two questions.
    if (request.question && (questions.length > 1 || questions[0]?.question !== request.question)) {
      card.append(el('p', undefined, request.question));
    }
    const fields = [];
    for (const [index, question] of questions.entries()) {
      const group = el('fieldset', 'interaction-question');
      group.append(el('legend', undefined, question.question));
      const picks = [];
      for (const option of question.options ?? []) {
        const label = el('label', 'choice-row');
        const input = el('input');
        input.type = question.kind === 'multi' ? 'checkbox' : 'radio';
        input.name = `clarification-${request.id}-${index}`;
        input.value = option;
        label.append(input, document.createTextNode(option));
        group.append(label);
        picks.push(input);
      }
      let input = null;
      if (question.kind === 'text' || question.allow_other) {
        input = el('input', 'interaction-other');
        input.type = 'text';
        input.setAttribute('aria-label', question.question);
        input.placeholder = question.kind === 'text' ? '输入回答' : '其他回答';
        group.append(input);
      }
      card.append(group);
      fields.push({ question, input, picks });
    }
    const actions = el('div', 'interaction-actions');
    const submit = el('button', 'button primary', '提交回答');
    submit.disabled = loading;
    submit.addEventListener('click', () => action(async () => {
      const answers = fields.map(({ question, input, picks }) => clarificationAnswer(question, {
        picks: picks.filter(item => item.checked).map(item => item.value), text: input?.value ?? '',
      }));
      const answer = request.questions?.length
        ? fields.map(({ question }, index) => `${question.header || question.question}: ${answers[index]}`).join('\n')
        : answers[0];
      for (const [index, value] of answers.entries()) {
        const question = request.questions?.[index]?.question ?? request.question ?? '';
        clarificationReplies.set(String(value ?? ''), question);
      }
      await deliver({ type: 'answer_clarification', request_id: request.id, answer });
      await refreshMetadata();
    }));
    actions.append(submit);
    card.append(actions);
    cards.push(card);
  }
  host.replaceChildren(...cards);
  host.hidden = !cards.length;
  document.body.classList.toggle('composer-takeover', cards.length > 0);
  controls();
}

/* --------------------------------------------------------------- composer */

function openDirectoryPicker(onPicked = selectWorkspace) {
  if (document.querySelector('.directory-picker-modal')) return;
  const dialog = el('dialog', 'modal directory-picker-modal');
  const card = el('div', 'directory-picker-card');
  const head = el('div', 'directory-picker-head');
  head.append(el('h2', 'directory-picker-title', '选择工作区目录'));
  const crumbBar = el('div', 'directory-crumb-bar');
  const crumbs = el('div', 'directory-crumbs');
  const home = el('button', 'directory-crumb is-disabled', '主目录');
  home.type = 'button';
  home.disabled = true;
  home.title = '桌面桥接未提供目录列表';
  crumbs.append(home);
  const edit = el('button', 'directory-crumb-edit');
  edit.type = 'button';
  edit.disabled = true;
  edit.setAttribute('aria-label', '编辑路径');
  edit.title = '桌面桥接未提供目录列表';
  edit.append(iconNode('edit', { size: 14 }));
  crumbBar.append(crumbs, edit);
  head.append(crumbBar);
  const content = el('div', 'directory-picker-content');
  const empty = el('div', 'directory-picker-empty');
  empty.append(
    iconNode('folder', { size: 24 }),
    el('p', undefined, '应用内目录浏览未接入：桌面桥接没有目录列表读取接口。'),
    el('p', 'directory-picker-hint', '可使用系统选择器选择工作区目录。'),
  );
  const choose = el('button', 'button primary', '使用系统选择器');
  choose.type = 'button';
  empty.append(choose);
  const selected = el('div', 'directory-selected');
  selected.hidden = true;
  const selectedIcon = el('span', 'directory-selected-icon');
  selectedIcon.append(iconNode('folder', { size: 18 }));
  selected.append(selectedIcon, el('span', 'directory-selected-path'));
  content.append(empty, selected);
  const footer = el('div', 'directory-picker-footer');
  const create = el('button', 'button ghost small', '新建文件夹');
  create.type = 'button';
  create.disabled = true;
  create.title = '桌面桥接未提供目录写入';
  const hiddenToggle = el('button', 'directory-hidden-toggle', '显示隐藏文件');
  hiddenToggle.type = 'button';
  hiddenToggle.disabled = true;
  hiddenToggle.setAttribute('aria-pressed', 'false');
  hiddenToggle.title = '桌面桥接未提供目录列表';
  const gap = el('span', 'directory-footer-gap');
  const cancel = el('button', 'button secondary', '取消');
  const open = el('button', 'button primary', '打开');
  cancel.type = 'button';
  open.type = 'button';
  open.disabled = true;
  footer.append(create, hiddenToggle, gap, cancel, open);
  card.append(head, content, footer);
  dialog.append(card);
  document.body.append(dialog);
  let picked = null;
  const showPicked = folder => {
    picked = folder;
    crumbs.replaceChildren();
    const parts = String(folder.path ?? '').split('/').filter(Boolean);
    parts.forEach((part, index) => {
      if (index) crumbs.append(iconNode('chevronRight', { size: 12 }));
      crumbs.append(el('span', index === parts.length - 1 ? 'directory-crumb directory-crumb-current' : 'directory-crumb', part));
    });
    selected.querySelector('.directory-selected-path').textContent = folder.path ?? '';
    selected.hidden = false;
    empty.hidden = true;
    open.disabled = false;
  };
  choose.addEventListener('click', () => action(async () => {
    const folder = await api.chooseFolder();
    if (folder) showPicked(folder);
  }));
  cancel.addEventListener('click', () => dialog.close());
  open.addEventListener('click', () => {
    if (!picked) return;
    dialog.close();
    onPicked(picked);
  });
  dialog.addEventListener('cancel', event => { event.preventDefault(); dialog.close(); });
  dialog.addEventListener('close', () => { dialog.remove(); window.dispatchEvent(new Event('ui-overlay-change')); }, { once: true });
  dialog.showModal();
  window.dispatchEvent(new Event('ui-overlay-change'));
  choose.focus();
}

function openWorkspaceMenu(anchor) {
  const spaces = recentWorkspaces(tasks);
  const items = [];
  if (spaces.length) {
    items.push({ heading: '最近工作区' });
    for (const space of spaces.slice(0, 12)) {
      items.push({
        label: space.name,
        icon: 'folder',
        checked: current?.session.repository === space.path,
        onSelect: () => action(async () => {
          const folder = await api.chooseRecentWorkspace(space.path);
          if (folder) selectWorkspace(folder);
        }),
      });
    }
    items.push({ divider: true });
  }
  items.push({ label: '打开本地文件夹…', icon: 'folderPlus', onSelect: () => openDirectoryPicker() });
  items.push({ label: '不使用工作区', icon: 'chat', checked: !selection && !hasWorkspace(), onSelect: () => { selection = null; if (current && isHome()) current = null; render(); } });
  openMenu({ anchor, items, width: 280, side: 'top' });
}

function selectWorkspace(folder) {
  const text = workspaceDraftText({ isHome: document.body.classList.contains('home'), text: $('message').value, homeDraft: drafts.get('new-session') });
  stashDraft();
  if (current) {
    drafts.set('new-session', text);
    current = null;
    history = [];
    $('conversation').replaceChildren();
  }
  selection = folder;
  if (folder?.path) chosenWorkspaces.set(workspaceKey(folder.path), folder);
  currentSource = null;
  $('message').value = text;
  drafts.set('new-session', text);
  composerError('');
  render();
  $('message').focus();
}

function openPresetMenu(anchor) {
  openMenu({
    anchor, width: 250, side: 'top',
    items: [
      { heading: '内置' },
      { label: '标准模式', icon: 'bot', checked: true, onSelect: () => {} },
      { label: 'PTC 模式', icon: 'layers', disabled: true, badge: '未接入' },
      { label: '极简模式', icon: 'minus', disabled: true, badge: '未接入' },
      { label: '创造模式', icon: 'sparkle', disabled: true, badge: '未接入' },
      { divider: true },
      { label: 'Agent 预设切换', icon: 'bot', disabled: true, badge: '未接入' },
    ],
  });
}

function openModelMenu(anchor) {
  action(async () => {
    if (!current) await createTask();
    const models = current?.session.available_models ?? [];
    const items = [];
    if (!models.length) {
      items.push({ heading: '暂无可用模型' });
      items.push({ label: '打开模型设置', icon: 'settings', onSelect: () => openSettings('models') });
    } else {
      items.push({ heading: '模型' });
      for (const model of models) {
        items.push({
          label: model.model,
          badge: model.provider,
          checked: current.session.model?.provider === model.provider && current.session.model?.model === model.model,
          onSelect: () => action(async () => {
            if (uploadPending()) throw new Error('附件处理尚未完成，暂不能切换模型');
            await deliver({ type: 'select_model', session_id: current.session.id, model });
            await refreshMetadata();
            if (current.session.model?.provider !== model.provider || current.session.model?.model !== model.model) throw new Error('Runtime 尚未确认模型切换');
          }),
        });
      }
    }
    openMenu({ anchor, items, width: 280, side: 'top', align: 'end' });
  });
}

function openPermissionMenu(anchor) {
  action(async () => {
    if (!current) await createTask();
    openMenu({
      anchor, width: 300, side: 'top',
      items: PERMISSION_MODES.map(([mode, label]) => ({
        label,
        icon: mode === 'full_access' ? 'shieldCheck' : 'shield',
        checked: current.session.mode === mode,
        onSelect: () => action(async () => {
          if (uploadPending() || uncertain) throw new Error('请先处理当前未完成的操作');
          const id = current.session.id;
          await deliver(permissionCommand(id, mode));
          await refreshMetadata();
          if (!permissionConfirmed(current, id, mode)) throw new Error('Runtime 尚未确认权限模式切换');
        }),
      })),
    });
  });
}

function commandCatalog() {
  const frozen = loading || !!uncertain || uploadPending();
  const session = !!current;
  const entries = [
    ['add', 'file', '文件', true, 'paperclip', null, null],
    ['commands', 'goal', '目标', true, 'compass', null, null],
    ['commands', 'plan', '计划', false, 'list', '未接入', '桌面尚未接入计划模式切换'],
    ['commands', 'feedback', '反馈', true, 'feedback', null, null],
    ['commands', 'diff', '工作区变更', !!current?.session.repository, 'diff', null, '当前会话没有工作区'],
    ['commands', 'compact', '压缩', session, 'archive', null, null],
    ['commands', 'permission', '权限', session, 'shield', null, null],
    ['commands', 'model', '模型', session, 'layers', null, null],
    ['commands', 'export', '下载日志', false, 'download', '未接入', 'Runtime 未提供 Session 打包导出接口'],
  ];
  return entries.map(([group, id, label, available, icon, badge, reason]) => ({
    group, id, label, icon, badge,
    enabled: available && !frozen,
    reason: frozen ? '请先处理当前未完成操作' : reason ?? '请先选择或新建会话',
  }));
}

function openCommandMenu(candidates = commandCatalog()) {
  $('command-popover')?.remove();
  if (!candidates.length) return;
  const node = el('div', 'menu command-popover');
  node.id = 'command-popover';
  node.dataset.selected = candidates.find(item => item.enabled)?.id ?? candidates[0].id;
  node.setAttribute('role', 'listbox');
  let group = null;
  for (const command of candidates) {
    if (command.group !== group) {
      group = command.group;
      node.append(el('div', 'menu-heading', group === 'add' ? '添加' : '指令'));
    }
    const button = el('button', 'menu-item');
    button.type = 'button';
    button.dataset.commandId = command.id;
    button.setAttribute('role', 'option');
    button.setAttribute('aria-selected', String(command.id === node.dataset.selected));
    button.append(iconNode(command.icon ?? 'command', { size: 14 }), el('span', 'menu-label', command.label));
    if (!command.enabled) {
      button.disabled = true;
      button.append(unconnected(command.badge ?? undefined));
      button.title = command.reason ?? '当前不可用';
    }
    button.addEventListener('click', () => executeCommand(command.id));
    node.append(button);
  }
  node.append(el('div', 'menu-note', '↑↓ 选择 · Enter 执行 · Esc 关闭'));
  document.body.append(node);
  const card = $('composer-card').getBoundingClientRect();
  const height = node.getBoundingClientRect().height;
  node.style.left = `${Math.round(card.left + 8)}px`;
  node.style.top = `${Math.round(Math.max(8, Math.min(innerHeight - height - 8, card.top - height - 4)))}px`;
  node.style.width = `${Math.min(360, card.width - 16)}px`;
}

function openSlashMenu() {
  const value = $('message').value;
  const candidates = /^\/[a-z-]*$/.test(value.trim())
    ? commandCandidates(value, commandCatalog())
    : [];
  openCommandMenu(candidates);
}

function openAtMenu() {
  $('at-popover')?.remove();
  const node = el('div', 'menu command-popover');
  node.id = 'at-popover';
  node.append(el('div', 'menu-heading', '@ 引用'));
  node.append(el('div', 'menu-note', '文件引用与会话引用尚未接入当前 Runtime。'));
  for (const [icon, label] of [['file', '引用文件'], ['chat', '引用会话']]) {
    const item = el('button', 'menu-item');
    item.type = 'button';
    item.disabled = true;
    item.append(iconNode(icon, { size: 14 }), el('span', 'menu-label', label), unconnected());
    node.append(item);
  }
  document.body.append(node);
  const card = $('composer-card').getBoundingClientRect();
  const height = node.getBoundingClientRect().height;
  node.style.left = `${Math.round(card.left + 8)}px`;
  node.style.top = `${Math.round(Math.max(8, Math.min(innerHeight - height - 8, card.top - height - 4)))}px`;
  node.style.width = `${Math.min(320, card.width - 16)}px`;
}

function closeComposerPopovers() {
  $('command-popover')?.remove();
  $('at-popover')?.remove();
}

function insertCommand(id) {
  const command = commandCatalog().find(item => item.id === id);
  if (!command?.enabled) return;
  $('message').value = `/${id}`;
  closeComposerPopovers();
  $('message').focus();
  controls();
}

async function executeCommand(id) {
  const command = commandCatalog().find(item => item.id === id);
  if (!command?.enabled) { composerError(command?.reason ?? '命令当前不可用'); return; }
  closeComposerPopovers();
  if (id === 'file') { composerError(''); controls(); uploadAttachment(); return; }
  if (id === 'goal') { $('message').value = '/goal '; $('message').focus(); return; }
  $('message').value = '';
  composerError('');
  controls();
  if (id === 'feedback') { openFeedbackDialog(); return; }
  if (id === 'diff') {
    if (!current?.session.repository) { composerError('当前会话没有工作区'); return; }
    openRightbarTab('changes');
    requestDiff();
    return;
  }
  if (id === 'permission') { openPermissionMenu($('permission-picker')); return; }
  if (id === 'model') { openModelMenu($('model-picker')); return; }
  if (id === 'compact') {
    action(async () => {
      await deliver({ type: 'compact_context', session_id: current.session.id });
      toast('已请求压缩上下文', { tone: 'info', holdMs: 2200 });
    });
    return;
  }
  composerError(command.reason ?? '命令当前不可用');
}

/* --------------------------------------------------------------- lifecycle */

function dismissPage() {
  if (!entryPage) return;
  entryPage = null;
  $('entry-page').hidden = true;
  for (const button of document.querySelectorAll('.panel-row')) button.setAttribute('aria-current', 'false');
}

function newSession() {
  dismissPage();
  stashDraft();
  selection = null;
  current = null;
  history = [];
  uncertain = null;
  pendingSubmit = false;
  activeView = 'chat';
  liveTrajectory = [];
  liveTurnStartAt = null;
  liveTurnClosed = false;
  trajectorySelectedId = null;
  trajectoryNodeLimit = 50;
  trajectoryRange = null;
  trajectoryViewport = null;
  $('message').value = drafts.get('new-session') ?? '';
  $('conversation').replaceChildren();
  render();
  action(createTask);
}

async function createTask() {
  const draftText = $('message').value;
  const key = selection?.id ?? null;
  if (!pendingCreation || pendingCreation.key !== key) pendingCreation = { key, id: crypto.randomUUID() };
  const result = await api.createTask(key, pendingCreation.id);
  pendingCreation = null;
  history = [];
  uncertain = null;
  currentSource = result.source_id ?? null;
  current = attachExtras(projectSnapshot(result.session), result.session);
  lastEventSeq = result.session.last_sequence ?? 0;
  activeView = 'chat';
  liveTrajectory = [];
  liveTurnStartAt = null;
  liveTurnClosed = false;
  trajectorySelectedId = null;
  trajectoryNodeLimit = 50;
  trajectoryRange = null;
  trajectoryViewport = null;
  $('conversation').replaceChildren();
  loadRightbar(current.session.id);
  render();
  if (draftText) {
    $('message').value = draftText;
    resizeComposer();
    drafts.delete('new-session');
    saveDrafts();
  }
  await refreshTasks();
  if (settings.defaultPermission && settings.defaultPermission !== 'assisted') {
    try {
      await deliver(permissionCommand(current.session.id, settings.defaultPermission));
      await refreshMetadata();
    } catch (error) {
      toast(`默认权限未应用：${error.message}`, { tone: 'warning', holdMs: 4000 });
    }
  }
  $('message').focus();
  setTimeout(refreshContextUsage, 80);
}

async function createTaskInWorkspace(path) {
  const draftText = drafts.get('new-session') ?? '';
  const folder = chosenWorkspaces.get(workspaceKey(path)) ?? await api.chooseRecentWorkspace(path);
  if (!folder) return;
  selection = folder;
  current = null;
  history = [];
  $('message').value = draftText;
  drafts.set('new-session', draftText);
  $('conversation').replaceChildren();
  await createTask();
}

async function openTask(task) {
  if (uploadPending()) throw new Error('附件处理尚未完成，请先处理当前上传');
  dismissPage();
  closeFind();
  stashDraft();
  const version = ++opening;
  const result = await api.openTask({ id: task.id, source_id: task.source_id });
  if (version !== opening) return;
  history = [];
  selection = null;
  uncertain = null;
  pendingSubmit = false;
  if (task.primary_workspace) foldedGroups.delete(task.primary_workspace);
  currentSource = result.source_id ?? task.source_id;
  current = attachExtras(projectSnapshot(result.session), result.session);
  lastEventSeq = result.session.last_sequence ?? 0;
  activeView = 'chat';
  liveTrajectory = [];
  liveTurnStartAt = null;
  liveTurnClosed = false;
  trajectorySelectedId = null;
  trajectoryNodeLimit = 50;
  trajectoryRange = null;
  trajectoryViewport = null;
  $('message').value = drafts.get(current.session.id) ?? '';
  $('conversation').replaceChildren();
  loadRightbar(current.session.id);
  render();
  await loadHistory();
  setTimeout(refreshContextUsage, 80);
  $('conversation-scroll').scrollTop = $('conversation-scroll').scrollHeight;
  closeNarrowSidebar();
}

/** A snapshot row carries Runtime-observed age, not an anchor; publish one so
 * the live duration keeps moving between refreshes. */
function activeJobRecord(job, prior = []) {
  const known = prior.find(candidate => candidate.task_id === job.task_id);
  return {
    ...(known ?? {}),
    ...job,
    started_at: known?.started_at ?? job.started_at ?? Date.now() - (job.elapsed_ms ?? 0),
    output: known?.output ?? job.output ?? '',
  };
}

function attachExtras(state, snapshot) {
  return {
    ...state,
    children: snapshot.children ?? state.children ?? [],
    backgroundTasks: (snapshot.active_background_tasks ?? state.backgroundTasks ?? []).map(job => activeJobRecord(job)),
    usage: state.usage ?? null,
  };
}

async function refreshTasks() {
  const include = (settings.archivedFilter ?? 'default') !== 'default';
  const index = await api.listTasks(include ? { include_archived: true } : undefined);
  tasks = index.tasks ?? [];
  sourceErrors = index.source_errors ?? [];
  renderSidebar();
  if (sourceErrors.length) {
    const summary = sourceErrorSummary(sourceErrors);
    const banner = $('error-banner');
    banner.replaceChildren(el('div', undefined, summary.summary));
    const details = el('details', 'source-errors');
    details.append(el('summary', undefined, '查看错误详情'));
    for (const item of summary.details) details.append(el('p', undefined, `${item.count} 个来源：${item.message}`));
    banner.append(details);
    banner.hidden = false;
  }
}

async function refreshMetadata() {
  if (!current) return;
  const id = current.session.id;
  const snapshot = await api.snapshot(id);
  if (current?.session.id !== id) return;
  current = attachExtras(applySessionMetadata(current, snapshot), snapshot);
  renderHeader();
  renderComposerMeta();
  renderSidebar();
}

async function refreshSnapshot() {
  if (!current) return;
  const id = current.session.id;
  const snapshot = await api.snapshot(id);
  if (current?.session.id !== id) return;
  // Same rule as a broadcast snapshot: a response that predates live state
  // must not replace it (it is the terminal-time refresh, so a slow answer
  // could otherwise resurrect the running row).
  if (staleSequence(snapshot.last_sequence)) return;
  lastEventSeq = Math.max(lastEventSeq, snapshot.last_sequence ?? 0);
  const prior = current;
  const diffQuery = current.diffQuery;
  const diffError = current.diffError;
  const settledBackgroundTasks = current.settledBackgroundTasks ?? [];
  const priorBackgroundTasks = current.backgroundTasks ?? [];
  const replayed = projectSnapshot(snapshot, history);
  current = attachExtras(retainInterruptedThoughts(mergeRestoredConversation(replayed, prior), prior), snapshot);
  // The snapshot is the authority for session metadata, status and the plan;
  // the merge only protects the live transcript from a stale replay.
  current.session = replayed.session;
  current.status = replayed.status;
  current.plan = replayed.plan ?? current.plan;
  current.approvals = replayed.approvals;
  current.clarifications = replayed.clarifications;
  current.settledBackgroundTasks = settledBackgroundTasks;
  const activeJobs = (snapshot.active_background_tasks ?? []).map(job => activeJobRecord(job, priorBackgroundTasks));
  const activeIds = new Set(activeJobs.map(job => job.task_id));
  current.backgroundTasks = [...activeJobs, ...priorBackgroundTasks.filter(job => !activeIds.has(job.task_id))];
  current.diffQuery = diffQuery;
  if (!snapshot.diff) current.diffError = diffError;
  render();
}

async function loadHistory() {
  if (!current) return;
  await deliver({ type: 'query_session_history', session_id: current.session.id, query_id: crypto.randomUUID() });
}

// A reconnect snapshot carries messages the old replay buffer never saw.
// Rebuild from the snapshot now, then replace it with the fresh durable log.
function reloadHistory() {
  loadHistory().catch(error => toast(`历史加载失败：${error.message}`, { tone: 'error', holdMs: 4000 }));
}

async function deliver(command, envelope = commandEnvelope(current.session.id, command, current.session.last_sequence ?? 0)) {
  const result = await api.deliver(envelope);
  if (!result.ok) {
    if (result.error.message.includes('version conflict')) await refreshMetadata();
    if (['outcome_unknown', 'transport'].includes(result.error.kind)) {
      uncertain = envelope;
      renderInteractions();
    }
    throw new Error(result.error.message);
  }
  if (uncertain?.command_id === envelope.command_id) uncertain = null;
  if (command.type === 'submit_message' && command.session_id === current?.session.id) {
    if ($('message').value.trim() === command.content) $('message').value = '';
    const sent = new Set((command.attachments ?? []).map(ref => ref.id));
    attachmentDrafts.set(draftKey(), attachments().filter(item => !sent.has(item.attachment?.id)));
    renderAttachments();
  }
  renderInteractions();
  return result;
}

/* ------------------------------------------------------------- attachments */

function renderAttachments() {
  const list = $('attachment-chips');
  const items = attachments();
  list.hidden = !items.length;
  list.replaceChildren();
  for (const item of items) {
    const chip = el('div', 'attachment-chip');
    chip.dataset.attachmentStatus = item.status;
    const status = {
      uploading: '选择 / 上传中…', queued: '正在处理…', unknown: '上传结果未知', failed: '上传失败',
      ready: item.attachment?.kind === 'image' ? '已上传' : '已上传 · 当前不能发送给模型',
    }[item.status];
    // An unknown outcome may still have uploaded successfully. Showing the
    // definite failure wording here would claim a result nobody observed.
    const problem = item.status === 'unknown' ? null : item.error?.message ? attachmentErrorPresentation(item.error.message) : null;
    const name = item.attachment?.name ?? '附件';
    const icon = el('span', 'attachment-icon');
    icon.append(fileIcon(name));
    const body = el('span', 'attachment-body');
    body.append(el('span', 'attachment-name', name), el('small', undefined, problem?.label ?? status));
    const actions = el('span', 'attachment-actions');
    chip.append(icon, body, actions);
    if (item.status === 'unknown' && item.import_id) {
      const discard = el('button', 'attachment-action', '放弃等待');
      discard.type = 'button';
      discard.title = '只放弃本地等待，不会取消 Runtime 上传';
      discard.addEventListener('click', () => action(async () => {
        const id = current.session.id;
        const ownerKey = draftKey();
        const result = await api.discardUpload(id, item.import_id);
        if (!result?.ok) throw new Error('未能放弃本地等待');
        attachmentDrafts.set(ownerKey, uploadDraft(attachmentDrafts.get(ownerKey) ?? [], { remove: item.key }));
        renderAttachments();
      }));
      actions.append(discard);
    }
    if (['unknown', 'failed'].includes(item.status)) {
      const retry = el('button', 'attachment-action', item.status === 'unknown' ? '重试原上传' : '重新选择');
      retry.type = 'button';
      retry.addEventListener('click', () => uploadAttachment(item));
      actions.append(retry);
    }
    if (!['queued', 'unknown', 'uploading'].includes(item.status)) {
      const remove = el('button', 'attachment-remove');
      remove.type = 'button';
      remove.setAttribute('aria-label', '移除附件草稿');
      remove.append(iconNode('close', { size: 12 }));
      remove.addEventListener('click', () => {
        attachmentDrafts.set(draftKey(), uploadDraft(attachments(), { remove: item.attachment?.id ?? item.key }));
        renderAttachments();
      });
      actions.append(remove);
    }
    list.append(chip);
  }
  controls();
}

async function uploadAttachment(previous = null) {
  composerError('');
  await action(async () => {
    if (!current) await createTask();
    uploadingKey = previous?.key ?? crypto.randomUUID();
    const key = uploadingKey;
    const sessionId = current.session.id;
    const receiving = { sessionId, key, events: [] };
    attachmentEarly = receiving;
    attachmentDrafts.set(sessionId, uploadDraft(attachmentDrafts.get(sessionId) ?? [], {
      status: 'uploading', key, import_id: previous?.status === 'unknown' ? previous.import_id : undefined,
    }));
    renderAttachments();
    try {
      const result = await api.chooseAttachment(sessionId);
      const draft = attachmentDrafts.get(sessionId) ?? [];
      const next = result.status === 'cancelled' ? uploadDraft(draft, { remove: key }) : reconcileUploadReply(draft, { ...result, key }, receiving.events);
      attachmentDrafts.set(sessionId, next);
      renderAttachments();
    } catch (error) {
      attachmentDrafts.set(sessionId, uploadDraft(attachmentDrafts.get(sessionId) ?? [], { status: 'failed', key, error: { message: error.message } }));
      renderAttachments();
    } finally {
      if (attachmentEarly === receiving) attachmentEarly = null;
      uploadingKey = null;
    }
  }, { onError: composerError, clearGlobal: false });
}

/* ---------------------------------------------------------------- message */

function localFileReference(value) {
  const text = String(value ?? '');
  if (!text || /^[a-z][a-z0-9+.-]*:/i.test(text) || text.startsWith('#') || text.startsWith('/')) return null;
  const match = /^([^#]+?)(?:#L(\d+)(?:-L?(\d+))?)?$/.exec(text);
  if (!match) return null;
  let path;
  try { path = decodeURIComponent(match[1]); } catch { path = match[1]; }
  path = path.replace(/^\.\//, '');
  if (!path || !/\.[A-Za-z0-9]{1,8}$/.test(path)) return null;
  return { path, line: Number(match[2]) || null, lineEnd: Number(match[3]) || Number(match[2]) || null };
}

function openLocalReference(reference) {
  if (!reference) return false;
  if (!current?.session?.repository) {
    toast('当前会话没有工作区，无法打开文件', { tone: 'info', holdMs: 2600 });
    return true;
  }
  openPreviewForPath(reference.path, reference.path.split(/[\\/]/).filter(Boolean).at(-1), { line: reference.line, lineEnd: reference.lineEnd });
  return true;
}

async function openMessageLink(url) {
  if (openLocalReference(localFileReference(url))) return;
  if (settings.linkOpen === 'system') {
    try { await api.openExternal(url); }
    catch (error) { toast(`无法打开链接：${error.message}`, { tone: 'error', holdMs: 4000 }); }
    return;
  }
  const tab = openRightbarTab('browser');
  if (!tab) return;
  await ensureBrowserTab(tab);
  await browserCommand('navigate', { url }, tab);
  renderRightbar();
}

/* ------------------------------------------------------------------- find */

function bodyTextNodes(container) {
  const walk = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
  const nodes = [];
  let node;
  while ((node = walk.nextNode())) if (!node.parentElement.closest('[data-ui-action],button')) nodes.push(node);
  return nodes;
}

function textRange(container, start, end) {
  const nodes = bodyTextNodes(container);
  let offset = 0;
  let first = null;
  let last = null;
  for (const node of nodes) {
    const length = node.textContent.length;
    if (first === null && start < offset + length) first = { node, offset: start - offset };
    if (end <= offset + length) { last = { node, offset: end - offset }; break; }
    offset += length;
  }
  if (!first || !last) return null;
  const range = document.createRange();
  range.setStart(first.node, first.offset);
  range.setEnd(last.node, last.offset);
  return range;
}

function openFind() {
  if (!current) return;
  $('find-bar').hidden = false;
  $('find-input').focus();
  $('find-input').select();
  updateFind(true);
}
function closeFind() {
  $('find-bar').hidden = true;
  window.CSS?.highlights?.delete('find-matches');
  window.CSS?.highlights?.delete('find-current');
}
function updateFind(scroll = false) {
  const query = $('find-input').value.trim();
  const rows = [...$('conversation').querySelectorAll('.message')].map(row => ({
    row, text: bodyTextNodes(row).map(node => node.textContent).join(''),
  }));
  findMatches = [];
  if (query) {
    const escaped = Array.from(query).map(char => '\\^$.*+?()[]{}|'.includes(char) ? `\\${char}` : char).join('');
    const matcher = new RegExp(escaped, 'giu');
    for (const { row, text } of rows) for (const match of text.matchAll(matcher)) {
      findMatches.push({ row, start: match.index, end: match.index + match[0].length });
    }
  }
  findSelected = Math.max(0, Math.min(findSelected, findMatches.length - 1));
  $('find-count').textContent = `${findMatches.length ? findSelected + 1 : 0} / ${findMatches.length}`;
  $('find-prev').disabled = !findMatches.length;
  $('find-next').disabled = !findMatches.length;
  if (!window.CSS?.highlights || !window.Highlight) return;
  const ranges = [];
  const selected = [];
  for (const [index, match] of findMatches.entries()) {
    const range = textRange(match.row, match.start, match.end);
    if (range) { ranges.push(range); if (index === findSelected) selected.push(range); }
    if (scroll && index === findSelected) match.row.scrollIntoView({ block: 'center' });
  }
  window.CSS.highlights.set('find-matches', new Highlight(...ranges));
  window.CSS.highlights.set('find-current', new Highlight(...selected));
}
function moveFind(direction) {
  if (!findMatches.length) return;
  findSelected = (findSelected + direction + findMatches.length) % findMatches.length;
  updateFind(true);
}

function openHistoryMenu(anchor) {
  if (!current) return;
  const questions = historyQuestions(current.messages);
  openMenu({
    anchor, width: 340, align: 'end',
    items: questions.length
      ? questions.map(question => ({
        label: question.text.replace(/\s+/g, ' ').slice(0, 64), icon: 'message',
        onSelect: () => {
          const row = [...$('conversation').querySelectorAll('.message')].find(node => node.dataset.messageId === question.id);
          row?.scrollIntoView({ block: 'center' });
        },
      }))
      : [{ label: '当前会话还没有提问', disabled: true }],
  });
}

/* ---------------------------------------------------------------- rightbar */

const RIGHT_KEY = 'codeleveler.rightbar.v1';
let rightbarOpen = false;
let rightbarExpanded = false;
let rightbarWidth = null;
let rightbarSessionKey = null;
let rightbarTabs = {};
let rightbarPanes = [{ id: 'pane1', tabs: [], activeId: null }];
let rightbarActivePane = 'pane1';
let rightbarSeq = 1;
let browserData = { tabs: [], selectedId: null };
let browserError = null;
let paneShare = 0.5;
let rightbarFloats = [];

function loadRightbar(sessionKey) {
  rightbarSessionKey = sessionKey;
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem(RIGHT_KEY) || '{}'); } catch { saved = {}; }
  const layout = saved?.bySession?.[sessionKey];
  if (layout && Array.isArray(layout.panes) && layout.panes.length) {
    rightbarPanes = layout.panes.map(pane => ({ id: pane.id, tabs: [...(pane.tabs ?? [])], activeId: pane.activeId ?? null }));
    rightbarTabs = { ...(layout.tabs ?? {}) };
    rightbarActivePane = layout.activePane && rightbarPanes.some(pane => pane.id === layout.activePane) ? layout.activePane : rightbarPanes[0].id;
    rightbarSeq = Number(layout.seq) || 1;
    rightbarExpanded = !!layout.expanded;
    paneShare = Number(layout.paneShare) >= .2 && Number(layout.paneShare) <= .8 ? Number(layout.paneShare) : .5;
    rightbarFloats = Array.isArray(layout.floats) ? layout.floats.filter(f => rightbarTabs[f.tabId]) : [];
  } else {
    const tab = newTabRecord('start');
    rightbarPanes = [{ id: 'pane1', tabs: [tab.id], activeId: tab.id }];
    rightbarTabs = { [tab.id]: tab };
    rightbarActivePane = 'pane1';
    rightbarExpanded = false;
    rightbarFloats = [];
  }
  const singleton = new Set(['files', 'changes', 'plan', 'deliverables', 'subagent']);
  const seen = new Set();
  for (const pane of rightbarPanes) {
    pane.tabs = pane.tabs.filter(id => {
      const tab = rightbarTabs[id];
      if (!tab) return false;
      if (singleton.has(tab.kind)) {
        if (seen.has(tab.kind)) { delete rightbarTabs[id]; return false; }
        seen.add(tab.kind);
      }
      return true;
    });
  }
  for (const pane of rightbarPanes) if (!pane.activeId || !pane.tabs.includes(pane.activeId)) pane.activeId = pane.tabs[0] ?? null;
  rightbarPanes = rightbarPanes.filter(pane => pane.tabs.length);
  if (!rightbarPanes.length) {
    const tab = newTabRecord('start');
    rightbarPanes = [{ id: 'pane1', tabs: [tab.id], activeId: tab.id }];
    rightbarTabs = { [tab.id]: tab };
    rightbarActivePane = 'pane1';
  }
  // Start is the pinned tab: a layout saved before it was pinned gets one back
  // instead of leaving the pane without its guide, active tab untouched.
  if (!rightbarPanes.some(pane => pane.tabs.some(id => rightbarTabs[id]?.kind === 'start'))) {
    const start = newTabRecord('start');
    rightbarTabs[start.id] = start;
    rightbarPanes[0].tabs.unshift(start.id);
  }
}

function persistRightbar() {
  if (!rightbarSessionKey) return;
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem(RIGHT_KEY) || '{}'); } catch { saved = {}; }
  saved.bySession ??= {};
  saved.bySession[rightbarSessionKey] = {
    panes: rightbarPanes, tabs: rightbarTabs, activePane: rightbarActivePane,
    seq: rightbarSeq, expanded: rightbarExpanded, paneShare, floats: rightbarFloats,
  };
  try { localStorage.setItem(RIGHT_KEY, JSON.stringify(saved)); } catch { /* optional */ }
}

function tabIcon(tab) {
  if (tab.kind === 'preview' && tab.path) return fileIcon(tab.path);
  const glyph = iconNode(TAB_ICON[tab.kind] ?? 'file', { size: tab.kind === 'browser' ? 14 : 16, className: 'tab-icon' });
  return glyph;
}

function changesTabTitle() {
  return '工作区变更';
}

const TAB_TITLES = {
  start: '开始', files: '文件', terminal: '终端', browser: '浏览器', changes: '变更',
  preview: '预览', deliverables: '交付', plan: '计划', subagent: '子智能体', schedule: '任务详情',
};

function tabTitle(kind, extra = {}) {
  return extra.title ?? TAB_TITLES[kind] ?? kind;
}

function newTabRecord(kind, extra = {}) {
  const id = `tab${rightbarSeq++}`;
  return { id, kind, title: tabTitle(kind, extra), ...extra };
}

function ensureRightbarSession() {
  const key = current?.session.id ?? 'new-session';
  if (key !== rightbarSessionKey) loadRightbar(key);
}
function activePane() { return rightbarPanes.find(pane => pane.id === rightbarActivePane) ?? rightbarPanes[0]; }

function openRightbarTab(kind, extra = {}) {
  ensureRightbarSession();
  const confirmed = kind === 'changes' && extra.source === 'confirmed';
  if (kind === 'changes' && !confirmed && !hasWorkspace()) { toast('当前会话没有工作区', { tone: 'warning' }); return null; }
  const pane = activePane();
  // Start is a pinned tab: opening something always adds a tab beside it
  // instead of consuming it.
  const reusable = kind === 'preview'
    ? Object.values(rightbarTabs).find(tab => tab.kind === 'preview' && tab.path === extra.path && !rightbarFloats.some(float => float.tabId === tab.id))
    : ['files', 'changes', 'plan', 'deliverables', 'subagent', 'schedule'].includes(kind)
      ? Object.values(rightbarTabs).find(tab => tab.kind === kind && !rightbarFloats.some(float => float.tabId === tab.id)) : null;
  const tab = reusable ?? newTabRecord(kind, extra);
  if (kind === 'changes') {
    const previous = { source: tab.source ?? 'workspace', toolIds: JSON.stringify(tab.toolIds ?? []) };
    if (confirmed) {
      tab.source = 'confirmed';
      tab.toolIds = [...new Set(extra.toolIds ?? [])];
      tab.title = extra.title ?? '本次修改';
      if (previous.source !== 'confirmed' || previous.toolIds !== JSON.stringify(tab.toolIds)) tab.selected = null;
    } else {
      tab.source = 'workspace';
      tab.toolIds = null;
      tab.title = changesTabTitle();
    }
    if (extra.selected !== undefined && extra.selected !== null) tab.selected = extra.selected;
  }
  if (reusable) {
    const owner = rightbarPanes.find(item => item.tabs.includes(reusable.id));
    if (owner) {
      owner.activeId = reusable.id;
      rightbarActivePane = owner.id;
    } else {
      pane.tabs.push(reusable.id);
      pane.activeId = reusable.id;
      rightbarActivePane = pane.id;
    }
  } else {
    rightbarTabs[tab.id] = tab;
    pane.tabs.push(tab.id);
    pane.activeId = tab.id;
    rightbarActivePane = pane.id;
  }
  rightbarOpen = true;
  rightbarExpanded = false;
  persistRightbar();
  renderRightbar();
  if (kind === 'changes' && tab.source !== 'confirmed') requestDiff();
  return tab;
}

function openRightbar(kind = null) {
  ensureRightbarSession();
  if (kind) { openRightbarTab(kind); return; }
  rightbarOpen = true;
  if (innerWidth < 1024) document.body.classList.remove('sidebar-open');
  persistRightbar();
  renderRightbar();
}
function closeRightbar() {
  rightbarOpen = false;
  rightbarExpanded = false;
  persistRightbar();
  renderRightbar();
  scheduleBrowserSurface();
}
async function closeTab(pane, tabId) {
  const index = pane.tabs.indexOf(tabId);
  if (index < 0) return;
  const closing = rightbarTabs[tabId];
  if (closing?.kind === 'terminal') {
    try { await terminals.close(closing, rightbarSessionKey); }
    catch (error) { toast(error.message, { tone: 'warning' }); return; }
    if (rightbarTabs[tabId] !== closing) return;
  }
  closeNativeBrowserTab(closing);
  pane.tabs.splice(index, 1);
  delete rightbarTabs[tabId];
  if (pane.activeId === tabId) pane.activeId = pane.tabs[Math.max(0, index - 1)] ?? pane.tabs[0] ?? null;
  if (!pane.tabs.length) {
    if (rightbarPanes.length > 1) {
      rightbarPanes = rightbarPanes.filter(item => item !== pane);
      rightbarActivePane = rightbarPanes[0].id;
    } else {
      const tab = newTabRecord('start');
      rightbarTabs[tab.id] = tab;
      pane.tabs.push(tab.id);
      pane.activeId = tab.id;
      rightbarOpen = false;
      rightbarExpanded = false;
    }
  }
  persistRightbar();
  renderRightbar();
  // A dock tab can disappear while its native browser binding is already null.
  // Recompute the surface so a stale WebContentsView never sits above the page.
  scheduleBrowserSurface();
}
function splitRightbar() {
  ensureRightbarSession();
  if (rightbarPanes.length > 1) {
    const [first, second] = rightbarPanes;
    for (const tabId of second.tabs) if (!first.tabs.includes(tabId)) first.tabs.push(tabId);
    rightbarPanes = [first];
    rightbarActivePane = first.id;
  } else {
    const pane = rightbarPanes[0];
    const moving = pane.activeId;
    const second = { id: 'pane2', tabs: [], activeId: null };
    if (moving && pane.tabs.length > 1) {
      pane.tabs = pane.tabs.filter(id => id !== moving);
      second.tabs.push(moving);
      second.activeId = moving;
      pane.activeId = pane.tabs[0] ?? null;
    } else {
      const tab = newTabRecord('start');
      rightbarTabs[tab.id] = tab;
      second.tabs.push(tab.id);
      second.activeId = tab.id;
    }
    rightbarPanes.push(second);
    rightbarActivePane = second.id;
  }
  persistRightbar();
  renderRightbar();
}
function toggleRightbarFullscreen() {
  rightbarExpanded = !rightbarExpanded;
  persistRightbar();
  renderRightbar();
}

function renderRightbar() {
  ensureRightbarSession();
  const bar = $('rightbar');
  const overlay = document.body.classList.contains('rightbar-overlay');
  document.body.classList.toggle('rightbar-open', rightbarOpen);
  document.body.classList.toggle('rightbar-expanded', rightbarOpen && rightbarExpanded);
  bar.hidden = !rightbarOpen;
  $('right-resize').hidden = !rightbarOpen || overlay || rightbarExpanded;
  updateRightbarButtons();
  if (!rightbarOpen) { renderFloats(); scheduleBrowserSurface(); return; }
  const container = $('right-panes');
  container.replaceChildren();
  for (const pane of rightbarPanes) {
    const paneNode = el('section', 'right-pane');
    paneNode.dataset.pane = pane.id;
    if (rightbarPanes.length > 1 && pane === rightbarPanes[0]) {
      paneNode.style.flex = `0 0 calc(${(paneShare * 100).toFixed(2)}% - 4px)`;
    }
    paneNode.addEventListener('pointerdown', () => {
      if (rightbarActivePane !== pane.id) { rightbarActivePane = pane.id; persistRightbar(); }
      void selectBrowserTab(rightbarTabs[pane.activeId]);
    });
    const head = el('div', 'pane-head');
    const strip = el('div', 'tab-strip');
    strip.setAttribute('role', 'tablist');
    const items = el('div', 'tab-items');
    const hasGuide = pane.tabs.some(id => rightbarTabs[id]?.kind === 'start');
    for (const tabId of pane.tabs) {
      const tab = rightbarTabs[tabId];
      if (!tab) continue;
      const button = el('div', 'pane-tab');
      button.setAttribute('role', 'tab');
      button.setAttribute('aria-selected', String(pane.activeId === tabId && rightbarActivePane === pane.id));
      button.tabIndex = 0;
      button.draggable = tab.kind !== 'start';
      button.dataset.tabId = tabId;
      button.append(tabIcon(tab), el('span', 'tab-title', tab.title));
      if (tab.kind !== 'start') {
        const close = el('button', 'tab-close');
        close.type = 'button';
        close.setAttribute('aria-label', `关闭 ${tab.title}`);
        close.append(iconNode('close', { size: 14 }));
        close.addEventListener('click', event => { event.stopPropagation(); closeTab(pane, tabId); });
        button.append(close);
      }
      button.addEventListener('click', () => {
        pane.activeId = tabId;
        rightbarActivePane = pane.id;
        persistRightbar();
        renderRightbar();
        void selectBrowserTab(rightbarTabs[tabId]);
      });
      button.addEventListener('keydown', event => {
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); button.click(); return; }
        if (event.key === 'Delete' || event.key === 'Backspace') { event.preventDefault(); closeTab(pane, tabId); return; }
        if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
        event.preventDefault();
        const tabs = [...items.querySelectorAll('.pane-tab')];
        const index = tabs.indexOf(button);
        const next = event.key === 'Home' ? tabs[0]
          : event.key === 'End' ? tabs.at(-1)
            : tabs[(index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length];
        next?.focus();
      });
      button.addEventListener('contextmenu', event => {
        event.preventDefault();
        openMenu({
          anchor: button,
          items: [
            { label: '关闭', icon: 'close', disabled: tab.kind === 'start', onSelect: () => closeTab(pane, tabId) },
            { label: '关闭其他', icon: 'close', onSelect: () => { for (const id of [...pane.tabs]) if (id !== tabId && rightbarTabs[id]?.kind !== 'start') closeTab(pane, id); } },
            { divider: true },
            { label: '向右分栏', icon: 'columns', disabled: rightbarPanes.length > 1, onSelect: splitRightbar },
            { label: '浮动窗口', icon: 'external', onSelect: () => floatTab(pane, tabId) },
          ],
        });
      });
      button.addEventListener('dragstart', event => {
        event.dataTransfer.setData('text/plain', tabId);
        event.dataTransfer.effectAllowed = 'move';
        button.classList.add('dragging');
      });
      button.addEventListener('dragend', () => button.classList.remove('dragging'));
      items.append(button);
    }
    items.addEventListener('dragover', event => { event.preventDefault(); event.dataTransfer.dropEffect = 'move'; });
    items.addEventListener('drop', event => {
      event.preventDefault();
      // The tab-strip drop is consumed here. Re-rendering detaches the event
      // target, so the document-level drop handler would otherwise see a
      // detached node and float the tab instead of moving it.
      event.stopPropagation();
      const movedId = event.dataTransfer.getData('text/plain');
      const source = rightbarPanes.find(item => item.tabs.includes(movedId));
      if (!source || !rightbarTabs[movedId]) return;
      source.tabs = source.tabs.filter(id => id !== movedId);
      if (source.activeId === movedId) source.activeId = source.tabs[0] ?? null;
      const target = event.target.closest('.pane-tab');
      let index = target ? pane.tabs.indexOf(target.dataset.tabId) : pane.tabs.length;
      if (index < 0) index = pane.tabs.length;
      // Start is pinned first; a drop can never land before it.
      const startIndex = pane.tabs.findIndex(id => rightbarTabs[id]?.kind === 'start');
      if (startIndex >= 0 && index <= startIndex) index = startIndex + 1;
      pane.tabs.splice(index, 0, movedId);
      pane.activeId = movedId;
      rightbarActivePane = pane.id;
      rightbarPanes = rightbarPanes.filter(item => item.tabs.length);
      if (!rightbarPanes.length) rightbarPanes = [pane];
      persistRightbar();
      renderRightbar();
    });
    strip.append(items, el('div', 'tab-strip-fill'));
    if (!hasGuide) {
      const add = el('button', 'add-tab');
      add.type = 'button';
      add.setAttribute('aria-label', '新标签页');
      add.dataset.tooltip = '新标签页';
      add.append(iconNode('plus', { size: 14 }));
      add.addEventListener('click', () => {
        const tab = newTabRecord('start');
        rightbarTabs[tab.id] = tab;
        pane.tabs.push(tab.id);
        pane.activeId = tab.id;
        rightbarActivePane = pane.id;
        persistRightbar();
        renderRightbar();
      });
      strip.append(add);
    }
    const chrome = el('div', 'pane-chrome');
    if (pane === rightbarPanes.at(-1)) {
    if (rightbarPanes.length === 1) {
      const split = el('button', 'icon-button split-button');
      split.type = 'button';
      split.setAttribute('aria-label', '分栏');
      const wide = bar.getBoundingClientRect().width >= 560;
      split.disabled = !wide;
      split.dataset.tooltip = wide ? '分栏' : '栏宽不足，拖宽侧边栏后再分栏';
      split.append(iconNode('columns', { size: 16 }));
      split.addEventListener('click', splitRightbar);
      chrome.append(split);
    }
    const full = el('button', 'icon-button');
    full.type = 'button';
    full.setAttribute('aria-label', rightbarExpanded ? '退出全屏' : '全屏');
    full.dataset.tooltip = rightbarExpanded ? '退出全屏' : '全屏';
    full.append(iconNode(rightbarExpanded ? 'restore' : 'expand', { size: 16 }));
    full.addEventListener('click', toggleRightbarFullscreen);
    const close = el('button', 'icon-button');
    close.type = 'button';
    close.setAttribute('aria-label', '收起右侧边栏');
    close.dataset.tooltip = '收起侧边栏';
    close.append(iconNode('panel', { size: 16 }));
    close.style.transform = 'scaleX(-1)';
    close.addEventListener('click', closeRightbar);
    chrome.append(full, close);
    }
    head.append(strip, chrome);
    const body = el('div', 'pane-body');
    const tab = pane.activeId ? rightbarTabs[pane.activeId] : null;
    if (tab) body.append(renderTabView(tab, pane));
    paneNode.append(head, body);
    container.append(paneNode);
    if (pane === rightbarPanes[0] && rightbarPanes.length > 1) {
      const splitHandle = el('div', 'pane-split-handle');
      splitHandle.setAttribute('role', 'separator');
      splitHandle.setAttribute('aria-orientation', 'vertical');
      splitHandle.tabIndex = 0;
      splitHandle.addEventListener('pointerdown', event => {
        event.preventDefault();
        splitHandle.setPointerCapture(event.pointerId);
        const box = container.getBoundingClientRect();
        const move = moveEvent => {
          const next = Math.max(.2, Math.min(.8, (moveEvent.clientX - box.left) / box.width));
          paneShare = next;
          paneNode.style.flex = `0 0 calc(${(next * 100).toFixed(2)}% - 4px)`;
        };
        const up = () => {
          splitHandle.removeEventListener('pointermove', move);
          splitHandle.removeEventListener('pointerup', up);
          splitHandle.removeEventListener('pointercancel', up);
          persistRightbar();
        };
        splitHandle.addEventListener('pointermove', move);
        splitHandle.addEventListener('pointerup', up, { once: true });
        splitHandle.addEventListener('pointercancel', up, { once: true });
      });
      splitHandle.addEventListener('keydown', event => {
        if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
        event.preventDefault();
        paneShare = Math.max(.2, Math.min(.8, paneShare + (event.key === 'ArrowLeft' ? .04 : -.04)));
        paneNode.style.flex = `0 0 calc(${(paneShare * 100).toFixed(2)}% - 4px)`;
        persistRightbar();
      });
      container.append(splitHandle);
    }
  }
  renderFloats();
  scheduleBrowserSurface();
}

function renderFloats() {
  let layer = document.getElementById('float-layer');
  if (!layer) { layer = el('div', 'float-layer'); layer.id = 'float-layer'; document.body.append(layer); }
  layer.replaceChildren();
  layer.hidden = !rightbarFloats.length;
  if (layer.hidden) return;
  for (const float of rightbarFloats) {
    const tab = rightbarTabs[float.tabId];
    if (!tab) continue;
    const node = el('div', 'rightbar-float');
    node.style.left = `${float.x}px`; node.style.top = `${float.y}px`;
    node.style.width = `${float.w}px`; node.style.height = `${float.h}px`; node.style.zIndex = String(float.z ?? 1);
    node.addEventListener('pointerdown', () => {
      float.z = Math.max(1, ...rightbarFloats.map(item => item.z ?? 1)) + 1;
      node.style.zIndex = String(float.z);
      void selectBrowserTab(tab);
    });
    const head = el('div', 'float-head');
    head.append(tabIcon(tab), el('span', 'float-title', tab.title));
    const spacer = el('div', 'composer-spacer');
    const back = iconButton('columns', '返回停靠区', { size: 15, tooltip: '返回停靠区' });
    back.addEventListener('click', event => { event.stopPropagation(); dockFloat(float); });
    const close = iconButton('close', '关闭浮动窗口', { size: 15, tooltip: '关闭' });
    close.addEventListener('click', event => { event.stopPropagation(); removeFloat(float); });
    head.append(spacer, back, close);
    const body = el('div', 'float-body');
    body.append(renderTabView(tab, null));
    const grip = el('div', 'float-grip');
    node.append(head, body, grip);
    head.addEventListener('pointerdown', event => {
      if (event.target.closest('button')) return;
      event.preventDefault();
      head.setPointerCapture(event.pointerId);
      const startX = event.clientX, startY = event.clientY, x0 = float.x, y0 = float.y;
      const move = moveEvent => {
        float.x = Math.max(0, Math.min(innerWidth - 220, x0 + moveEvent.clientX - startX));
        float.y = Math.max(0, Math.min(innerHeight - 140, y0 + moveEvent.clientY - startY));
        node.style.left = `${float.x}px`; node.style.top = `${float.y}px`;
        requestAnimationFrame(scheduleBrowserSurface);
      };
      const up = () => { head.removeEventListener('pointermove', move); persistRightbar(); };
      head.addEventListener('pointermove', move);
      head.addEventListener('pointerup', up, { once: true });
    });
    grip.addEventListener('pointerdown', event => {
      event.preventDefault(); event.stopPropagation();
      grip.setPointerCapture(event.pointerId);
      const startX = event.clientX, startY = event.clientY, w0 = float.w, h0 = float.h;
      const move = moveEvent => {
        float.w = Math.max(220, w0 + moveEvent.clientX - startX);
        float.h = Math.max(140, h0 + moveEvent.clientY - startY);
        node.style.width = `${float.w}px`; node.style.height = `${float.h}px`;
        requestAnimationFrame(scheduleBrowserSurface);
      };
      const up = () => { grip.removeEventListener('pointermove', move); persistRightbar(); };
      grip.addEventListener('pointermove', move);
      grip.addEventListener('pointerup', up, { once: true });
    });
    layer.append(node);
  }
}

function floatTab(pane, tabId) {
  const tab = rightbarTabs[tabId];
  if (!tab) return;
  pane.tabs = pane.tabs.filter(id => id !== tabId);
  if (pane.activeId === tabId) pane.activeId = pane.tabs[0] ?? null;
  if (!pane.tabs.length) {
    const start = newTabRecord('start');
    rightbarTabs[start.id] = start;
    pane.tabs.push(start.id);
    pane.activeId = start.id;
  }
  rightbarFloats.push({ id: `float${rightbarSeq++}`, tabId, x: 160 + rightbarFloats.length * 24, y: 120 + rightbarFloats.length * 24, w: 380, h: 300, z: 1 });
  persistRightbar();
  renderRightbar();
  void selectBrowserTab(tab);
}

function dockFloat(float) {
  const pane = activePane();
  pane.tabs.push(float.tabId);
  pane.activeId = float.tabId;
  rightbarFloats = rightbarFloats.filter(item => item !== float);
  persistRightbar();
  renderRightbar();
  void selectBrowserTab(rightbarTabs[float.tabId]);
}

async function removeFloat(float) {
  const closing = rightbarTabs[float.tabId];
  if (closing?.kind === 'terminal') {
    try { await terminals.close(closing, rightbarSessionKey); }
    catch (error) { toast(error.message, { tone: 'warning' }); return; }
    if (rightbarTabs[float.tabId] !== closing) return;
  }
  closeNativeBrowserTab(closing);
  rightbarFloats = rightbarFloats.filter(item => item !== float);
  delete rightbarTabs[float.tabId];
  persistRightbar();
  renderRightbar();
}

function updateRightbarButtons() {
  for (const id of ['header-right-toggle', 'floating-right-toggle']) {
    const button = $(id);
    button.setAttribute('aria-expanded', String(rightbarOpen));
    button.dataset.tooltip = rightbarOpen ? '收起侧边栏' : '打开侧边栏';
    button.setAttribute('aria-label', rightbarOpen ? '收起右侧边栏' : '打开右侧边栏');
  }
  // The floating control exists only for the home surface with the dock
  // closed. Its hidden state must follow open/close immediately: otherwise
  // closing the dock leaves the pointer without any reopen control, and
  // opening it leaves a duplicate button floating over the dock.
  $('floating-right-toggle').hidden = !isHome() || !$('entry-page').hidden || rightbarOpen;
}

function renderTabView(tab, pane) {
  if (tab.kind === 'start') return startView();
  if (tab.kind === 'files') return filesView(tab);
  if (tab.kind === 'terminal') return terminalView(tab);
  if (tab.kind === 'browser') return browserView(tab, pane);
  if (tab.kind === 'changes') return changesView(tab);
  if (tab.kind === 'preview') return previewView(tab);
  if (tab.kind === 'deliverables') return deliverablesView();
  if (tab.kind === 'plan') return planView();
  if (tab.kind === 'subagent') return subagentView();
  if (tab.kind === 'schedule') return scheduleView();
  return startView();
}

function startView() {
  const guide = el('div', 'start-guide');
  const mark = el('span', 'start-guide-mark');
  mark.append(iconNode('compass', { size: 56, stroke: 1.1 }));
  const cards = el('div', 'start-cards');
  const card = (kind, icon, tone, title, description, shortcut, onClick, unavailable) => {
    const button = el('button', 'start-card');
    button.type = 'button';
    button.setAttribute('aria-keyshortcuts', shortcut === '⌘ P' ? 'Meta+P' : shortcut === '⌃ `' ? 'Control+`' : 'Meta+T');
    if (unavailable) {
      button.dataset.tooltip = `未接入：${unavailable}`;
      button.setAttribute('aria-description', `未接入：${unavailable}`);
    }
    const iconWrap = el('span', `start-card-icon ${tone}`);
    iconWrap.append(iconNode(icon, { size: 26 }));
    const text = el('span', 'start-card-text');
    text.append(el('span', 'start-card-title', title), el('span', 'start-card-desc', description));
    button.append(iconWrap, text, el('span', 'start-card-shortcut', shortcut));
    button.addEventListener('click', onClick);
    return button;
  };
  // Start is pinned: the cards open a tab beside it instead of consuming it.
  const launch = kind => openRightbarTab(kind);
  cards.append(card('files', 'folder', 'folder', '工作区文件', '浏览会话工作区的文件', '⌘ P', () => launch('files'), '桌面桥接没有目录读取接口'));
  cards.append(card('terminal', 'terminal', 'terminal', '新建终端', '在会话工作区运行命令', '⌃ `', () => launch('terminal')));
  cards.append(card('browser', 'globe', 'globe', '浏览器', '浏览网页', '⌘ T', () => launch('browser')));
  guide.append(mark, cards);
  return guide;
}

const fileTreeStates = new Map();

function filesStateFor(session) {
  const key = session.id;
  let state = fileTreeStates.get(key);
  if (!state) {
    state = {
      sessionId: key,
      root: session.repository ?? '',
      status: 'idle',
      entries: new Map(),
      expanded: new Set(),
      loading: new Set(),
      errors: new Map(),
    };
    fileTreeStates.set(key, state);
  }
  if (state.root !== (session.repository ?? '')) {
    state.root = session.repository ?? '';
    state.status = 'idle';
    state.entries.clear();
    state.errors.clear();
    state.loading.clear();
  }
  return state;
}

function filesRefresh(state) {
  state.entries.clear();
  state.errors.clear();
  state.loading.clear();
  state.status = 'idle';
  if (rightbarOpen) renderRightbar();
}

function filesLoad(state, rel = '', { force = false } = {}) {
  if (state.loading.has(rel)) return;
  if (!force && state.entries.has(rel)) return;
  state.loading.add(rel);
  state.errors.delete(rel);
  if (rel === '') state.status = 'loading';
  api.filesList({ sessionId: state.sessionId, path: rel }).then(result => {
    state.entries.set(rel, result);
    state.loading.delete(rel);
    if (rel === '') state.status = 'ready';
  }).catch(error => {
    state.loading.delete(rel);
    state.errors.set(rel, error?.message ?? '无法读取目录');
    if (rel === '') state.status = 'error';
  }).finally(() => {
    if (rightbarOpen && activePane()?.activeId && rightbarTabs[activePane().activeId]?.kind === 'files') renderRightbar();
  });
}

function filesRow(entry, depth) {
  const row = el('button', 'files-row');
  row.type = 'button';
  row.style.paddingLeft = `${12 + depth * 16}px`;
  row.dataset.kind = entry.kind;
  row.dataset.path = entry.path;
  if (entry.external) {
    row.disabled = true;
    row.title = '链接指向工作区外部，已拒绝访问';
    row.dataset.external = 'true';
  }
  row.append(el('span', 'files-chevron'), el('span', 'files-icon'));
  row.append(el('span', 'files-name', entry.name));
  if (entry.symlink) row.append(el('span', 'files-tag', '链接'));
  return row;
}

function filesTree(state, rel, depth, host, selectedPath = null) {
  const listing = state.entries.get(rel);
  if (!listing) {
    if (state.loading.has(rel)) {
      const loading = el('div', 'files-loading', '正在读取…');
      loading.style.paddingLeft = `${12 + depth * 16}px`;
      host.append(loading);
    }
    else if (state.errors.has(rel)) {
      const error = el('div', 'files-error');
      error.append(el('span', undefined, state.errors.get(rel)));
      const retry = el('button', 'button ghost small', '重试');
      retry.type = 'button';
      retry.addEventListener('click', () => filesLoad(state, rel, { force: true }));
      error.append(retry);
      host.append(error);
    }
    return;
  }
  if (!listing.entries.length) {
    const empty = el('div', 'files-empty-dir', '空目录');
    empty.style.paddingLeft = `${12 + depth * 16}px`;
    host.append(empty);
  }
  for (const entry of listing.entries) {
    const row = filesRow(entry, depth);
    const chevron = row.querySelector('.files-chevron');
    const icon = row.querySelector('.files-icon');
    if (entry.kind === 'directory') {
      const open = state.expanded.has(entry.path);
      chevron.append(iconNode(open ? 'chevronDown' : 'chevronRight', { size: 12 }));
      icon.append(iconNode(open ? 'folderOpen' : 'folder', { size: 16 }));
      row.setAttribute('aria-expanded', String(open));
      if (!entry.external) row.addEventListener('click', () => {
        if (state.expanded.has(entry.path)) state.expanded.delete(entry.path);
        else {
          state.expanded.add(entry.path);
          filesLoad(state, entry.path);
        }
        renderRightbar();
      });
      host.append(row);
      if (open && !entry.external) {
        const children = el('div', 'files-children');
        host.append(children);
        if (!state.entries.has(entry.path)) filesLoad(state, entry.path);
        filesTree(state, entry.path, depth + 1, children, selectedPath);
      }
    } else {
      chevron.hidden = true;
      icon.append(fileIcon(entry.path));
      row.classList.toggle('selected', entry.path === selectedPath);
      if (!entry.external) row.addEventListener('click', () => openPreviewForPath(entry.path, entry.name));
      host.append(row);
    }
  }
  if (listing.truncated) host.append(el('div', 'files-note', `目录条目过多，仅显示前 ${listing.entries.length} 项。`));
}

function filesView() {
  const view = el('div', 'files-view');
  const session = current?.session;
  const path = session?.repository ?? '';
  const toolbar = el('div', 'view-toolbar');
  const pathNode = el('div', 'toolbar-path');
  if (path) {
    const last = path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
    pathNode.append(document.createTextNode(path.slice(0, path.length - last.length)), el('strong', undefined, last));
  } else pathNode.textContent = '未选择工作区';
  pathNode.title = path;
  const refresh = iconButton('refresh', '刷新文件列表', { size: 15, tooltip: '刷新文件列表' });
  const open = el('div', 'open-in-app-pane');
  toolbar.append(pathNode, refresh, open);
  renderOpenInAppInto(open, path, 'directory');
  if (!session || !path) {
    refresh.disabled = true;
    const state = el('div', 'files-empty');
    state.append(el('div', undefined, '当前会话没有工作区。先选择一个本地文件夹。'));
    const choose = el('button', 'button secondary small', '选择工作区');
    choose.type = 'button';
    choose.addEventListener('click', () => $('workspace-picker').click());
    state.append(choose);
    view.append(toolbar, state);
    return view;
  }
  const tree = filesStateFor(session);
  refresh.addEventListener('click', () => filesRefresh(tree));
  if (tree.status === 'idle') filesLoad(tree, '');
  const body = el('div', 'files-body');
  if (tree.status === 'loading' || tree.status === 'idle') {
    body.append(el('div', 'files-loading', '正在读取目录…'));
  } else if (tree.status === 'error') {
    const error = el('div', 'files-error');
    error.append(el('span', undefined, tree.errors.get('') ?? '无法读取工作区'));
    const retry = el('button', 'button secondary small', '重试');
    retry.type = 'button';
    retry.addEventListener('click', () => filesRefresh(tree));
    error.append(retry);
    body.append(error);
  } else {
    filesTree(tree, '', 0, body);
  }
  view.append(toolbar, body);
  return view;
}

const terminals = new DesktopTerminals(api, () => { persistRightbar(); renderRightbar(); });
function terminalView(tab) {
  const view = terminals.view(tab, current?.session);
  persistRightbar();
  return view;
}

function browserTabData(tab) {
  if (!tab?.browserId) return null;
  return browserData.tabs.find(item => item.id === tab.browserId) ?? null;
}

async function ensureBrowserTab(tab) {
  if (!tab) return null;
  if (tab.browserId) {
    const known = browserTabData(tab);
    if (known) return known;
    try {
      browserData = await api.browserState();
      const refreshed = browserTabData(tab);
      if (refreshed) return refreshed;
    } catch { /* the native tab may have been closed; create a replacement */ }
    tab.browserId = null;
  }
  if (tab.creating) {
    for (let i = 0; i < 40 && tab.creating; i += 1) await new Promise(resolve => setTimeout(resolve, 50));
    return browserTabData(tab);
  }
  tab.creating = true;
  try {
    browserData = await api.browserCommand({ type: 'new' });
    tab.browserId = browserData.selectedId;
    updateBrowserTabTitle(tab);
    persistRightbar();
    return browserTabData(tab);
  } catch (error) {
    browserError = error.message;
    return null;
  } finally {
    tab.creating = false;
  }
}

function activeBrowserTab() {
  const tab = rightbarTabs[activePane().activeId];
  return tab?.kind === 'browser' ? tab : null;
}

function updateBrowserTabTitle(tab) {
  const data = browserTabData(tab);
  tab.title = data && data.url && data.url !== 'about:blank' ? browserChrome(data).title : '浏览器';
}

/** Drive one UI browser tab explicitly; the active pane is not necessarily its owner. */
async function browserCommand(type, extra = {}, tab = activeBrowserTab()) {
  browserError = null;
  try {
    if (type === 'new') {
      browserData = await api.browserCommand({ type: 'new', ...extra });
      if (tab) tab.browserId = browserData.selectedId;
    } else {
      const id = tab?.browserId ?? browserData.selectedId;
      if (!id) throw new Error('没有可用的浏览器标签');
      browserData = await api.browserCommand({ type, id, ...extra });
    }
    if (tab) updateBrowserTabTitle(tab);
    renderRightbar();
    return true;
  } catch (error) {
    browserError = error.message;
    if (tab && typeof extra.url === 'string') { tab.addressDraft = extra.url; tab.addressDirty = true; }
    renderRightbar();
    return false;
  }
}

/** The native surface can show only the selected browser tab. */
async function selectBrowserTab(tab) {
  if (tab?.kind !== 'browser' || !tab.browserId || tab.browserId === browserData.selectedId) return;
  try {
    browserData = await api.browserCommand({ type: 'select', id: tab.browserId });
    scheduleBrowserSurface();
  } catch (error) {
    browserError = error.message;
    renderRightbar();
  }
}

function closeNativeBrowserTab(tab) {
  if (tab?.kind !== 'browser' || !tab.browserId) return;
  api.browserCommand({ type: 'close', id: tab.browserId })
    .then(data => { browserData = data; scheduleBrowserSurface(); })
    .catch(error => { browserError = error.message; });
  tab.browserId = null;
}

function browserView(tab, pane = null) {
  if (!tab.browserId && !tab.creating) void ensureBrowserTab(tab).then(() => {
    // Creation re-renders the toolbar once. Do not replace an address field
    // the person is already typing into.
    if (rightbarOpen && !document.activeElement?.closest?.('.browser-address-wrap')) renderRightbar();
  });
  if (pane && rightbarActivePane === pane.id && tab.browserId && tab.browserId !== browserData.selectedId) {
    queueMicrotask(() => selectBrowserTab(tab));
  }
  const data = browserTabData(tab);
  const chrome = browserChrome(data);
  const view = el('div', 'browser-view');
  const toolbar = el('div', 'browser-toolbar');
  const back = iconButton('arrowLeft', '后退', { size: 15, tooltip: '后退' });
  back.disabled = !chrome.back;
  back.addEventListener('click', () => browserCommand('back', {}, tab));
  const forward = iconButton('arrowRight', '前进', { size: 15, tooltip: '前进' });
  forward.disabled = !chrome.forward;
  forward.addEventListener('click', () => browserCommand('forward', {}, tab));
  const reload = iconButton('refresh', '刷新', { size: 15, tooltip: '刷新' });
  reload.disabled = !chrome.reload;
  reload.addEventListener('click', () => browserCommand('reload', {}, tab));
  const addressWrap = el('div', 'browser-address-wrap');
  if (chrome.loading) addressWrap.append(el('span', 'browser-loading-dot'));
  const address = el('input');
  address.type = 'text';
  address.value = tab.addressDirty ? (tab.addressDraft ?? '') : (data?.url === 'about:blank' ? '' : data?.url ?? '');
  address.placeholder = '输入 HTTP(S) 地址';
  address.setAttribute('aria-label', '输入 HTTP(S) 地址');
  const navigateAddress = () => {
    const url = address.value.trim();
    if (!url) return;
    tab.addressDraft = undefined;
    tab.addressDirty = false;
    tab.navigating = true;
    // A fresh tab creates its native view on mount. Navigating immediately
    // must wait for that creation and reuse it; issuing `new` again would
    // leave an orphan about:blank tab and split the selection.
    const run = () => browserCommand(browserTabData(tab) ? 'navigate' : 'new', { url }, tab);
    const finish = tab.creating
      ? ensureBrowserTab(tab).then(() => { if (browserTabData(tab)) return run(); })
      : run();
    void Promise.resolve(finish).finally(() => { delete tab.navigating; });
  };
  address.addEventListener('input', () => { tab.addressDraft = address.value; tab.addressDirty = true; });
  address.addEventListener('keydown', event => {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    navigateAddress();
  });
  address.addEventListener('blur', () => {
    // A state-driven re-render replaces this input, which also fires blur.
    // That is not the person leaving the field; discarding the draft there
    // would overwrite what they are typing with the current page URL.
    if (!address.isConnected || tab.navigating) return;
    // Leaving the field without submitting restores the address of the page
    // that is actually loaded; the draft belongs only to the focused edit.
    tab.addressDraft = undefined;
    tab.addressDirty = false;
    const live = browserTabData(tab);
    address.value = live?.url && live.url !== 'about:blank' ? live.url : '';
  });
  const go = el('button', 'browser-go');
  go.type = 'button';
  go.setAttribute('aria-label', '前往');
  go.dataset.tooltip = '前往';
  go.append(iconNode('arrowRight', { size: 14 }));
  go.addEventListener('click', navigateAddress);
  addressWrap.append(address, go);
  const external = iconButton('external', '在系统浏览器中打开', { size: 15, tooltip: '在系统浏览器中打开' });
  external.disabled = !chrome.external;
  external.addEventListener('click', () => browserCommand('external', {}, tab));
  const sandbox = el('label', 'browser-sandbox-toggle');
  sandbox.title = '浏览器始终在沙箱中运行；当前载体不提供关闭沙箱';
  const sandboxInput = el('input');
  sandboxInput.type = 'checkbox';
  sandboxInput.checked = true;
  sandboxInput.disabled = true;
  sandboxInput.setAttribute('aria-label', '沙箱');
  sandbox.append(sandboxInput, el('span', 'switch-track'));
  toolbar.append(back, forward, reload, addressWrap, external, sandbox);
  const surface = el('div', 'browser-surface');
  surface.dataset.browserTab = tab.id;
  const empty = el('div', 'browser-empty');
  empty.append(el('h3', undefined, tab.creating ? '正在新建浏览器标签…' : '输入 HTTP(S) 地址开始浏览'));
  surface.append(empty);
  view.append(toolbar);
  if (browserError) {
    const error = el('div', 'browser-error');
    error.append(el('div', undefined, browserError));
    const edit = el('button', 'button secondary small', '修改地址');
    edit.type = 'button';
    edit.addEventListener('click', () => {
      const input = document.querySelector('.browser-address-wrap input');
      input?.focus();
      input?.select();
    });
    const dismiss = el('button', 'button secondary small', '关闭提示');
    dismiss.type = 'button';
    dismiss.addEventListener('click', () => { browserError = null; renderRightbar(); });
    error.append(edit, dismiss);
    view.append(error);
  }
  if (data?.error) {
    const error = el('div', 'browser-error');
    error.append(el('div', undefined, `网页无法打开：${data.error}`));
    const retry = el('button', 'button secondary small', '重试');
    retry.type = 'button';
    retry.addEventListener('click', () => browserCommand('reload'));
    error.append(retry);
    view.append(error);
  }
  if (data && data.url !== 'about:blank' && !data.error) empty.hidden = true;
  view.append(surface);
  requestAnimationFrame(scheduleBrowserSurface);
  return view;
}

function scheduleBrowserSurface() {
  if (!api.browserSurface) return;
  const candidates = [...document.querySelectorAll('.browser-surface')]
    .filter(node => node.getClientRects().length)
    .map(node => ({ node, tab: rightbarTabs[node.dataset.browserTab] }))
    .filter(item => item.tab && browserTabData(item.tab)?.id === browserData.selectedId);
  let active = candidates[0] ?? null;
  if (!active) {
    // A stale browser-state event can clear a tab's native id after the
    // manager already selected a replacement. One visible browser surface and
    // one selected native tab are the same view; re-bind instead of hiding it.
    const surfaces = [...document.querySelectorAll('.browser-surface')].filter(node => node.getClientRects().length);
    const selected = browserData?.tabs?.find(item => item.id === browserData.selectedId);
    if (surfaces.length === 1 && selected) {
      const tab = rightbarTabs[surfaces[0].dataset.browserTab];
      // Only repair a tab that still has a native id the latest state no
      // longer knows. A tab with no id is still creating its own view, and
      // adopting the current selection would steal another tab's page.
      if (tab?.kind === 'browser' && !tab.creating && tab.browserId && !browserTabData(tab)) { tab.browserId = selected.id; active = { node: surfaces[0], tab }; }
    }
  }
  const data = active?.tab ? browserTabData(active.tab) : null;
  const dialogsOpen = [...document.querySelectorAll('dialog')].some(dialog => dialog.open);
  const menusOpen = !!document.querySelector('.menu');
  const visible = !!active && !!data && data.url !== 'about:blank' && !data.error
    && !dialogsOpen && !menusOpen;
  const rect = active?.node.getBoundingClientRect();
  const bounds = visible && rect ? browserSurfaceBounds(rect, { width: innerWidth, height: innerHeight }) : null;
  api.browserSurface({ visible: !!bounds, bounds: bounds ?? { x: 0, y: 0, width: 1, height: 1 } }).catch(error => { browserError = error.message; });
}

function requestDiff() {
  if (!current?.session.repository || current.diffQuery?.status === 'pending' || loading || uncertain) return;
  const sessionId = current.session.id;
  const queryId = crypto.randomUUID();
  current = beginDiffQuery(current, queryId);
  renderRightbar();
  const timer = setTimeout(() => {
    if (current?.session.id !== sessionId) return;
    current = expireDiffQuery(current, queryId, '15 秒内未收到读取结果，变更结果未确认。可以重新读取。');
    renderRightbar();
  }, 15000);
  action(async () => { await deliver({ type: 'request_diff', session_id: sessionId, query_id: queryId }); }, {
    onError: message => {
      if (current?.session.id !== sessionId) return;
      current = expireDiffQuery(current, queryId, `无法确认读取结果：${message}`);
      clearTimeout(timer);
      renderRightbar();
    },
    clearGlobal: false,
  }).finally(() => clearTimeout(timer));
}

function parsePatch(patch) {
  const out = [];
  let oldLine = 0;
  let newLine = 0;
  for (const raw of String(patch ?? '').split('\n')) {
    if (raw.startsWith('diff ') || raw.startsWith('index ') || raw.startsWith('--- ') || raw.startsWith('+++ ')) {
      out.push({ kind: 'meta', text: raw, old: null, next: null }); continue;
    }
    if (raw.startsWith('@@')) {
      const match = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
      if (match) { oldLine = Number(match[1]); newLine = Number(match[2]); }
      out.push({ kind: 'hunk', text: raw, old: null, next: null }); continue;
    }
    if (raw.startsWith('+')) { out.push({ kind: 'added', text: raw.slice(1), old: null, next: newLine++ }); continue; }
    if (raw.startsWith('-')) { out.push({ kind: 'removed', text: raw.slice(1), old: oldLine++, next: null }); continue; }
    if (raw.startsWith('\\')) { out.push({ kind: 'meta', text: raw, old: null, next: null }); continue; }
    if (raw === '') continue;
    out.push({ kind: 'context', text: raw.slice(1), old: oldLine++, next: newLine++ });
  }
  return out;
}

function reviewLineNode(line, language = 'text') {
  const node = el('div', `review-line ${line.kind}`);
  const text = el('span', 'text');
  appendHighlighted(text, line.text, language);
  node.append(
    el('span', 'ln', line.old === null ? '' : String(line.old)),
    el('span', 'ln', line.next === null ? '' : String(line.next)),
    el('span', 'marker', line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ''),
    text,
  );
  return node;
}

function reviewSplit(lines, language = 'text') {
  const blocks = [];
  for (const line of lines) {
    if (line.kind === 'meta') continue;
    if (line.kind === 'hunk') { blocks.push({ hunk: line, lines: [] }); continue; }
    if (!blocks.length) blocks.push({ hunk: null, lines: [] });
    blocks.at(-1).lines.push(line);
  }
  const wrap = el('div', 'review-split-wrap');
  const sideRow = (line, side) => {
    const node = el('div', `review-line ${line ? line.kind : 'side-empty'}`);
    if (!line) {
      node.append(el('span', 'ln', ''), el('span', 'marker', ''), el('span', 'text', ''));
      return node;
    }
    const number = side === 'left' ? line.old : line.next;
    const text = el('span', 'text');
    appendHighlighted(text, line.text, language);
    node.append(
      el('span', 'ln', number === null ? '' : String(number)),
      el('span', 'marker', line.kind === 'added' ? '+' : line.kind === 'removed' ? '−' : ''),
      text,
    );
    return node;
  };
  for (const block of blocks) {
    if (block.hunk) {
      const header = el('div', 'review-hunk', block.hunk.text);
      wrap.append(header);
    }
    const pairs = [];
    const pending = [];
    const flush = () => { while (pending.length) pairs.push({ left: pending.shift(), right: null }); };
    for (const line of block.lines) {
      if (line.kind === 'context') { flush(); pairs.push({ left: line, right: line }); continue; }
      if (line.kind === 'removed') { pending.push(line); continue; }
      if (line.kind === 'added') {
        pairs.push(pending.length ? { left: pending.shift(), right: line } : { left: null, right: line });
      }
    }
    flush();
    const left = el('div', 'review-diff');
    const right = el('div', 'review-diff');
    for (const pair of pairs) {
      left.append(sideRow(pair.left, 'left'));
      right.append(sideRow(pair.right, 'right'));
    }
    const split = el('div', 'review-split');
    const leftHalf = el('div', 'review-half');
    const rightHalf = el('div', 'review-half');
    leftHalf.append(left);
    rightHalf.append(right);
    split.append(leftHalf, rightHalf);
    wrap.append(split);
  }
  return wrap;
}

function changesView(tab) {
  const confirmed = tab.source === 'confirmed';
  if (!confirmed && hasWorkspace() && !current.diff && !current.diffError && current.diffQuery?.status !== 'pending') setTimeout(requestDiff, 0);
  const view = el('div');
  view.style.display = 'flex'; view.style.flexDirection = 'column'; view.style.flex = '1'; view.style.minHeight = '0';
  const files = confirmed ? confirmedChangeFiles(tab.toolIds ?? [], current?.tools ?? []) : current?.diff?.files ?? [];
  if (!tab.selected || !files.some(file => file.path === tab.selected)) tab.selected = files[0]?.path ?? null;
  const selected = files.find(file => file.path === tab.selected);
  const mode = tab.mode ?? 'split';
  const head = el('div', 'review-head');
  const fileButton = el('button', 'review-file');
  fileButton.type = 'button';
  fileButton.append(iconNode('file', { size: 14 }), el('span', 'review-path', selected?.path ?? '选择文件'), iconNode('chevronDown', { size: 12 }));
  fileButton.addEventListener('click', () => openMenu({
    anchor: fileButton, width: 360,
    items: files.length
      ? files.map(file => ({ label: `${file.path}    +${file.added} −${file.removed}`, checked: file.path === tab.selected, onSelect: () => { tab.selected = file.path; renderRightbar(); } }))
      : [{ label: '没有变更文件', disabled: true }],
  }));
  head.append(fileButton);
  const counts = el('div', 'review-counts');
  if (selected) counts.append(el('span', 'add', `+${selected.added}`), el('span', 'del', `−${selected.removed}`));
  head.append(counts);
  const tools = el('div', 'review-tools');
  const copyPatch = iconButton('copy', '复制完整 diff', { size: 15, tooltip: '复制完整 diff' });
  copyPatch.disabled = !selected?.patch;
  copyPatch.addEventListener('click', () => { if (selected?.patch) void copyText(api, selected.patch); });
  const split = iconButton('columns', '切换左右对比', { size: 15, tooltip: '切换左右对比' });
  split.setAttribute('aria-pressed', String(mode === 'split'));
  split.addEventListener('click', () => { tab.mode = mode === 'split' ? 'unified' : 'split'; persistRightbar(); renderRightbar(); });
  const wrap = iconButton('wrap', '切换换行', { size: 15, tooltip: '切换换行' });
  wrap.setAttribute('aria-pressed', String(!!tab.wrap));
  wrap.addEventListener('click', () => { tab.wrap = !tab.wrap; renderRightbar(); });
  const open = iconButton('external', '打开完整文件', { size: 15, tooltip: '打开完整文件' });
  open.disabled = !selected;
  open.addEventListener('click', () => { if (selected) openPreviewForPath(selected.path); });
  const refresh = iconButton('refresh', '重新读取工作区变更', { size: 15, tooltip: '重新读取工作区变更' });
  refresh.disabled = confirmed || current?.diffQuery?.status === 'pending';
  refresh.addEventListener('click', requestDiff);
  const openApp = el('div', 'open-in-app-pane');
  tools.append(copyPatch, split, wrap, open, refresh, openApp);
  if (selected?.path) renderOpenInAppInto(openApp, selected.path, 'file');
  head.append(tools);
  view.append(head);
  view.append(el('div', 'review-scope', confirmed
    ? '运行时已确认的工具改动；这是修改发生时的历史证据，不随当前工作区变化。'
    : '当前工作区 Git 变更；历史轮次快照未提供，不能由当前状态冒充。'));
  const body = el('div', 'review-body');
  if (tab.wrap) body.classList.add('wrap');
  if (!confirmed && current?.diffQuery?.status === 'pending' && !current.diff) body.append(el('div', 'review-status', '正在读取变更…'));
  else if (!confirmed && current?.diffError) {
    const status = el('div', 'review-status error', current.diffError);
    const retry = el('button', 'button secondary small', '重新读取');
    retry.type = 'button';
    retry.addEventListener('click', requestDiff);
    status.append(retry);
    body.append(status);
  } else if (!selected) body.append(el('div', 'review-status',
    confirmed ? '运行时没有提供已确认的变更。' : hasWorkspace() ? '工作区没有变更' : '当前会话没有工作区'));
  else if (!selected.patch) body.append(el('div', 'review-status', '该快照未包含补丁正文。'));
  else {
    const lines = parsePatch(selected.patch);
    const language = previewLanguage(selected.path);
    const oneSided = !lines.some(line => line.kind === 'removed') || !lines.some(line => line.kind === 'added');
    if (mode === 'split' && !oneSided) {
      const splitView = reviewSplit(lines, language);
      body.append(splitView);
      requestAnimationFrame(() => {
        const halves = [...splitView.querySelectorAll('.review-half')];
        if (halves.length < 2) return;
        let syncing = false;
        for (const half of halves) half.addEventListener('scroll', () => {
          if (syncing) return;
          syncing = true;
          for (const other of halves) if (other !== half) other.scrollLeft = half.scrollLeft;
          syncing = false;
        });
      });
    } else {
      const diff = el('div', 'review-diff');
      const fragment = document.createDocumentFragment();
      for (const line of lines) fragment.append(reviewLineNode(line, language));
      diff.append(fragment);
      body.append(diff);
    }
  }
  view.append(body);
  return view;
}

const PREVIEW_LANGUAGES = {
  rs: 'rust', ts: 'typescript', tsx: 'typescript', js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript',
  json: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', py: 'python', go: 'go', sh: 'shell', bash: 'shell',
  css: 'css', html: 'html', htm: 'html', sql: 'sql',
};
const PREVIEW_LANGUAGE_LABELS = {
  rust: 'Rust', typescript: 'TypeScript', javascript: 'JavaScript', json: 'JSON', yaml: 'YAML', toml: 'TOML',
  python: 'Python', go: 'Go', shell: 'Shell', css: 'CSS', html: 'HTML', sql: 'SQL', text: '纯文本',
};

function previewLanguage(path) {
  const extension = String(path ?? '').split('.').pop()?.toLowerCase() ?? '';
  return PREVIEW_LANGUAGES[extension] ?? 'text';
}

function previewRenderers(path) {
  const extension = String(path ?? '').split('.').pop()?.toLowerCase() ?? '';
  const language = previewLanguage(path);
  if (['md', 'markdown'].includes(extension)) return [{ id: 'markdown', label: 'Markdown' }, { id: 'code', label: '原始内容' }];
  if (['png', 'jpg', 'jpeg', 'gif', 'webp'].includes(extension)) return [{ id: 'image', label: '图片' }];
  if (['bmp', 'ico', 'svg'].includes(extension)) return [{ id: 'image', label: '图片' }];
  if (extension === 'pdf') return [{ id: 'pdf', label: 'PDF' }];
  if (['csv', 'tsv'].includes(extension)) return [{ id: 'csv', label: '表格' }, { id: 'code', label: '原始内容' }];
  if (['xlsx', 'xls'].includes(extension)) return [{ id: 'excel', label: '表格' }];
  if (['doc', 'docx', 'ppt', 'pptx'].includes(extension)) return [{ id: 'office', label: 'Office' }];
  if (['html', 'htm'].includes(extension)) return [{ id: 'code', label: 'HTML' }, { id: 'text', label: '纯文本' }];
  return [{ id: 'code', label: PREVIEW_LANGUAGE_LABELS[language] ?? '代码' }, { id: 'text', label: '纯文本' }];
}

const CODE_KEYWORDS = {
  rust: ['as', 'async', 'await', 'break', 'const', 'continue', 'crate', 'dyn', 'else', 'enum', 'extern', 'false', 'fn', 'for', 'if', 'impl', 'in', 'let', 'loop', 'match', 'mod', 'move', 'mut', 'pub', 'ref', 'return', 'self', 'Self', 'static', 'struct', 'super', 'trait', 'true', 'type', 'unsafe', 'use', 'where', 'while'],
  typescript: ['abstract', 'as', 'async', 'await', 'break', 'case', 'catch', 'class', 'const', 'continue', 'declare', 'default', 'delete', 'do', 'else', 'enum', 'export', 'extends', 'false', 'finally', 'for', 'from', 'function', 'if', 'implements', 'import', 'in', 'instanceof', 'interface', 'let', 'new', 'null', 'private', 'protected', 'public', 'readonly', 'return', 'static', 'super', 'switch', 'this', 'throw', 'true', 'try', 'type', 'typeof', 'undefined', 'var', 'void', 'while', 'yield'],
  python: ['and', 'as', 'assert', 'async', 'await', 'break', 'class', 'continue', 'def', 'del', 'elif', 'else', 'except', 'False', 'finally', 'for', 'from', 'global', 'if', 'import', 'in', 'is', 'lambda', 'None', 'not', 'or', 'pass', 'raise', 'return', 'True', 'try', 'while', 'with', 'yield'],
  go: ['break', 'case', 'chan', 'const', 'continue', 'default', 'defer', 'else', 'fallthrough', 'for', 'func', 'go', 'goto', 'if', 'import', 'interface', 'map', 'package', 'range', 'return', 'select', 'struct', 'switch', 'type', 'var'],
  shell: ['case', 'do', 'done', 'elif', 'else', 'esac', 'fi', 'for', 'function', 'if', 'in', 'then', 'until', 'while'],
  sql: ['and', 'as', 'by', 'create', 'delete', 'from', 'group', 'having', 'insert', 'into', 'join', 'limit', 'not', 'null', 'on', 'or', 'order', 'select', 'set', 'table', 'update', 'values', 'where'],
  css: ['important', 'media', 'import', 'supports'],
  html: [],
  json: ['true', 'false', 'null'],
  yaml: ['true', 'false', 'null', 'yes', 'no'],
  toml: ['true', 'false'],
};

function appendHighlighted(parent, text, language = 'text') {
  if (!text) return parent;
  const keywords = new Set(CODE_KEYWORDS[language] ?? []);
  const hashComments = ['yaml', 'toml', 'python', 'shell'].includes(language);
  let index = 0;
  while (index < text.length) {
    const rest = text.slice(index);
    const string = /^(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`)/.exec(rest);
    if (string) {
      parent.append(el('span', 'tok-string', string[0]));
      index += string[0].length;
      continue;
    }
    if ((language !== 'html' && rest.startsWith('//')) || (hashComments && rest.startsWith('#')) || (language === 'sql' && rest.startsWith('--'))) {
      parent.append(el('span', 'tok-comment', rest));
      break;
    }
    const number = /^(?:0x[0-9a-fA-F]+|\d+(?:\.\d+)?)/.exec(rest);
    if (number) {
      parent.append(el('span', 'tok-number', number[0]));
      index += number[0].length;
      continue;
    }
    const word = /^[A-Za-z_$][\w$]*/.exec(rest);
    if (word) {
      const value = word[0];
      const after = text.slice(index + value.length);
      let cls = '';
      if (keywords.has(value)) cls = 'tok-keyword';
      else if (/^\s*\(/.test(after)) cls = 'tok-function';
      parent.append(cls ? el('span', cls, value) : document.createTextNode(value));
      index += value.length;
      continue;
    }
    const punctuation = /^[{}()\[\];,.:=+\-*/<>!&|?]+/.exec(rest);
    if (punctuation) {
      parent.append(el('span', 'tok-punct', punctuation[0]));
      index += punctuation[0].length;
      continue;
    }
    const char = text[index];
    parent.append(document.createTextNode(char));
    index += 1;
  }
  return parent;
}

function parseDelimited(content, delimiter) {
  const rows = [];
  let row = [];
  let field = '';
  let quoted = false;
  const text = String(content ?? '');
  for (let index = 0; index < text.length; index += 1) {
    const char = text[index];
    if (quoted) {
      if (char === '"' && text[index + 1] === '"') { field += '"'; index += 1; }
      else if (char === '"') quoted = false;
      else field += char;
    } else if (char === '"') quoted = true;
    else if (char === delimiter) { row.push(field); field = ''; }
    else if (char === '\n') {
      row.push(field); field = '';
      if (row.length > 1 || row[0] !== '') rows.push(row);
      row = [];
    } else if (char !== '\r') field += char;
    if (rows.length >= 500) break;
  }
  if (field !== '' || row.length) { row.push(field); if (row.length > 1 || row[0] !== '') rows.push(row); }
  return rows.slice(0, 500).map(cells => cells.slice(0, 50));
}

function previewCsvBody(content, path) {
  const delimiter = String(path ?? '').toLowerCase().endsWith('.tsv') ? '\t' : ',';
  const rows = parseDelimited(content, delimiter);
  const wrap = el('div', 'preview-csv');
  if (!rows.length) {
    wrap.append(el('div', 'review-status', '文件没有可显示的行'));
    return wrap;
  }
  const table = el('table', 'preview-csv-table');
  const head = el('thead');
  const headRow = el('tr');
  for (const cell of rows[0]) headRow.append(el('th', undefined, cell));
  head.append(headRow);
  const body = el('tbody');
  for (const row of rows.slice(1)) {
    const tr = el('tr');
    for (let index = 0; index < rows[0].length; index += 1) tr.append(el('td', undefined, row[index] ?? ''));
    body.append(tr);
  }
  table.append(head, body);
  wrap.append(table);
  if (rows.length >= 500) wrap.append(el('div', 'preview-truncated', '表格较大，仅显示前 500 行、50 列。'));
  return wrap;
}

function previewCodeBody(content, path) {
  const wrap = el('div', 'preview-code');
  const lines = String(content).split('\n');
  const banner = el('div', 'preview-code-banner');
  banner.append(el('span', undefined, String(path ?? '').split('.').pop()?.toLowerCase() || 'text'));
  const pre = el('pre', 'preview-code-pre');
  const code = el('code');
  for (const [index, line] of lines.entries()) {
    const row = el('span', 'preview-code-line');
    row.dataset.line = String(index + 1);
    const text = el('span', 'preview-line-text');
    appendHighlighted(text, line, previewLanguage(path));
    row.append(el('span', 'preview-line-number', String(index + 1)), text);
    code.append(row);
  }
  pre.append(code);
  wrap.append(banner, pre);
  return wrap;
}

function previewIsText(renderer) {
  return ['markdown', 'code', 'text', 'csv'].includes(renderer);
}

function previewIsImage(renderer) {
  return renderer === 'image';
}

async function loadPreview(tab, { force = false } = {}) {
  const sessionId = current?.session?.id;
  if (!tab || tab.kind !== 'preview' || !sessionId) return;
  if (!previewIsText(tab.renderer) && !previewIsImage(tab.renderer)) return;
  if (tab.loading) return;
  if (!force && tab.loadedPath === tab.path && (previewIsImage(tab.renderer) ? tab.image !== undefined : tab.content !== undefined)) return;
  tab.loading = true;
  tab.error = null;
  if (rightbarOpen) renderRightbar();
  try {
    if (previewIsImage(tab.renderer)) {
      const result = await api.filesImage({ sessionId, path: tab.path });
      tab.image = result.dataUrl;
      tab.imageMime = result.mime;
      tab.content = undefined;
    } else {
      const result = await api.filesRead({ sessionId, path: tab.path });
      tab.content = result.text;
      tab.truncated = result.truncated === true;
      tab.image = undefined;
    }
    tab.loadedPath = tab.path;
    tab.loading = false;
  } catch (error) {
    tab.loading = false;
    tab.error = error?.message ?? '无法读取文件';
  }
  if (rightbarOpen) renderRightbar();
}

function openPreviewForPath(path, title = null, target = {}) {
  if (!path) return null;
  const renderers = previewRenderers(path);
  const existing = Object.values(rightbarTabs).find(item => item.kind === 'preview' && item.path === path);
  const preferredRenderer = target.line && renderers.some(item => item.id === 'code') ? 'code' : existing?.renderer ?? renderers[0].id;
  const tab = openRightbarTab('preview', { path, title: title ?? String(path).split(/[\\/]/).pop() ?? '预览', renderer: preferredRenderer, renderers: existing?.renderers ?? renderers, content: existing?.content, image: existing?.image, error: existing?.error ?? null, loadedPath: existing?.loadedPath, truncated: existing?.truncated });
  tab.targetLine = Number(target.line) || null;
  tab.targetLineEnd = Number(target.lineEnd) || tab.targetLine;
  tab.targetKind = target.kind ?? null;
  // An explicit open is also the user's refresh action: re-read even when the
  // tab already holds content, so a changed file cannot show a stale cache.
  if (existing) tab.loadedPath = undefined;
  void loadPreview(tab);
  return tab;
}

function previewDiffFile(path) {
  const normalized = String(path ?? '').replace(/^\.\//, '');
  return (current?.diff?.files ?? []).find(file => file.path === normalized) ?? null;
}

function previewUnavailable(label, path) {
  const empty = el('div', 'right-empty');
  const mark = el('span', 'empty-icon'); mark.append(iconNode('file', { size: 48, stroke: 1.1 }));
  empty.append(mark, el('h3', undefined, `${label} 预览尚未接入`), el('p', undefined, '当前只读文件桥接未提供该格式的渲染能力，请用系统应用打开。'));
  const openHost = el('div', 'open-in-app-unpreviewable');
  empty.append(openHost);
  renderOpenInAppInto(openHost, path, 'file');
  return empty;
}

function previewView(tab) {
  const view = el('div', 'preview-view');
  const path = tab.path ?? '';
  const renderers = tab.renderers?.length ? tab.renderers : previewRenderers(path);
  const selected = renderers.find(item => item.id === tab.renderer) ?? renderers[0];
  const toolbar = el('div', 'preview-toolbar');
  const pathNode = el('div', 'preview-path');
  if (path) {
    const last = path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
    pathNode.append(document.createTextNode(path.slice(0, path.length - last.length)), el('span', 'preview-path-name', last));
  } else pathNode.textContent = '未选择文件';
  pathNode.title = path;
  const renderer = el('button', 'preview-renderer', selected.label);
  renderer.type = 'button';
  renderer.setAttribute('aria-label', '打开方式');
  renderer.title = selected.label;
  renderer.addEventListener('click', () => openMenu({
    anchor: renderer, width: 180, align: 'end',
    items: renderers.map(item => ({ label: item.label, checked: item.id === selected.id, onSelect: () => { tab.renderer = item.id; persistRightbar(); renderRightbar(); void loadPreview(tab); } })),
  }));
  const wrap = iconButton('wrap', '自动换行', { size: 15, tooltip: tab.wrap ? '关闭自动换行' : '自动换行' });
  wrap.setAttribute('aria-pressed', String(!!tab.wrap));
  wrap.hidden = !previewIsText(selected.id) || selected.id === 'csv';
  wrap.addEventListener('click', () => { tab.wrap = !tab.wrap; persistRightbar(); renderRightbar(); });
  const copy = iconButton('copy', '复制文件内容', { size: 15, tooltip: '复制文件内容' });
  copy.disabled = !previewIsText(selected.id) || tab.content === undefined;
  copy.addEventListener('click', async () => {
    if (tab.content === undefined) return;
    const ok = await copyText(api, tab.content, { quiet: true, showFailure: true });
    toast(ok ? '已复制文件内容' : '复制失败', { tone: ok ? 'success' : 'error', holdMs: 1600 });
  });
  const diffFile = previewDiffFile(path);
  const review = iconButton('diff', '查看本轮改动', { size: 15, tooltip: '查看本轮改动' });
  review.hidden = !diffFile;
  review.addEventListener('click', () => {
    const changes = openRightbarTab('changes');
    if (!changes) return;
    changes.selected = diffFile.path;
    persistRightbar();
    renderRightbar();
  });
  const reload = iconButton('refresh', '重新读取文件', { size: 15, tooltip: '重新读取文件' });
  reload.disabled = !previewIsText(selected.id) && !previewIsImage(selected.id);
  reload.addEventListener('click', () => void loadPreview(tab, { force: true }));
  const open = el('div', 'open-in-app-pane');
  toolbar.append(pathNode, renderer, wrap, copy, review, reload, open);
  renderOpenInAppInto(open, path, 'file');
  const body = el('div', `preview-body${tab.wrap ? ' wrap' : ''}`);

  const loadingState = () => {
    if (!tab.loading && !tab.error) void loadPreview(tab);
    if (tab.error) {
      const status = el('div', 'review-status error');
      status.append(el('span', undefined, tab.error));
      const retry = el('button', 'button secondary small', '重试');
      retry.type = 'button';
      retry.addEventListener('click', () => void loadPreview(tab, { force: true }));
      status.append(retry);
      return status;
    }
    return el('div', 'review-status', previewIsImage(selected.id) ? '正在读取图片…' : '正在读取文件…');
  };

  if (previewIsImage(selected.id)) {
    if (tab.loading || tab.image === undefined || tab.error) body.append(loadingState());
    else {
      const image = el('img', 'preview-image');
      image.src = tab.image;
      image.alt = path.split(/[\\/]/).filter(Boolean).at(-1) ?? '图片预览';
      image.addEventListener('error', () => {
        body.replaceChildren(el('div', 'review-status error', '图片无法解码或已损坏'));
      });
      const frame = el('div', 'preview-image-frame');
      frame.append(image, el('div', 'preview-image-meta', `${tab.imageMime ?? ''} · ${selected.label}`));
      body.append(frame);
    }
  } else if (!previewIsText(selected.id)) {
    body.append(previewUnavailable(selected.label, path));
  } else if (tab.loading || tab.content === undefined || tab.error) {
    body.append(loadingState());
  } else if (selected.id === 'markdown') {
    const markdown = el('div', 'markdown');
    renderMarkdown(markdown, tab.content, {
      onLink: openMessageLink,
      onImage: (placeholder, src) => {
        const reference = localFileReference(src);
        if (!reference || !current?.session?.id) return;
        api.filesImage({ sessionId: current.session.id, path: reference.path }).then(result => {
          const image = el('img', 'preview-md-image');
          image.src = result.dataUrl;
          image.alt = placeholder.dataset.alt ?? reference.path;
          placeholder.replaceWith(image);
        }).catch(() => {
          placeholder.textContent = `[图片无法加载：${placeholder.dataset.alt ?? reference.path}]`;
        });
      },
    });
    addCodeActions(markdown);
    body.append(markdown);
  } else if (selected.id === 'csv') body.append(previewCsvBody(tab.content, path));
  else if (selected.id === 'code') body.append(previewCodeBody(tab.content, path));
  else body.append(el('pre', undefined, tab.content));
  if (tab.truncated) body.append(el('div', 'preview-truncated', '文件超过 1 MiB，仅显示开头 1 MiB。'));

  if (tab.targetLine) {
    requestAnimationFrame(() => {
      const target = body.querySelector(`.preview-code-line[data-line="${tab.targetLine}"]`);
      if (!target) return;
      const end = Math.max(tab.targetLine, Math.min(tab.targetLineEnd ?? tab.targetLine, tab.targetLine + 200));
      for (let line = tab.targetLine; line <= end; line += 1) {
        body.querySelector(`.preview-code-line[data-line="${line}"]`)?.classList.add('target-line');
      }
      target.scrollIntoView({ block: 'center' });
    });
  }
  view.append(toolbar, body);
  return view;
}

function deliverablesView() {
  const report = current?.session?.completion_report ?? current?.completionReport;
  const presented = presentedFiles();
  if (!report && !presented.length) {
    const empty = el('div', 'right-empty');
    const mark = el('span', 'empty-icon'); mark.append(iconNode('archive', { size: 48, stroke: 1.1 }));
    empty.append(mark, el('h3', undefined, '暂无交付内容'), el('p', undefined, '当前 Runtime 不暴露交付物/产物列表，桌面不会伪造文件卡片。'));
    return empty;
  }
  const view = el('div', 'deliverable-list');
  if (current?.session.repository) {
    const toolbar = el('div', 'view-toolbar deliverable-toolbar');
    toolbar.append(el('div', 'toolbar-path', '工作区'));
    const open = el('div', 'open-in-app-pane');
    toolbar.append(open);
    renderOpenInAppInto(open, current.session.repository, 'directory');
    view.append(toolbar);
  }
  const card = el('div', 'deliverable-summary-card');
  const icon = el('span', 'delivered-icon');
  icon.append(iconNode('archive', { size: 20 }));
  const copy = el('span', 'delivered-copy');
  copy.append(
    el('span', 'delivered-name', '本轮交付'),
    el('span', 'delivered-meta', `${report.files_changed ?? 0} 个文件变更 · +${report.added ?? 0} −${report.removed ?? 0}`),
  );
  card.append(icon, copy);
  const note = el('div', 'deliverable-note');
  note.append(el('span', undefined, report
    ? 'Runtime 只提供变更聚合统计；以下文件来自本轮真实交付工具调用。'
    : '以下文件来自本轮真实交付工具调用；Runtime 未提供变更聚合。'));
  view.append(card, note);
  if (presented.length) {
    const list = el('div', 'presented-files-card');
    for (const file of presented) {
      const row = el('button', 'presented-file');
      row.type = 'button';
      const icon = el('span', 'presented-file-icon');
      icon.append(iconNode('fileText', { size: 18 }));
      const body = el('span', 'presented-file-body');
      body.append(el('span', 'presented-file-name', file.path), el('span', 'presented-file-status', { running: '正在交付', success: '已交付', failed: '交付失败' }[file.status] ?? '已请求打开'));
      row.append(icon, body);
      row.addEventListener('click', () => openPreviewForPath(file.path));
      list.append(row);
    }
    view.append(list);
  }
  return view;
}

function planView() {
  const steps = current?.plan?.steps ?? [];
  if (!steps.length) {
    const empty = el('div', 'right-empty');
    empty.append(el('h3', undefined, '暂无执行计划'), el('p', undefined, 'Agent 创建计划后，步骤会显示在这里。'));
    return empty;
  }
  const view = el('div', 'plan-view');
  const stateLabel = current?.plan?.state === 'completed' ? '（已完成）' : current?.plan?.state === 'interrupted' ? '（已中断）' : '';
  view.append(el('h2', 'plan-title', `计划${stateLabel}`));
  for (const step of steps) {
    const row = el('div', 'plan-step-row');
    row.dataset.planStatus = step.status;
    row.append(el('span', 'step-mark', { pending: '○', running: '●', done: '✓', failed: '✕', skipped: '–' }[step.status] ?? '?'));
    row.append(el('span', 'step-text', step.description), el('span', 'step-status', planStepLabel(step.status)));
    view.append(row);
  }
  return view;
}

function subagentView() {
  const children = current?.children ?? [];
  if (!children.length) {
    const empty = el('div', 'right-empty');
    empty.append(el('h3', undefined, '暂无子智能体'), el('p', undefined, 'Agent 委派子任务后，这里会显示它们的运行状态。'));
    return empty;
  }
  const view = el('div', 'subagent-list');
  for (const child of children) {
    const card = el('div', 'subagent-card');
    const name = child.nickname || child.title || child.id;
    const lineage = el('div', 'subagent-lineage');
    const root = el('button', 'subagent-lineage-crumb', '主会话');
    root.type = 'button';
    root.title = '回到主会话';
    root.addEventListener('click', () => { closeRightbar(); document.querySelector('#message')?.focus(); });
    lineage.append(root, iconNode('chevronRight', { size: 12 }), el('span', 'subagent-lineage-current', name));
    card.append(lineage);
    const head = el('button', 'subagent-head');
    head.type = 'button';
    head.setAttribute('aria-expanded', 'false');
    head.append(iconNode('bot', { size: 16 }), el('span', 'subagent-name', name));
    const state = childIsSettled(child) ? (child.ok === false ? '失败' : '已完成') : child.phase || '运行中';
    const badge = el('span', 'subagent-state', state);
    badge.dataset.done = String(childIsSettled(child));
    head.append(badge);
    head.append(iconNode('chevronDown', { size: 12, className: 'subagent-expand' }));
    card.append(head);
    const detail = child.phase || child.tool || child.preview || child.detail || child.scope || child.role;
    if (detail) card.append(el('div', 'subagent-detail', String(detail)));
    if (child.input_tokens || child.output_tokens) {
      card.append(el('div', 'subagent-usage', `输入 ${child.input_tokens ?? 0} · 输出 ${child.output_tokens ?? 0}`));
    }
    const extra = el('div', 'subagent-extra');
    if (child.purpose) extra.append(el('div', 'subagent-purpose', String(child.purpose)));
    const readonly = el('div', 'subagent-readonly');
    readonly.setAttribute('role', 'status');
    readonly.append(
      el('strong', undefined, '此子智能体只读'),
      el('span', undefined, '桌面端未提供子会话输入接口；可在这里查看执行状态。'),
    );
    extra.append(readonly);
    card.append(extra);
    head.addEventListener('click', () => {
      const open = card.classList.toggle('expanded');
      head.setAttribute('aria-expanded', String(open));
    });
    view.append(card);
  }
  return view;
}

function scheduleView() {
  const view = el('div', 'schedule-task-view');
  const empty = el('div', 'right-empty schedule-task-empty');
  empty.append(
    iconNode('clock', { size: 24 }),
    el('h3', undefined, '定时任务未接入'),
    el('p', undefined, 'Runtime 未向桌面提供定时任务目录与任务详情，无法显示规则或运行记录。'),
  );
  view.append(empty);
  return view;
}

/* ----------------------------------------------------------------- layout */

function layout() {
  const narrow = innerWidth < 1024;
  const sidebarOpen = narrow && document.body.classList.contains('sidebar-open');
  const sidebarCollapsed = narrow ? !sidebarOpen : wideSidebarCollapsed;
  document.body.classList.toggle('sidebar-collapsed', sidebarCollapsed);
  $('sidebar').inert = sidebarCollapsed;
  $('sidebar').setAttribute('aria-hidden', String(sidebarCollapsed));
  const sidebarWidth = sidebarCollapsed ? 0 : 280;
  $('leading-seat').hidden = !sidebarCollapsed;
  const mainWidth = $('main').clientWidth;
  const chatWidth = Math.round(Math.min(920, Math.max(320, Math.min(Math.max(680, mainWidth * 0.64), mainWidth - 32))));
  root.style.setProperty('--chat-width', `${chatWidth}px`);
  const available = innerWidth - sidebarWidth - 400;
  const overlay = innerWidth < 768 || available < 300;
  document.body.classList.toggle('rightbar-overlay', overlay && rightbarOpen && !rightbarExpanded);
  const preferredWidth = Math.max(300, rightbarWidth ?? Math.round(innerWidth * 0.45));
  const renderedWidth = overlay ? innerWidth : Math.max(300, Math.min(preferredWidth, Math.min(available, Math.round(innerWidth * 0.7))));
  root.style.setProperty('--right-width', `${overlay && rightbarOpen ? innerWidth : renderedWidth}px`);
  $('right-resize').hidden = !rightbarOpen || overlay || rightbarExpanded;
  $('rightbar').hidden = !rightbarOpen;
  scheduleBrowserSurface();
}

function closeNarrowSidebar() {
  if (innerWidth < 1024) document.body.classList.remove('sidebar-open');
  layout();
}

/* --------------------------------------------------------------- settings */

function openSettings(section = 'general') {
  settingsSection = section;
  renderSettings();
  if (!$('settings-dialog').open) $('settings-dialog').showModal();
  $('settings-nav-list').querySelector('[aria-current="page"]')?.focus();
  if (section === 'general' && updateState.phase === 'idle') checkForUpdate();
  scheduleBrowserSurface();
}
function closeSettings() {
  $('settings-dialog').close();
  $('settings-content').dataset.section = '';
  scheduleBrowserSurface();
}

function settingRow(label, desc, control, extra = null) {
  const row = el('div', 'setting-row');
  const copy = el('div', 'setting-copy');
  copy.append(el('div', 'setting-label', label));
  if (desc) copy.append(el('div', 'setting-desc', desc));
  row.append(copy);
  const wrap = el('div', 'setting-control');
  wrap.append(control);
  if (extra) wrap.append(extra);
  row.append(wrap);
  return row;
}

// Disabled controls keep the reference layout; the tooltip states why the
// capability is unavailable without adding a badge to every row.
function unavailableControl(control, reason) {
  const wrap = el('span', 'setting-unavailable');
  wrap.title = reason;
  control.title = reason;
  wrap.append(control);
  return wrap;
}

function menuControl(value, options, onChange, { disabled = false, label = '' } = {}) {
  const button = el('button', 'select-button');
  button.type = 'button';
  button.disabled = disabled;
  button.setAttribute('aria-label', label);
  const currentOption = options.find(option => option.value === value) ?? options[0];
  button.append(el('span', undefined, currentOption?.label ?? ''), iconNode('chevronDown', { size: 14, className: 'select-chevron' }));
  button.addEventListener('click', () => openMenu({
    anchor: button, align: 'end', width: 260,
    items: options.map(option => ({
      label: option.label, checked: option.value === value, disabled: option.disabled,
      badge: option.badge, onSelect: () => onChange(option.value),
    })),
  }));
  return button;
}

function switchControl(checked, onChange, { disabled = false } = {}) {
  const label = el('label', 'switch');
  const input = el('input');
  input.type = 'checkbox';
  input.checked = checked;
  input.disabled = disabled;
  input.addEventListener('change', () => onChange(input.checked));
  label.append(input, el('span', 'switch-track'));
  return label;
}

function settingsNav() {
  const nav = $('settings-nav-list');
  nav.replaceChildren();
  for (const [id, label, icon] of [
    ['general', '通用设置', 'settings'], ['models', '模型', 'layers'],
    ['plugins', '内置插件', 'plugins'], ['presets', 'Agent 预设', 'bot'],
  ]) {
    const button = el('button');
    button.type = 'button';
    button.setAttribute('aria-current', settingsSection === id ? 'page' : 'false');
    button.append(iconNode(icon, { size: 16 }), el('span', undefined, label));
    button.addEventListener('click', () => { settingsSection = id; renderSettings(); });
    nav.append(button);
  }
}

function renderSettings() {
  settingsNav();
  const content = $('settings-content');
  const sameSection = content.dataset.section === settingsSection;
  const priorScroll = content.scrollTop;
  content.replaceChildren();
  content.dataset.section = settingsSection;
  if (settingsSection === 'general') renderGeneralSettings(content);
  else if (settingsSection === 'models') renderModelSettings(content);
  else if (settingsSection === 'plugins') renderPluginSettings(content);
  else renderPresetSettings(content);
  content.scrollTop = sameSection ? priorScroll : 0;
}

function themeCards() {
  const group = el('div', 'theme-cards');
  group.setAttribute('role', 'radiogroup');
  group.setAttribute('aria-label', '外观');
  for (const [value, label, icon] of [['light', '浅色', 'sun'], ['dark', '深色', 'moon'], ['system', '跟随系统', 'monitor']]) {
    const button = el('button', 'theme-card');
    button.type = 'button';
    button.setAttribute('role', 'radio');
    button.setAttribute('aria-checked', String(settings.theme === value));
    button.append(iconNode(icon, { size: 16 }), el('span', undefined, label));
    button.addEventListener('click', () => {
      settings.theme = value;
      saveSettings();
      applyTheme();
      renderSettings();
    });
    group.append(button);
  }
  return group;
}

function fontSizeControl(value, min, max, onChange, { expander = false } = {}) {
  const wrap = el('div', 'font-size-control');
  const stepper = el('div', 'font-stepper');
  stepper.append(el('span', 'font-stepper-value', String(value)));
  const arrows = el('div', 'font-stepper-arrows');
  const up = el('button', 'font-stepper-arrow');
  up.type = 'button';
  up.setAttribute('aria-label', '增大字号');
  up.disabled = value >= max;
  up.append(iconNode('chevronUp', { size: 9 }));
  up.addEventListener('click', () => onChange(Math.min(max, value + 1)));
  const down = el('button', 'font-stepper-arrow');
  down.type = 'button';
  down.setAttribute('aria-label', '减小字号');
  down.disabled = value <= min;
  down.append(iconNode('chevronDown', { size: 9 }));
  down.addEventListener('click', () => onChange(Math.max(min, value - 1)));
  arrows.append(up, down);
  stepper.append(arrows);
  wrap.append(stepper, el('span', 'font-size-unit', 'px'));
  if (expander) {
    const toggle = iconButton('chevronDown', '更多字体设置', { size: 12, className: 'font-expander', tooltip: '更多字体设置' });
    toggle.setAttribute('aria-expanded', String(fontSettingsOpen));
    if (fontSettingsOpen) toggle.classList.add('open');
    toggle.addEventListener('click', () => { fontSettingsOpen = !fontSettingsOpen; renderSettings(); });
    wrap.append(toggle);
  }
  return wrap;
}

function fontFamilyControl(value, onChange, label) {
  const input = el('input', 'settings-input');
  input.value = value ?? '';
  input.placeholder = '默认';
  input.maxLength = 200;
  input.setAttribute('aria-label', label);
  let committed = value ?? '';
  const commit = () => {
    if (input.value === committed) return;
    committed = input.value;
    onChange(input.value);
  };
  input.addEventListener('change', commit);
  input.addEventListener('blur', commit);
  input.addEventListener('keydown', event => {
    if (event.key === 'Enter') { event.preventDefault(); commit(); input.blur(); }
  });
  return input;
}

let updateState = { phase: 'idle' };

function checkForUpdate() {
  if (updateState.phase === 'checking') return;
  updateState = { phase: 'checking' };
  if ($('settings-dialog').open) renderSettings();
  api.checkUpdate().then(result => {
    updateState = result.available ? { phase: 'available', ...result } : { phase: 'current', ...result };
    if ($('settings-dialog').open) renderSettings();
  }).catch(() => {
    updateState = { phase: 'failed' };
    if ($('settings-dialog').open) renderSettings();
  });
}

function updateControl() {
  const wrap = el('div', 'desktop-update-indicator');
  if (updateState.phase === 'checking') {
    wrap.append(el('span', 'desktop-update-spinner'), el('span', 'desktop-update-label', '正在检查更新…'));
    return wrap;
  }
  if (updateState.phase === 'available') {
    wrap.append(iconNode('download', { size: 14 }), el('span', 'desktop-update-label', `新版本：v${updateState.latest}`));
    const view = el('button', 'button ghost small', '查看');
    view.type = 'button';
    view.title = `当前 v${updateState.current}；安装请运行 leveler update`;
    view.addEventListener('click', () => action(() => api.openExternal('https://github.com/dengmengmian/CodeLeveler/releases')));
    wrap.append(view);
    return wrap;
  }
  if (updateState.phase === 'current') {
    wrap.append(iconNode('check', { size: 14 }), el('span', 'desktop-update-label', `已是最新 · v${updateState.latest}`));
    const again = el('button', 'button ghost small', '重新检查');
    again.type = 'button';
    again.addEventListener('click', checkForUpdate);
    wrap.append(again);
    return wrap;
  }
  if (updateState.phase === 'failed') {
    wrap.append(el('span', 'desktop-update-dot'), el('span', 'desktop-update-label', '检查更新失败'));
    const retry = el('button', 'button ghost small', '重试');
    retry.type = 'button';
    retry.addEventListener('click', checkForUpdate);
    wrap.append(retry);
    return wrap;
  }
  const check = el('button', 'button ghost small', '检查更新');
  check.type = 'button';
  check.addEventListener('click', checkForUpdate);
  wrap.append(check);
  return wrap;
}

function renderGeneralSettings(content) {
  const group = el('div', 'settings-group');
  group.append(settingRow('权限', '选择新会话的默认权限模式', menuControl(settings.defaultPermission, PERMISSION_MODES.map(([value, label]) => ({ value, label })), value => {
    settings.defaultPermission = value; saveSettings(); toast('默认权限已更新', { tone: 'success', holdMs: 1600 });
  }, { label: '权限' })));
  group.append(settingRow('语言', '当前仅提供中文；语言切换未接入', menuControl('zh-CN', [{ value: 'zh-CN', label: '中文' }], () => {}, { label: '语言' })));
  const themeRow = el('div', 'setting-row appearance-row');
  const themeCopy = el('div', 'setting-copy');
  themeCopy.append(el('div', 'setting-label', '外观'));
  themeRow.append(themeCopy);
  group.append(themeRow, themeCards());
  group.append(settingRow('字号大小', '仅影响会话内容的字号', fontSizeControl(settings.fontSize, 10, 22, value => {
    settings.fontSize = value; saveSettings(); applyContentFont(); renderSettings();
  }, { expander: true })));
  if (fontSettingsOpen) {
    group.append(settingRow('文本字体', '用逗号分隔多个字体；留空使用默认字体', fontFamilyControl(settings.textFontFamily, value => {
      settings.textFontFamily = value; saveSettings(); applyContentFont();
    }, '文本字体')));
    group.append(settingRow('代码字体', '用于代码块、行内代码和工具输出', fontFamilyControl(settings.codeFontFamily, value => {
      settings.codeFontFamily = value; saveSettings(); applyContentFont();
    }, '代码字体')));
    group.append(settingRow('代码字号', '仅影响代码块、行内代码和工具输出', fontSizeControl(settings.codeFontSize, 10, 16, value => {
      settings.codeFontSize = value; saveSettings(); applyContentFont(); renderSettings();
    })));
    group.append(settingRow('终端字体', '用于侧边栏终端', fontFamilyControl(settings.terminalFontFamily, value => {
      settings.terminalFontFamily = value; saveSettings(); applyContentFont();
    }, '终端字体')));
    group.append(settingRow('终端字号', '仅影响侧边栏终端', fontSizeControl(settings.terminalFontSize, 10, 20, value => {
      settings.terminalFontSize = value; saveSettings(); applyContentFont(); renderSettings();
    })));
  }
  const viewRow = settingRow('显示代码工作视图', '开启后，显示轨迹、本轮代码差异，可选择完整的 Agent 预设切换',
    unavailableControl(switchControl(settings.codingView, () => {}, { disabled: true }), '当前版本不可用：没有本轮代码差异视图，无法完整切换'));
  viewRow.classList.add('setting-row-compact');
  group.append(viewRow);
  const shortcuts = el('button', 'settings-shortcut-button', '编辑快捷键');
  shortcuts.type = 'button';
  shortcuts.addEventListener('click', () => showShortcuts());
  group.append(settingRow('快捷键', '查看和编辑当前可用的快捷键和输入操作', shortcuts));
  group.append(settingRow('网页链接默认打开方式', '对话中网页链接的打开位置', menuControl(settings.linkOpen, [
    { value: 'side', label: '应用内侧边栏' }, { value: 'system', label: '默认浏览器' },
  ], value => { settings.linkOpen = value; saveSettings(); }, { label: '网页链接默认打开方式' })));
  group.append(settingRow('繁忙时的发送行为', '智能体运行时 Enter 键和发送按钮的行为；Cmd/Ctrl+Enter 使用另一行为', menuControl(settings.busySend, [
    { value: 'queue', label: '排队发送' }, { value: 'steer', label: '插话发送' },
  ], value => { settings.busySend = value; saveSettings(); renderComposerMeta(); controls(); }, { label: '繁忙时的发送行为' })));
  group.append(settingRow('性能与用量', '选择性能与用量信息展示的详细程度', menuControl(settings.performance, [
    { value: 'detailed', label: '详细' }, { value: 'compact', label: '简洁' },
  ], value => { settings.performance = value; saveSettings(); renderConversation(); renderContextMeter(); }, { label: '性能与用量' })));
  const sessionLogRow = settingRow('在使用官方模型 API 时上传 Session Log', '帮助改进 DeepSeek 模型与产品',
    unavailableControl(switchControl(false, () => {}, { disabled: true }), '当前版本不可用：未接入 Session Log 上传接口'));
  sessionLogRow.classList.add('setting-row-compact');
  group.append(sessionLogRow);
  const updateRow = settingRow('软件更新', '检查 GitHub Releases 上是否有新版本；安装由 leveler update 完成', updateControl());
  updateRow.classList.add('setting-row-compact');
  group.append(updateRow);
  content.append(group);
  content.append(el('div', 'settings-footer', `当前版本：${$('brand-version').textContent || 'CodeLeveler'}`));
}

function showShortcuts() {
  openMenu({
    anchor: $('settings-close'), align: 'end', width: 320,
    items: [
      { heading: '快捷键' },
      { label: '新建会话', shortcut: '⌘ N', disabled: true },
      { label: '打开设置', shortcut: '⌘ ,', disabled: true },
      { label: '搜索当前对话', shortcut: '⌘ F', disabled: true },
      { label: '切换右侧边栏', shortcut: '⌥ ⌘ B', disabled: true },
      { label: '新建浏览器标签', shortcut: '⌘ T', disabled: true },
      { heading: '输入' },
      { label: '发送', shortcut: 'Enter', disabled: true },
      { label: '换行', shortcut: 'Shift Enter', disabled: true },
      { label: '命令菜单', shortcut: '/', disabled: true },
    ],
  });
}

function renderModelSettings(content) {
  content.append(el('h2', undefined, '模型'));
  content.append(el('p', 'settings-note', '填入各提供商的 API 密钥即可使用其模型。CodeLeveler 的 Provider 配置由 Runtime 管理，桌面只展示当前会话可用的模型。'));
  const models = current?.session.available_models ?? [];
  if (!current) content.append(el('div', 'settings-note', '请先打开或新建一个会话，再选择模型。'));
  else if (!models.length) {
    const card = el('div', 'settings-card');
    card.append(el('h3', undefined, '暂无可用模型'), el('p', undefined, 'Runtime 没有提供可用的模型。请先在 CodeLeveler 配置中启用 Provider。'), unconnected());
    content.append(card);
  } else {
    const grouped = new Map();
    for (const model of models) {
      if (!grouped.has(model.provider)) grouped.set(model.provider, []);
      grouped.get(model.provider).push(model);
    }
    for (const [provider, list] of grouped) {
      const card = el('div', 'settings-card');
      const head = el('div', 'provider-row');
      head.append(el('span', 'provider-name', provider));
      const actions = el('div', 'provider-actions');
      for (const label of ['编辑', '删除']) {
        const button = el('button', 'button ghost small', label);
        button.type = 'button';
        button.disabled = true;
        button.title = 'Runtime 未提供 Provider 编辑接口';
        actions.append(button);
      }
      head.append(actions);
      card.append(head);
      for (const model of list) {
        const row = el('div', 'provider-row');
        const selected = current.session.model?.provider === model.provider && current.session.model?.model === model.model;
        row.append(el('span', 'provider-badge', selected ? '当前' : '可选'), el('span', 'provider-name', model.model));
        const use = el('button', 'button secondary small', selected ? '已选择' : '使用');
        use.type = 'button'; use.disabled = selected || loading;
        use.addEventListener('click', () => action(async () => {
          await deliver({ type: 'select_model', session_id: current.session.id, model });
          await refreshMetadata();
          renderSettings();
        }));
        row.append(use);
        card.append(row);
      }
      content.append(card);
    }
  }
  const card = el('div', 'settings-card');
  card.append(el('h3', undefined, '添加模型提供商'), el('p', undefined, 'Provider、密钥与 Base URL 的写入接口尚未接入桌面。'));
  const add = el('button', 'button secondary small', '添加 Provider');
  add.type = 'button';
  add.disabled = true;
  add.title = 'Runtime 未提供 Provider 写入接口';
  card.append(add);
  card.append(el('p', 'settings-note', '自定义 Base URL、协议与模型行同样未接入；当前只读展示 Runtime 已加载的模型。'));
  card.append(unconnected());
  content.append(card);
}

function renderPluginSettings(content) {
  content.append(el('h2', 'settings-title-large', '内置插件'));
  const toolbar = el('div', 'plugin-toolbar');
  const search = el('input', 'settings-input plugin-search');
  search.type = 'search';
  search.placeholder = '搜索插件';
  search.disabled = true;
  search.title = '当前 Runtime 未提供插件目录';
  search.setAttribute('aria-label', '搜索插件');
  const filter = el('button', 'button ghost small', '全部');
  filter.type = 'button';
  filter.disabled = true;
  filter.title = '当前 Runtime 未提供插件分类';
  toolbar.append(search, filter);
  content.append(toolbar);
  const card = el('div', 'settings-card');
  card.append(el('h3', undefined, '当前 Runtime 未提供插件目录'));
  card.append(el('p', undefined, 'CodeLeveler 的工具与能力由 Runtime 内置，桌面桥接层没有可读取的插件清单或启停接口。这里不会展示伪造的插件开关。'));
  card.append(unconnected());
  content.append(card);
}

function renderPresetSettings(content) {
  content.append(el('h2', 'settings-title-large', 'Agent 预设'));
  content.append(el('p', 'settings-note', '选择 Agent 的工具和工作方式。日常任务用「标准模式」。'));
  const standard = el('div', 'preset-card');
  const head = el('div', 'preset-head');
  head.append(el('span', 'preset-name', '标准模式'), el('span', 'preset-tag', '当前'));
  standard.append(head, el('div', 'preset-desc', '处理代码、文件和资料，适合大多数任务。Agent 会按需使用检索、编辑和终端等工具。'));
  content.append(standard);
  if (!current) { content.append(el('div', 'settings-note', '打开会话后可读取 Runtime 的 Agent 定义。')); return; }
  if (!agentsRead || agentsRead.owner !== current.session.id) {
    agentsRead = { owner: current.session.id, status: 'pending', data: null, error: null, id: crypto.randomUUID() };
    api.deliver(commandEnvelope(current.session.id, { type: 'list_agents', session_id: current.session.id, query_id: agentsRead.id }, current.session.last_sequence ?? 0))
      .then(result => { if (!result.ok) throw new Error(result.error.message); })
      .catch(error => {
        if (agentsRead?.id) { agentsRead = { ...agentsRead, status: 'unknown', error: error.message }; renderSettings(); }
      });
  }
  if (agentsRead.status === 'pending') content.append(el('div', 'settings-note', '正在读取 Agent 定义…'));
  if (agentsRead.status === 'unknown') content.append(el('div', 'settings-note', agentsRead.error ?? '读取失败'));
  for (const agent of agentsRead.data?.agents ?? []) {
    const card = el('div', 'preset-card');
    const row = el('div', 'preset-head');
    row.append(el('span', 'preset-name', agent.name), el('span', 'preset-tag', agent.source ?? 'runtime'));
    card.append(row, el('div', 'preset-desc', agent.description || '没有描述'));
    const tags = el('div', 'preset-tags');
    tags.append(el('span', 'preset-tag', agent.status ?? 'unknown'));
    const view = el('button', 'button ghost small', '查看配置');
    view.type = 'button';
    view.addEventListener('click', () => openPresetDialog(agent, 'config'));
    const usage = el('button', 'button ghost small', '如何使用');
    usage.type = 'button';
    usage.addEventListener('click', () => openPresetDialog(agent, 'usage'));
    const makeDefault = el('button', 'button ghost small', '设为默认');
    makeDefault.type = 'button';
    makeDefault.disabled = true;
    makeDefault.title = 'Runtime 未提供预设默认切换接口';
    tags.append(view, usage, makeDefault);
    card.append(tags);
    content.append(card);
  }
  for (const problem of agentsRead.data?.problems ?? []) content.append(el('div', 'settings-note', `${problem.source}: ${problem.error}`));
  const create = el('button', 'button secondary', '让 Agent 帮我创建预设模式');
  create.type = 'button';
  create.disabled = true;
  create.title = '当前桌面桥接未提供预设写入接口';
  content.append(create);
}

function openPresetDialog(agent, mode) {
  const dialog = el('dialog', 'modal preset-dialog');
  const card = el('div', 'modal-card');
  const head = el('div', 'modal-head');
  head.append(el('h2', undefined, `${agent.name} · ${mode === 'config' ? '配置' : '如何使用'}`));
  const close = iconButton('close', '关闭', { size: 16, tooltip: '关闭' });
  head.append(close);
  const body = el('div', 'preset-dialog-body');
  if (mode === 'config') {
    const safe = {
      name: agent.name, source: agent.source, capability: agent.capability,
      model: agent.model ?? null, reasoning_effort: agent.reasoning_effort ?? null,
      skills: agent.skills ?? [], fingerprint: agent.fingerprint,
    };
    body.append(el('pre', 'preset-config', JSON.stringify(safe, null, 2)));
  } else {
    body.append(el('p', undefined, agent.description || '没有描述'));
    body.append(el('p', undefined, '该预设由 Runtime 提供；当前桌面只读展示其定义，切换能力尚未接入。'));
  }
  card.append(head, body);
  dialog.append(card);
  document.body.append(dialog);
  close.addEventListener('click', () => dialog.close());
  dialog.addEventListener('close', () => dialog.remove(), { once: true });
  dialog.showModal();
}

/* -------------------------------------------------------------- entry pages */

function openPage(kind) {
  if (innerWidth < 1024) document.body.classList.remove('sidebar-open');
  if (entryPage === kind) { closePage(); return; }
  entryPage = kind;
  $('entry-page').hidden = false;
  renderEntryPage();
  renderHeader();
  layout();
}
function closePage() {
  entryPage = null;
  $('entry-page').hidden = true;
  renderHeader();
  render();
}

function renderEntryPage() {
  const host = $('entry-page');
  if (!entryPage) { host.hidden = true; return; }
  host.hidden = false;
  for (const button of document.querySelectorAll('.panel-row')) button.setAttribute('aria-current', button.dataset.panel === entryPage ? 'page' : 'false');
  const page = el('div', 'entry-inner');
  const head = el('div', 'entry-head');
  const copy = el('div');
  if (entryPage === 'plugins') {
    copy.append(el('h1', undefined, '插件'), el('p', undefined, '扩展 CodeLeveler 的工具与界面。'));
    const action = el('button', 'button secondary', '安装插件');
    action.type = 'button'; action.disabled = true; action.title = '当前版本未接入插件安装';
    const caret = el('button', 'button secondary entry-split-caret');
    caret.type = 'button'; caret.disabled = true; caret.title = '当前版本未接入插件安装';
    caret.setAttribute('aria-label', '插件安装选项');
    caret.append(iconNode('chevronDown', { size: 12 }));
    const split = el('div', 'entry-split'); split.append(action, caret);
    const wrap = el('div', 'settings-actions'); wrap.append(split);
    head.append(copy, wrap);
    page.append(head);
    const search = el('div', 'entry-search');
    search.append(iconNode('search', { size: 15 }));
    const input = el('input');
    input.placeholder = '搜索插件';
    input.setAttribute('aria-label', '搜索插件');
    search.append(input);
    page.append(search);
    const tabs = el('div', 'entry-tabs');
    for (const [label, selected] of [['内置', true], ['插件市场', false], ['设置', false]]) {
      const tab = el('button', 'entry-tab', label);
      tab.type = 'button';
      tab.setAttribute('aria-selected', String(selected));
      tabs.append(tab);
    }
    page.append(tabs);
    const card = el('div', 'entry-card');
    card.append(el('h2', undefined, '当前 Runtime 未提供插件目录'), el('p', undefined, '桌面桥接层没有可读取的插件清单、启停或安装接口。这里不会伪造插件列表或开关。'));
    page.append(card);
  } else {
    copy.append(el('h1', undefined, '自动化任务'), el('p', undefined, '按计划运行 Agent 任务。'));
    const action = el('button', 'button secondary', '新建任务');
    action.type = 'button'; action.disabled = true; action.title = '当前版本未接入调度接口';
    const wrap = el('div', 'settings-actions'); wrap.append(action);
    head.append(copy, wrap);
    page.append(head);
    const search = el('div', 'entry-search');
    search.append(iconNode('search', { size: 15 }));
    const input = el('input');
    input.placeholder = '搜索自动化任务';
    input.disabled = true;
    input.title = '当前版本未接入调度接口';
    input.setAttribute('aria-label', '搜索自动化任务');
    search.append(input);
    page.append(search);
    const tabs = el('div', 'entry-tabs');
    for (const [label, selected] of [['全部', true], ['已启用', false], ['已结束', false]]) {
      const tab = el('button', 'entry-tab', label);
      tab.type = 'button';
      tab.disabled = true;
      tab.title = '当前版本未接入调度接口';
      tab.setAttribute('aria-selected', String(selected));
      tabs.append(tab);
    }
    page.append(tabs);
    const card = el('div', 'entry-card');
    card.append(el('h2', undefined, '调度接口尚未接入'), el('p', undefined, 'CodeLeveler Runtime 暴露了 schedule 工具与事件，但桌面桥接层没有列表/创建/取消接口。这里不会展示伪造的计划记录。'));
    page.append(card);
    const empty = el('div', 'entry-empty');
    const mark = el('span', 'empty-mark');
    mark.append(iconNode('automation', { size: 44, stroke: 1.1 }));
    empty.append(mark, document.createTextNode('还没有自动化任务，在会话中创建的任务会显示在这里'));
    page.append(empty);
  }
  host.replaceChildren(page);
}

/* ------------------------------------------------------------ event stream */

function consumeSettingsEvent(frame) {
  if (frame.event !== 'runtime') return;
  if (frame.data?.type === 'agents_loaded' && agentsRead && frame.data.query_id === agentsRead.id) {
    agentsRead = { ...agentsRead, status: 'confirmed', data: frame.data };
    if ($('settings-dialog').open && settingsSection === 'presets') renderSettings();
  }
}

api.onEvent(frame => {
  consumeSettingsEvent(frame);
  if (frame.event === 'connection') { applyConnection(frame); return; }
  if (frame.event === 'restored') {
    // The Main process reopened the confirmed session after a bridge loss.
    // Apply the fresh snapshot; history stays authoritative and is reloaded.
    const result = frame.data ?? {};
    const session = result.session;
    if (!session?.id) return;
    currentSource = result.source_id ?? currentSource;
    history = [];
    current = attachExtras(projectSnapshot(session, history), session);
    lastEventSeq = session.last_sequence ?? 0;
    activeView = 'chat';
    liveTrajectory = [];
    liveTurnStartAt = null;
    liveTurnClosed = false;
    trajectorySelectedId = null;
    trajectoryNodeLimit = 50;
    trajectoryRange = null;
    trajectoryViewport = null;
    $('message').value = drafts.get(session.id) ?? $('message').value;
    render();
    applyConnection({ event: 'connection', state: 'connected', resumed: true });
    reloadHistory();
    refreshTasks();
    return;
  }
  if (attachmentEarly && frame.session_id === attachmentEarly.sessionId && frame.event === 'runtime'
    && ['attachment_added', 'attachment_processing_failed'].includes(frame.data?.type)
    && frame.data.command_id && !attachmentEarly.events.some(event => event.command_id === frame.data.command_id)) {
    if (attachmentEarly.events.length < 16) attachmentEarly.events.push(frame.data);
  }
  if (frame.event === 'error') {
    if (current?.diffQuery?.status === 'pending') {
      current = expireDiffQuery(current, current.diffQuery.id, 'Runtime 连接中断，变更结果未确认');
      renderRightbar();
    }
    showError(frame.error.message);
    return;
  }
  if (frame.event === 'handoff') return;
  if (frame.session_id === current?.session.id && frame.event === 'runtime'
    && ['attachment_added', 'attachment_processing_failed'].includes(frame.data?.type)) {
    const previous = attachments();
    const next = applyUploadEvent(previous, frame.data);
    if (next !== previous) {
      attachmentDrafts.set(draftKey(), next);
      uploadingKey = null;
      renderAttachments();
    }
  }
  if (frame.session_id !== current?.session.id) return;
  if (frame.event === 'snapshot') {
    if (staleSequence(frame.data.last_sequence)) return;
    lastEventSeq = Math.max(lastEventSeq, frame.data.last_sequence ?? 0);
    const prior = current;
    const diffQuery = current?.diffQuery;
    const diffError = current?.diffError;
    const priorContext = current?.contextUsage ?? null;
    const settledBackgroundTasks = current?.settledBackgroundTasks ?? [];
    const priorBackgroundTasks = current?.backgroundTasks ?? [];
    const replayed = projectSnapshot(frame.data, history);
    current = attachExtras(mergeRestoredConversation(replayed, prior), frame.data);
    current.session = replayed.session;
    current.status = replayed.status;
    current.plan = replayed.plan ?? current.plan;
    current.approvals = replayed.approvals;
    current.clarifications = replayed.clarifications;
    current.settledBackgroundTasks = settledBackgroundTasks;
    const activeJobs = (frame.data.active_background_tasks ?? []).map(job => activeJobRecord(job, priorBackgroundTasks));
    const activeIds = new Set(activeJobs.map(job => job.task_id));
    current.backgroundTasks = [...activeJobs, ...priorBackgroundTasks.filter(job => !activeIds.has(job.task_id))];
    current.contextUsage = priorContext;
    current.diffQuery = diffQuery;
    if (!frame.data.diff) current.diffError = diffError;
    render();
    return;
  }
  if (frame.event !== 'runtime') return;
  const event = frame.data;
  recordLiveTrajectory(event);
  if (typeof event.seq === 'number') lastEventSeq = Math.max(lastEventSeq, event.seq);
  if (event.type === 'session_history_loaded') {
    history = event.entries ?? [];
    liveTrajectory = [];
    const replayed = projectSnapshot(current.session, history);
    current = retainInterruptedThoughts(mergeRestoredConversation(replayed, current), current);
    current.plan = replayed.plan ?? current.plan;
    renderConversation();
    if (activeView === 'trajectory') renderTrajectory();
    return;
  }
  if (event.type === 'session_opened') {
    if (staleSequence(event.session.last_sequence)) return;
    lastEventSeq = Math.max(lastEventSeq, event.session.last_sequence ?? 0);
    const priorContext = current?.contextUsage ?? null;
    // The stale replay buffer predates whatever forced the reconnect; the
    // snapshot is the newer truth. Reload the durable log right after.
    history = [];
    current = attachExtras(projectSnapshot(event.session, history), event.session);
    current.contextUsage = priorContext;
    liveTrajectory = [];
    liveTurnStartAt = null;
    liveTurnClosed = false;
    render();
    reloadHistory();
    refreshTasks();
    return;
  }
  if (event.type === 'session_updated') {
    if (staleSequence(event.session.last_sequence)) return;
    lastEventSeq = Math.max(lastEventSeq, event.session.last_sequence ?? 0);
    current = attachExtras(applySessionMetadata(current, event.session), event.session);
    renderHeader();
    renderComposerMeta();
    renderSidebar();
    // The status the runtime settles after a turn arrives here, not on the
    // terminal event. The transcript owns the running row and the plan/diff
    // cards, so it has to re-render or a finished turn keeps saying 运行中.
    scheduleConversation();
    return;
  }
  if (event.type === 'token_usage') {
    current = { ...current, usage: { ...(current.usage ?? {}), ...event } };
    renderContextMeter();
    scheduleConversation();
    return;
  }
  if (event.type === 'context_usage') { current = { ...current, contextUsage: event.accounting }; renderContextMeter(); return; }
  if (event.type === 'context_loaded') { current = { ...current, contextUsage: event.accounting ?? current.contextUsage }; renderContextMeter(); return; }
  if (event.type.startsWith('sub_agent_')) {
    current = applySubagentEvent(current, event);
    renderHeader();
    if (rightbarOpen) renderRightbar();
    return;
  }
  if (event.type.startsWith('background_task_') || event.type === 'background_tasks_reconciled') {
    current = applyBackgroundEvent(current, event);
    renderHeader();
    if (jobsPopoverState?.node.isConnected) {
      if (event.type === 'background_task_output') updateJobsOutput(event.task_id);
      else renderJobsInto(jobsPopoverState.node);
    }
    return;
  }
  if (event.type === 'notification') {
    if (event.level === 'warning' || event.level === 'error') toast(event.message, { tone: event.level, holdMs: 5000 });
    return;
  }
  if (event.type === 'agents_loaded') return;
  const pendingDiff = current.diffQuery?.status === 'pending' && event.query_id === current.diffQuery.id;
  const statusBefore = current.status;
  const activityBefore = current.activity;
  current = applyEvent(current, event);
  // The header and composer read status/activity; when an event changes either
  // they must be refreshed here, not only on the next full render.
  if (current.status !== statusBefore || current.activity !== activityBefore) { renderHeader(); renderComposerMeta(); }
  modelRetry = event.type === 'model_retrying' ? { attempt: event.attempt ?? 0, max: event.max_attempts ?? 0 } : null;
  if (terminalEvents.has(event.type)) { pendingSubmit = false; setTimeout(flushQueuedMessage, 0); }
  if (terminalEvents.has(event.type) && current?.session?.repository) {
    const attemptDiff = (attempt = 0) => {
      if (attempt > 6 || !current?.session?.repository) return;
      if (loading || uncertain) { setTimeout(() => attemptDiff(attempt + 1), 400); return; }
      requestDiff();
    };
    setTimeout(() => attemptDiff(), 800);
  }
  if ((event.type === 'tool_call_started' || event.type === 'tool_call_completed') && event.duration_ms !== undefined) {
    const tool = current.tools.find(item => item.id === event.id);
    if (tool) tool.durationMs = event.duration_ms;
  }
  if (['diff_updated', 'diff_failed'].includes(event.type) && current.diffQuery
    && event.query_id === current.diffQuery.id
    && ['confirmed', 'failed'].includes(current.diffQuery.status)) {
    if (pendingDiff && rightbarOpen) {
      const tab = Object.values(rightbarTabs).find(item => item.kind === 'changes');
      if (tab && !tab.selected) tab.selected = current.diff?.files?.[0]?.path ?? null;
    }
    renderRightbar();
    renderConversation();
  } else if (event.type === 'plan_updated') {
    renderRightbar();
    // The composer dock is a plan progress surface; refresh it on the event
    // that changes the plan instead of waiting for an unrelated full render.
    renderComposerDocks();
  }
  controls();
  scheduleConversation();
  if (['plan_updated', 'diff_updated', 'diff_failed'].includes(event.type)
    || (['tool_call_started', 'tool_call_completed'].includes(event.type)
      && current.tools.some(tool => tool.id === event.id && browserActivity(tool)))) {
    renderRightbar();
  }
  if (['approval_requested', 'approval_resolved', 'clarification_requested', 'clarification_resolved'].includes(event.type)) renderInteractions();
  if ($('settings-dialog').open && settingsSection === 'presets') renderSettings();
  if (terminalEvents.has(event.type)) action(async () => { await refreshSnapshot(); await loadHistory(); await refreshTasks(); });
  if (event.type === 'turn_failed') showError(event.failure?.message || event.error || 'Runtime 执行失败');
});

function applySubagentEvent(state, event) {
  const children = [...(state.children ?? [])];
  const index = children.findIndex(child => child.id === event.id);
  const next = { ...(index >= 0 ? children[index] : { id: event.id }) };
  if (event.type === 'sub_agent_updated') Object.assign(next, {
    nickname: event.nickname, role: event.role, title: event.title, done: event.done, ok: event.ok,
    detail: event.detail, background: event.background, scope: event.scope, outcome: event.outcome,
  });
  if (event.type === 'sub_agent_state_changed') next.state = event.state;
  if (event.type === 'sub_agent_progress') Object.assign(next, { input_tokens: event.input_tokens, output_tokens: event.output_tokens, cached_input_tokens: event.cached_input_tokens });
  if (event.type === 'sub_agent_activity') Object.assign(next, { phase: event.phase, tool: event.tool, preview: event.preview, is_error: event.is_error });
  if (index >= 0) children[index] = next; else children.push(next);
  return { ...state, children };
}

function applyBackgroundEvent(state, event) {
  const jobs = [...(state.backgroundTasks ?? [])];
  if (event.type === 'background_tasks_reconciled') {
    // Authoritative replacement after a lifecycle broadcast lag. Keep the local
    // clock anchor and streamed output for rows we already know.
    const prior = new Map(jobs.map(job => [job.task_id, job]));
    const active = (event.tasks ?? []).map(task => {
      const known = prior.get(task.task_id);
      return known
        ? { ...known, program: task.program, args: task.args, pid: task.pid, elapsed_ms: task.elapsed_ms }
        : { ...task, started_at: Date.now() - (task.elapsed_ms ?? 0), output: '' };
    });
    return { ...state, backgroundTasks: active };
  }
  if (event.type === 'background_task_started') {
    jobs.push({ task_id: event.task_id, program: event.program, args: event.args, pid: event.pid, elapsed_ms: 0, started_at: Date.now(), output: '' });
    return { ...state, backgroundTasks: jobs };
  }
  if (event.type === 'background_task_output') {
    const index = jobs.findIndex(job => job.task_id === event.task_id);
    if (index >= 0) jobs[index] = { ...jobs[index], output: `${jobs[index].output ?? ''}${event.chunk ?? ''}`.slice(-65536) };
    return { ...state, backgroundTasks: jobs };
  }
  if (event.type === 'background_task_exited') {
    const exiting = jobs.find(task => task.task_id === event.task_id);
    // A snapshot refresh can drop the active row before this event arrives.
    // The exit event itself is the authority for the terminal fact.
    const record = exiting ?? { task_id: event.task_id, program: '', args: [], started_at: Date.now() };
    const settled = [...(state.settledBackgroundTasks ?? []), {
      ...record,
      status: event.ok === false ? 'failed' : 'completed',
      finished_at: Date.now(),
      output: event.output ?? record.output ?? '',
      exit_code: event.exit_code,
      duration_ms: event.duration_ms,
      stopped: event.stopped === true,
    }].slice(-20);
    pendingJobStops.delete(event.task_id);
    if (jobsArmed === event.task_id) disarmJob();
    return { ...state, backgroundTasks: jobs.filter(task => task.task_id !== event.task_id), settledBackgroundTasks: settled };
  }
  if (event.type === 'background_task_unknown') {
    // The runtime could not establish the task's outcome. That is its own
    // terminal fact: never paint it as running, failed, or stopped.
    const record = jobs.find(task => task.task_id === event.task_id);
    const settled = [...(state.settledBackgroundTasks ?? []), {
      ...(record ?? { task_id: event.task_id, program: '', args: [], started_at: Date.now() }),
      status: 'unknown',
      finished_at: Date.now(),
      output: event.output ?? record?.output ?? '',
      duration_ms: event.duration_ms,
      stopped: false,
    }].slice(-20);
    pendingJobStops.delete(event.task_id);
    if (jobsArmed === event.task_id) disarmJob();
    return { ...state, backgroundTasks: jobs.filter(task => task.task_id !== event.task_id), settledBackgroundTasks: settled };
  }
  return { ...state, backgroundTasks: jobs };
}

function scheduleConversation() {
  if (paintPending) return;
  paintPending = true;
  requestAnimationFrame(() => { paintPending = false; renderConversation(); });
}

/* ------------------------------------------------------------------- boot */

function boot() {
  installTooltips();
  document.querySelector('.brand-mark').append(brandMark({ size: 18 }));
  $('hero-mark').append(brandMark({ size: 25 }));
  $('sidebar-toggle').append(iconNode('panel', { size: 16 }));
  $('leading-toggle').append(iconNode('panel', { size: 16 }));
  $('leading-new').append(iconNode('plus', { size: 16 }));
  $('new-session').querySelector('.new-session-content').append(iconNode('plus', { size: 14 }), el('span', undefined, '新会话'));
  $('nav-plugins').append(iconNode('plugins', { size: 16 }), el('span', undefined, '插件'));
  $('nav-automation').append(iconNode('automation', { size: 16 }), el('span', undefined, '自动化任务'));
  $('session-search').append(iconNode('search', { size: 14 }));
  $('view-options').append(iconNode('sliders', { size: 14 }));
  $('add-workspace').append(iconNode('folderPlus', { size: 14 }));
  $('account-menu').querySelector('.account-icon').append(iconNode('more', { size: 16 }));
  $('floating-right-toggle').append(iconNode('panel', { size: 16 }));
  $('header-right-toggle').append(iconNode('panel', { size: 16 }));
  $('find-bar').querySelector('.find-icon').append(iconNode('search', { size: 14 }));
  $('find-prev').append(iconNode('chevronUp', { size: 14 }));
  $('find-next').append(iconNode('chevronDown', { size: 14 }));
  $('find-close').append(iconNode('close', { size: 14 }));
  $('attachment-add').append(iconNode('plus', { size: 16 }));
  $('settings-close').append(iconNode('close', { size: 16 }));
  $('scroll-bottom').append(iconNode('arrowDown', { size: 16 }));
  $('conversation-scroll').addEventListener('scroll', updateScrollBottom, { passive: true });
  $('scroll-bottom').addEventListener('click', () => $('conversation-scroll').scrollTo({ top: $('conversation-scroll').scrollHeight, behavior: 'smooth' }));
  ensureHeaderActions();
  applyTheme();
  applyContentFont();
  api.appInfo?.().then(info => { $('brand-version').textContent = `v${info.version}`; }).catch(() => {});
  api.connectionStatus?.().then(applyConnection).catch(() => {});
  api.onBrowserState(data => {
    browserData = data;
    for (const tab of Object.values(rightbarTabs)) {
      if (tab.kind !== 'browser' || !tab.browserId) continue;
      const found = data.tabs.find(item => item.id === tab.browserId);
      if (!found) { tab.browserId = null; continue; }
      tab.title = found.url && found.url !== 'about:blank' ? browserChrome(found).title : '浏览器';
    }
    if (rightbarOpen) {
      // Native state can arrive while the address field is focused. Re-render
      // there would destroy the input and the keystrokes in it.
      if (document.activeElement?.closest?.('.browser-address-wrap')) {
        // Keep the field alive, but still publish the native title in the
        // pane strip so the tab does not look stale.
        for (const node of document.querySelectorAll('.pane-tab[data-tab-id]')) {
          const record = rightbarTabs[node.dataset.tabId];
          const title = node.querySelector('.tab-title');
          if (record?.kind === 'browser' && title) title.textContent = record.title;
        }
        scheduleBrowserSurface();
      } else renderRightbar();
    } else scheduleBrowserSurface();
  });
  api.onOpenSettings?.(() => openSettings('general'));
  api.onQuitInspect?.(() => {
    const active = status => ['running', 'waiting_user'].includes(status);
    // `loading` also covers passive refreshes that follow a completed turn;
    // it is not evidence that a task would be interrupted. Only a submit in
    // flight, an uncertain delivery, queued work, uploads or live tasks are.
    const busy = pendingSubmit || uncertain || uploadPending() || queuedMessages.length > 0
      || (current ? active(current.status) : false)
      || tasks.some(task => active(task.status));
    api.quitInspectResult?.(busy);
  });
  window.addEventListener('ui-overlay-change', () => scheduleBrowserSurface());

  $('new-session').addEventListener('click', newSession);
  $('leading-new').addEventListener('click', newSession);
  $('sidebar-toggle').addEventListener('click', toggleSidebar);
  $('leading-toggle').addEventListener('click', toggleSidebar);
  $('nav-plugins').addEventListener('click', () => openPage('plugins'));
  $('nav-automation').addEventListener('click', () => openPage('automation'));
  $('session-search').addEventListener('click', openSidebarSearch);
  $('view-options').addEventListener('click', event => openViewOptions(event.currentTarget));
  $('add-workspace').addEventListener('click', () => openDirectoryPicker());
  $('account-menu').addEventListener('click', event => openAccountMenu(event.currentTarget));
  $('workspace-picker').addEventListener('click', event => openWorkspaceMenu(event.currentTarget));
  $('preset-picker').addEventListener('click', event => openPresetMenu(event.currentTarget));
  $('model-picker').addEventListener('click', event => openModelMenu(event.currentTarget));
  $('permission-picker').addEventListener('click', event => openPermissionMenu(event.currentTarget));
  $('plan-mode').addEventListener('click', () => toast('计划模式切换尚未接入', { tone: 'info', holdMs: 2400 }));
  $('attachment-add').addEventListener('click', () => {
    if (document.querySelector('#command-popover')) closeComposerPopovers();
    else openCommandMenu();
  });
  $('send').addEventListener('click', () => submitComposer(false));
  $('stop').addEventListener('click', () => action(() => deliver({ type: 'cancel_current_turn', session_id: current.session.id })));
  $('header-right-toggle').addEventListener('click', () => (rightbarOpen ? closeRightbar() : openRightbar()));
  $('floating-right-toggle').addEventListener('click', () => (rightbarOpen ? closeRightbar() : openRightbar()));
  $('find-input').addEventListener('input', () => { findSelected = 0; updateFind(true); });
  $('find-input').addEventListener('keydown', event => {
    if (event.key === 'Enter') { event.preventDefault(); moveFind(event.shiftKey ? -1 : 1); }
    if (event.key === 'Escape') { event.preventDefault(); closeFind(); }
  });
  $('find-prev').addEventListener('click', () => moveFind(-1));
  $('find-next').addEventListener('click', () => moveFind(1));
  $('find-close').addEventListener('click', closeFind);
  $('settings-close').addEventListener('click', closeSettings);
  $('settings-dialog').addEventListener('cancel', event => { event.preventDefault(); closeSettings(); });
  $('settings-dialog').addEventListener('keydown', event => { trapTab($('settings-dialog'), event); if (event.key === 'Escape') event.stopPropagation(); });
  $('settings-open-config').addEventListener('click', () => action(async () => {
    await api.openConfig();
    toast('已打开配置文件', { tone: 'success', holdMs: 1800 });
  }));
  $('message').addEventListener('input', () => {
    composerError('');
    resizeComposer();
    controls();
    const value = $('message').value;
    drafts.set(draftKey(), value);
    saveDrafts();
    if (/^\/[a-z-]*$/.test(value.trim())) openSlashMenu(); else $('command-popover')?.remove();
    if (/^\s*@/.test(value)) openAtMenu(); else $('at-popover')?.remove();
  });
  $('message').addEventListener('keydown', event => {
    if (event.isComposing || event.keyCode === 229) return;
    const menu = $('command-popover');
    if (menu) {
      if (event.key === 'Escape') { event.preventDefault(); $('message').value = ''; closeComposerPopovers(); controls(); return; }
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault();
        const buttons = [...menu.querySelectorAll('button:not(:disabled)')];
        if (!buttons.length) return;
        const at = buttons.findIndex(button => button.dataset.commandId === menu.dataset.selected);
        const next = buttons[(at + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length];
        menu.dataset.selected = next.dataset.commandId;
        for (const button of menu.querySelectorAll('button')) button.setAttribute('aria-selected', String(button === next));
        return;
      }
      if (event.key === 'Enter' && !event.shiftKey) {
        event.preventDefault();
        const selected = menu.querySelector(`button[data-command-id="${menu.dataset.selected}"]`);
        if (selected && !selected.disabled) executeCommand(selected.dataset.commandId);
        return;
      }
    }
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      submitComposer(event.metaKey || event.ctrlKey);
    }
  });
  $('composer-card').addEventListener('click', event => {
    if (!event.target.closest('button') && !event.target.closest('input')) $('message').focus();
  });
  document.addEventListener('pointerdown', event => {
    if (!event.target.closest('#command-popover,#at-popover,#message,#composer-card')) closeComposerPopovers();
    if (!event.target.closest('.menu')) closeMenu();
    if (innerWidth < 1024 && document.body.classList.contains('sidebar-open')
      && !event.target.closest('#sidebar,#leading-seat,#sidebar-toggle')) {
      document.body.classList.remove('sidebar-open');
      layout();
    }
  });
  document.addEventListener('keydown', event => {
    const approval = current?.approvals?.[0];
    const approvalKey = approval && document.body.classList.contains('composer-takeover')
      && !document.querySelector('dialog[open], .menu')
      && !event.target.closest('button,a,input,textarea,select,[contenteditable="true"]')
      && !event.metaKey && !event.ctrlKey && !event.altKey && !event.isComposing;
    if (approvalKey && event.key === 'Enter') {
      event.preventDefault(); event.stopImmediatePropagation();
      action(async () => {
        await deliver({ type: 'approval_decision', request_id: approval.id, decision: 'approve_once' });
        await refreshMetadata();
      });
      return;
    }
    if (approvalKey && event.key === 'Escape') {
      event.preventDefault(); event.stopImmediatePropagation();
      action(async () => {
        await deliver({ type: 'approval_decision', request_id: approval.id, decision: 'deny' });
        await refreshMetadata();
      });
      return;
    }
    if (event.key === 'Escape') {
      closeComposerPopovers();
      if (trajectorySelectedId) { trajectorySelectedId = null; renderTrajectory(); }
      if (!$('find-bar').hidden) closeFind();
      if (document.body.classList.contains('sidebar-open')) { document.body.classList.remove('sidebar-open'); layout(); }
    }
    if ((event.metaKey || event.ctrlKey) && event.altKey && event.key.toLowerCase() === 'o' && current?.session.repository) {
      event.preventDefault();
      const apps = openInAppState.apps ?? [];
      const preferred = apps.find(app => app.id === openInAppState.preferred) ?? apps[0];
      if (preferred) openInApp(current.session.repository, preferred.id, { remember: true });
      else toast('本地打开应用尚未就绪', { tone: 'info', holdMs: 2400 });
    }
    if (event.ctrlKey && event.code === 'Backquote') {
      event.preventDefault();
      ensureRightbarSession();
      const existing = Object.values(rightbarTabs).find(tab => tab.kind === 'terminal');
      if (existing) {
        const pane = rightbarPanes.find(item => item.tabs.includes(existing.id));
        if (pane) { pane.activeId = existing.id; rightbarActivePane = pane.id; }
        openRightbar();
      } else openRightbarTab('terminal');
      return;
    }
    if ((event.metaKey || event.ctrlKey) && !event.shiftKey && event.key.toLowerCase() === 'n') { event.preventDefault(); newSession(); }
    if ((event.metaKey || event.ctrlKey) && event.key === '/') { event.preventDefault(); showShortcuts(); }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'w') {
      event.preventDefault();
      if (rightbarOpen) {
        const pane = activePane();
        if (pane.activeId) closeTab(pane, pane.activeId); else closeRightbar();
      } else window.close();
    }
    if ((event.metaKey || event.ctrlKey) && event.key === ',') { event.preventDefault(); openSettings('general'); }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'f' && current) { event.preventDefault(); openFind(); }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 't' && !event.altKey) { event.preventDefault(); openRightbarTab('browser'); }
    if ((event.metaKey || event.ctrlKey) && event.altKey && event.key.toLowerCase() === 'b') { event.preventDefault(); rightbarOpen ? closeRightbar() : openRightbar(); }
    if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === 'b') { event.preventDefault(); toggleSidebar(); }
    if (event.key === 'Escape' && document.body.classList.contains('rightbar-expanded')) { event.preventDefault(); toggleRightbarFullscreen(); }
    if (event.key === 'Escape' && current && ['running', 'waiting_user'].includes(current.status)
      && !document.querySelector('dialog[open], .menu')) {
      const now = Date.now();
      if (now - lastEscapeAt < 450) { lastEscapeAt = 0; $('stop').click(); }
      else lastEscapeAt = now;
    }
    if (event.key === 'Tab' && document.body.classList.contains('rightbar-overlay') && rightbarOpen) trapTab($('rightbar'), event);
  });
  document.addEventListener('dragover', event => {
    if (event.dataTransfer?.types.includes('text/plain') && !event.target.closest?.('#rightbar,#float-layer')) event.preventDefault();
  });
  document.addEventListener('drop', event => {
    if (event.target.closest?.('#rightbar,#float-layer')) return;
    const tabId = event.dataTransfer?.getData('text/plain');
    if (!tabId || !rightbarTabs[tabId]) return;
    const pane = rightbarPanes.find(item => item.tabs.includes(tabId));
    if (!pane) return;
    event.preventDefault();
    floatTab(pane, tabId);
  });
  window.addEventListener('resize', () => { layout(); renderRightbar(); });
  setupRightResize();
  $('settings-dialog').addEventListener('click', event => { if (event.target === $('settings-dialog')) closeSettings(); });
}

function toggleSidebar() {
  if (innerWidth < 1024) document.body.classList.toggle('sidebar-open');
  else {
    wideSidebarCollapsed = !wideSidebarCollapsed;
    try { localStorage.setItem('codeleveler.sidebar.collapsed', String(wideSidebarCollapsed)); } catch { /* optional */ }
  }
  if (document.body.classList.contains('sidebar-open')) rightbarOpen && closeRightbar();
  layout();
  if ($('sidebar').inert && $('sidebar').contains(document.activeElement)) $('leading-toggle').focus();
}

function resizeComposer() {
  const input = $('message');
  input.style.height = 'auto';
  const min = document.body.classList.contains('home') ? 52 : 36;
  input.style.height = `${Math.min(336, Math.max(min, input.scrollHeight))}px`;
}

async function submitComposer(complement = false) {
  const content = $('message').value.trim();
  const intent = slashIntent(content, commandCatalog());
  if (intent.kind === 'action') { executeCommand(intent.id); return; }
  if (intent.kind === 'blocked') { composerError(intent.reason); return; }
  const refs = attachments().filter(item => item.status === 'ready').map(item => item.attachment);
  if (connectionState.state !== 'connected') { composerError('连接已断开，消息未发送；正在重连'); return; }
  if ((!content && !refs.length) || loading || uncertain || uploadPending()) return;
  const unsupported = attachmentSubmissionError(refs);
  if (unsupported) { composerError(unsupported); return; }
  composerError('');
  action(async () => {
    if (!current) await createTask();
    const running = ['running', 'waiting_user'].includes(current.status);
    const steer = running && !!content && (complement ? settings.busySend !== 'steer' : settings.busySend === 'steer');
    if (running && !steer) {
      queuedMessages.push({ id: crypto.randomUUID(), sessionId: current.session.id, content, attachments: refs });
      const sent = new Set(refs.map(ref => ref.id));
      attachmentDrafts.set(draftKey(), attachments().filter(item => !sent.has(item.attachment?.id)));
      renderAttachments();
      if ($('message').value.trim() === content) { $('message').value = ''; clearDraft(); resizeComposer(); }
      renderComposerDocks();
      return;
    }
    if (steer) {
      await deliver({ type: 'steer_current_turn', session_id: current.session.id, content });
      // A steer is queued for the running turn's next model call; no message
      // event acknowledges it immediately. Say so instead of silently
      // clearing the composer as if the text had been submitted as a turn.
      toast('已插话：将在当前回合的下一次模型调用中送达', { tone: 'info', holdMs: 2800 });
    } else {
      await deliver({ type: 'submit_message', session_id: current.session.id, content, attachments: refs });
      pendingSubmit = true;
    }
    if ($('message').value.trim() === content) { $('message').value = ''; clearDraft(); resizeComposer(); }
    const sent = new Set(refs.map(ref => ref.id));
    attachmentDrafts.set(draftKey(), attachments().filter(item => !sent.has(item.attachment?.id)));
    renderAttachments();
    await refreshMetadata();
  }, { onError: composerError, clearGlobal: false });
}

function setupRightResize() {
  const handle = $('right-resize');
  handle.addEventListener('pointerdown', event => {
    event.preventDefault();
    handle.setPointerCapture(event.pointerId);
    document.body.classList.add('resizing-right');
    const startX = event.clientX;
    const startWidth = $('rightbar').getBoundingClientRect().width;
    const move = moveEvent => {
      rightbarWidth = Math.max(300, Math.min(Math.round(innerWidth * 0.7), startWidth + (startX - moveEvent.clientX)));
      layout();
      renderRightbar();
    };
    const up = () => {
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
      handle.removeEventListener('pointercancel', up);
      document.body.classList.remove('resizing-right');
      persistRightbar();
      renderRightbar();
    };
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up, { once: true });
    handle.addEventListener('pointercancel', up, { once: true });
  });
  handle.addEventListener('keydown', event => {
    if (!['ArrowLeft', 'ArrowRight'].includes(event.key)) return;
    event.preventDefault();
    const base = rightbarWidth ?? $('rightbar').getBoundingClientRect().width;
    rightbarWidth = Math.max(300, Math.min(Math.round(innerWidth * 0.7), base + (event.key === 'ArrowLeft' ? 20 : -20)));
    persistRightbar();
    layout();
    renderRightbar();
  });
}

try {
  wideSidebarCollapsed = localStorage.getItem('codeleveler.sidebar.collapsed') === 'true';
} catch { /* optional */ }

boot();
loadRightbar('new-session');
render();
action(async () => { await refreshTasks(); render(); });
